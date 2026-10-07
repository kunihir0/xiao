use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::{
    Error, Notification, PersistentIdWindow, Result, RetryPolicy,
    transport::{ReceiverConnector, SessionEvent},
};

pub const DEFAULT_QUEUE_CAPACITY: usize = 64;
pub const DEFAULT_DEDUPE_CAPACITY: usize = 256;
pub const MAX_QUEUE_CAPACITY: usize = 4096;
/// Source capacity is bounded by the byte budget for the largest notification.
pub const MAX_SOURCE_CAPACITY: usize = MAX_QUEUE_BYTES / MAX_NOTIFICATION_BYTES;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    Starting,
    Ready,
    Stopped,
}

pub struct ReceiverHandle {
    notifications: mpsc::Receiver<Notification>,
    shutdown: Option<oneshot::Sender<()>>,
    readiness: watch::Receiver<Readiness>,
    readiness_tx: watch::Sender<Readiness>,
    task: Option<JoinHandle<Result<()>>>,
    queued_bytes: Arc<AtomicUsize>,
}
impl fmt::Debug for ReceiverHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceiverHandle")
            .field("ready", &*self.readiness.borrow())
            .finish_non_exhaustive()
    }
}
impl ReceiverHandle {
    #[must_use]
    pub fn readiness(&self) -> Readiness {
        *self.readiness.borrow()
    }
    #[must_use]
    pub fn readiness_watch(&self) -> watch::Receiver<Readiness> {
        self.readiness.clone()
    }
    pub async fn recv(&mut self) -> Option<Notification> {
        let notification = self.notifications.recv().await;
        if let Some(value) = &notification {
            self.queued_bytes
                .fetch_sub(notification_bytes(value), Ordering::AcqRel);
        }
        notification
    }
    pub async fn shutdown(mut self) -> Result<()> {
        if let Some(sender) = self.shutdown.take() {
            let _ = sender.send(());
        }
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        task.await.map_err(|error| Error::Task(error.to_string()))?
    }
}
/// Dropping is a synchronous safety fallback: it publishes terminal readiness
/// and aborts the owned task. Call [`ReceiverHandle::shutdown`] when the
/// caller needs bounded asynchronous cancellation and surfaced task errors.
impl Drop for ReceiverHandle {
    fn drop(&mut self) {
        let _ = self.readiness_tx.send(Readiness::Stopped);
        if let Some(sender) = self.shutdown.take() {
            let _ = sender.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Clone, Debug)]
pub struct ReceiverSourceSender {
    input: mpsc::Sender<Result<Notification>>,
    queued_bytes: Arc<AtomicUsize>,
}

impl ReceiverSourceSender {
    /// Validates and queues a source item. The byte reservation occurs only
    /// after a channel permit is available, so canceled sends cannot leak
    /// source-budget accounting.
    pub async fn send(&self, item: Result<Notification>) -> Result<()> {
        let bytes = item.as_ref().map_or(0, notification_bytes);
        if bytes > MAX_NOTIFICATION_BYTES {
            return Err(Error::NotificationTooLarge);
        }
        let permit = self
            .input
            .reserve()
            .await
            .map_err(|_| Error::ChannelClosed)?;
        if bytes > 0 && !reserve_queue_bytes(&self.queued_bytes, bytes) {
            return Err(Error::QueueByteBudget);
        }
        permit.send(item);
        Ok(())
    }
}

pub struct ReceiverSource {
    input: mpsc::Receiver<Result<Notification>>,
    queued_bytes: Arc<AtomicUsize>,
}
impl ReceiverSource {
    pub fn channel(capacity: usize) -> Result<(ReceiverSourceSender, Self)> {
        if capacity == 0 {
            return Err(Error::InvalidRegistration(
                "receiver source capacity must be non-zero".into(),
            ));
        }
        let capacity = capacity.min(MAX_SOURCE_CAPACITY);
        let (input_sender, input) = mpsc::channel(capacity);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        Ok((
            ReceiverSourceSender {
                input: input_sender,
                queued_bytes: Arc::clone(&queued_bytes),
            },
            Self {
                input,
                queued_bytes,
            },
        ))
    }
}

#[must_use]
pub fn start(
    source: ReceiverSource,
    queue_capacity: usize,
    dedupe_capacity: usize,
) -> ReceiverHandle {
    start_with_policy(
        source,
        queue_capacity,
        dedupe_capacity,
        RetryPolicy::default(),
    )
}

#[must_use]
pub fn start_with_policy(
    source: ReceiverSource,
    queue_capacity: usize,
    dedupe_capacity: usize,
    policy: RetryPolicy,
) -> ReceiverHandle {
    let (notifications_tx, notifications) =
        mpsc::channel(queue_capacity.clamp(1, MAX_QUEUE_CAPACITY));
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let (readiness_tx, readiness) = watch::channel(Readiness::Starting);
    let queued_bytes = Arc::new(AtomicUsize::new(0));
    let task_queued_bytes = Arc::clone(&queued_bytes);
    let task_readiness = readiness_tx.clone();
    let task = tokio::spawn(async move {
        let ReceiverSource {
            input: source_input,
            queued_bytes: source_queued_bytes,
        } = source;
        let mut source = source_input;
        let mut dedupe = PersistentIdWindow::new(dedupe_capacity);
        let mut attempts: u32 = 0;
        task_readiness
            .send(Readiness::Ready)
            .map_err(|_| Error::ChannelClosed)?;
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    let _ = task_readiness.send(Readiness::Stopped);
                    return Ok(());
                }
                item = source.recv() => {
                    let Some(item) = item else {
                        let _ = task_readiness.send(Readiness::Stopped);
                        return Ok(());
                    };
                    if let Ok(notification) = &item {
                        source_queued_bytes.fetch_sub(notification_bytes(notification), Ordering::AcqRel);
                    }
                    let notification = match item {
                        Ok(notification) => {
                            attempts = 0;
                            notification
                        }
                        Err(error) => {
                            attempts = attempts.saturating_add(1);
                            if attempts > policy.max_attempts() {
                                let _ = task_readiness.send(Readiness::Stopped);
                                return Err(error);
                            }
                            let _ = task_readiness.send(Readiness::Starting);
                            tokio::select! {
                                _ = &mut shutdown_rx => {
                                    let _ = task_readiness.send(Readiness::Stopped);
                                    return Ok(());
                                }
                                _ = tokio::time::sleep(policy.random_delay(attempts)) => {}
                            }
                            task_readiness.send(Readiness::Ready).map_err(|_| Error::ChannelClosed)?;
                            continue;
                        }
                    };
                    if notification.persistent_id.as_deref().is_some_and(|id| dedupe.observe(id)) {
                        continue;
                    }
                    let bytes = notification_bytes(&notification);
                    if bytes > MAX_NOTIFICATION_BYTES
                        || !reserve_queue_bytes(&task_queued_bytes, bytes)
                    {
                        let _ = task_readiness.send(Readiness::Stopped);
                        return Err(if bytes > MAX_NOTIFICATION_BYTES {
                            Error::NotificationTooLarge
                        } else {
                            Error::QueueByteBudget
                        });
                    }
                    let send_result = tokio::select! {
                        _ = &mut shutdown_rx => {
                            task_queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
                            let _ = task_readiness.send(Readiness::Stopped);
                            return Ok(());
                        }
                        result = notifications_tx.send(notification) => result,
                    };
                    if send_result.is_err() {
                        task_queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
                        let _ = task_readiness.send(Readiness::Stopped);
                        return Err(Error::ChannelClosed);
                    }
                }
            }
        }
    });
    ReceiverHandle {
        notifications,
        shutdown: Some(shutdown_tx),
        readiness,
        readiness_tx,
        task: Some(task),
        queued_bytes,
    }
}

/// Starts the production check-in/registration/MCS reconnect loop.
#[must_use]
pub fn start_connected(
    connector: Arc<dyn ReceiverConnector>,
    queue_capacity: usize,
    dedupe_capacity: usize,
    policy: RetryPolicy,
) -> ReceiverHandle {
    let (notifications_tx, notifications) =
        mpsc::channel(queue_capacity.clamp(1, MAX_QUEUE_CAPACITY));
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let (readiness_tx, readiness) = watch::channel(Readiness::Starting);
    let queued_bytes = Arc::new(AtomicUsize::new(0));
    let task_queued_bytes = Arc::clone(&queued_bytes);
    let task_readiness = readiness_tx.clone();
    let task = tokio::spawn(async move {
        let mut dedupe = PersistentIdWindow::new(dedupe_capacity);
        let mut attempts = 0_u32;
        loop {
            let ids = dedupe.ids();
            let connection = connector.connect(&ids);
            tokio::pin!(connection);
            let mut session = tokio::select! {
                _ = &mut shutdown_rx => {
                    let _ = task_readiness.send(Readiness::Stopped);
                    return Ok(());
                }
                result = &mut connection => {
                    match result {
                        Ok(session) => session,
                        Err(error) => {
                            attempts = attempts.saturating_add(1);
                            if attempts > policy.max_attempts() {
                                let _ = task_readiness.send(Readiness::Stopped);
                                return Err(error);
                            }
                            let _ = task_readiness.send(Readiness::Starting);
                            let delay = policy.random_delay(attempts);
                            tokio::select! {
                                _ = &mut shutdown_rx => {
                                    let _ = task_readiness.send(Readiness::Stopped);
                                    return Ok(());
                                }
                                _ = tokio::time::sleep(delay) => {}
                            }
                            continue;
                        }
                    }
                }
            };
            // A connection that completed login is a successful session. Any
            // later reconnect budget counts only failures in this session.
            attempts = 0;
            let _ = task_readiness.send(Readiness::Ready);
            let mut heartbeat = tokio::time::interval(session.heartbeat_interval());
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => {
                        let _ = task_readiness.send(Readiness::Stopped);
                        return Ok(());
                    }
                    _ = heartbeat.tick() => {
                        if let Err(error) = session.send_heartbeat().await {
                            attempts = attempts.saturating_add(1);
                            if attempts > policy.max_attempts() {
                                let _ = task_readiness.send(Readiness::Stopped);
                                return Err(error);
                            }
                            let _ = task_readiness.send(Readiness::Starting);
                            break;
                        }
                    }
                    event = session.receive() => {
                        match event {
                            Ok(SessionEvent::Notification(notification)) => {
                                let bytes = notification_bytes(&notification);
                                if bytes > MAX_NOTIFICATION_BYTES || !reserve_queue_bytes(&task_queued_bytes, bytes) {
                                    let _ = task_readiness.send(Readiness::Stopped);
                                    return Err(if bytes > MAX_NOTIFICATION_BYTES {
                                        Error::NotificationTooLarge
                                    } else {
                                        Error::QueueByteBudget
                                    });
                                }
                                if notification.persistent_id.as_deref().is_some_and(|id| dedupe.observe(id)) {
                                    task_queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
                                    continue;
                                }
                                let send_result = tokio::select! {
                                    _ = &mut shutdown_rx => {
                                        task_queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
                                        let _ = task_readiness.send(Readiness::Stopped);
                                        return Ok(());
                                    }
                                    result = notifications_tx.send(notification) => result,
                                };
                                if send_result.is_err() {
                                    task_queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
                                    let _ = task_readiness.send(Readiness::Stopped);
                                    return Err(Error::ChannelClosed);
                                }
                            }
                            Ok(SessionEvent::Closed) => {
                                // A clean session close follows a successful
                                // session, so reconnect failures start fresh.
                                attempts = 0;
                                let _ = task_readiness.send(Readiness::Starting);
                                break;
                            }
                            Err(error) => {
                                attempts = attempts.saturating_add(1);
                                if attempts > policy.max_attempts() {
                                    let _ = task_readiness.send(Readiness::Stopped);
                                    return Err(error);
                                }
                                let _ = task_readiness.send(Readiness::Starting);
                                break;
                            }
                        }
                    }
                }
            }
            let delay = policy.random_delay(attempts);
            tokio::select! {
                _ = &mut shutdown_rx => {
                    let _ = task_readiness.send(Readiness::Stopped);
                    return Ok(());
                }
                _ = tokio::time::sleep(delay) => {}
            }
        }
    });
    ReceiverHandle {
        notifications,
        shutdown: Some(shutdown_tx),
        readiness,
        readiness_tx,
        task: Some(task),
        queued_bytes,
    }
}

pub const MAX_NOTIFICATION_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_QUEUE_BYTES: usize = 16 * 1024 * 1024;

fn reserve_queue_bytes(current: &AtomicUsize, bytes: usize) -> bool {
    let mut previous = current.load(Ordering::Acquire);
    loop {
        let Some(next) = previous.checked_add(bytes) else {
            return false;
        };
        if next > MAX_QUEUE_BYTES {
            return false;
        }
        match current.compare_exchange_weak(previous, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(value) => previous = value,
        }
    }
}
fn notification_bytes(notification: &Notification) -> usize {
    notification.payload.len()
        + notification.persistent_id.as_ref().map_or(0, String::len)
        + notification
            .app_data
            .iter()
            .map(|entry| entry.key.len() + entry.value.len())
            .sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn shutdown_is_joined_and_readiness_is_truthful() {
        let source_result = ReceiverSource::channel(2);
        assert!(source_result.is_ok());
        let Some((sender, source)) = source_result.ok() else {
            return;
        };
        let handle = start(source, 2, 2);
        assert_eq!(handle.readiness(), Readiness::Starting);
        tokio::task::yield_now().await;
        assert_eq!(handle.readiness(), Readiness::Ready);
        drop(sender);
        assert!(handle.shutdown().await.is_ok());
    }
    #[tokio::test]
    async fn duplicate_ids_are_not_forwarded() {
        let source_result = ReceiverSource::channel(2);
        assert!(source_result.is_ok());
        let Some((sender, source)) = source_result.ok() else {
            return;
        };
        let mut handle = start(source, 2, 2);
        tokio::task::yield_now().await;
        let n = Notification::new(vec![1], Some("same".into()));
        assert!(sender.send(Ok(n.clone())).await.is_ok());
        assert!(sender.send(Ok(n)).await.is_ok());
        assert!(handle.recv().await.is_some());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(5), handle.recv())
                .await
                .is_err()
        );
        assert!(handle.shutdown().await.is_ok());
    }

    #[tokio::test]
    async fn shutdown_cancels_send_when_output_queue_is_full() {
        let source_result = ReceiverSource::channel(2);
        assert!(source_result.is_ok());
        let Some((sender, source)) = source_result.ok() else {
            return;
        };
        let handle = start(source, 1, 2);
        tokio::task::yield_now().await;
        assert!(
            sender
                .send(Ok(Notification::new(vec![1], None)))
                .await
                .is_ok()
        );
        assert!(
            sender
                .send(Ok(Notification::new(vec![2], None)))
                .await
                .is_ok()
        );
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(1), handle.shutdown()).await;
        assert!(matches!(result, Ok(Ok(()))));
    }

    #[tokio::test]
    async fn source_sender_rejects_oversized_notifications_before_queueing() {
        let source_result = ReceiverSource::channel(1);
        assert!(source_result.is_ok());
        let Some((sender, _source)) = source_result.ok() else {
            return;
        };
        let result = sender
            .send(Ok(Notification::new(
                vec![0; MAX_NOTIFICATION_BYTES + 1],
                None,
            )))
            .await;
        assert!(matches!(result, Err(Error::NotificationTooLarge)));
    }

    struct ClosingSession;
    impl crate::transport::McsSession for ClosingSession {
        fn receive<'a>(&'a mut self) -> crate::transport::BoxFuture<'a, Result<SessionEvent>> {
            Box::pin(async { Ok(SessionEvent::Closed) })
        }

        fn send_heartbeat<'a>(&'a mut self) -> crate::transport::BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    struct ResetProbeConnector {
        calls: Arc<AtomicUsize>,
    }
    impl ReceiverConnector for ResetProbeConnector {
        fn connect<'a>(
            &'a self,
            _persistent_ids: &'a [String],
        ) -> crate::transport::BoxFuture<'a, Result<Box<dyn crate::transport::McsSession>>>
        {
            Box::pin(async move {
                let call = self.calls.fetch_add(1, Ordering::AcqRel);
                if call == 0 {
                    Ok(Box::new(ClosingSession) as Box<dyn crate::transport::McsSession>)
                } else {
                    Err(Error::TransportClosed)
                }
            })
        }
    }

    #[tokio::test]
    async fn reconnect_budget_resets_after_a_successful_session() {
        let calls = Arc::new(AtomicUsize::new(0));
        let connector = Arc::new(ResetProbeConnector {
            calls: Arc::clone(&calls),
        });
        let policy = RetryPolicy::new(std::time::Duration::ZERO, std::time::Duration::ZERO, 0)
            .with_max_attempts(1);
        let handle = start_connected(connector, 1, 1, policy);
        for _ in 0..100 {
            if calls.load(Ordering::Acquire) >= 3 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(calls.load(Ordering::Acquire), 3);
        let result = handle.shutdown().await;
        assert!(matches!(result, Err(Error::TransportClosed)));
    }
    #[test]
    fn rejects_zero_source_capacity() {
        assert!(ReceiverSource::channel(0).is_err());
    }
}
