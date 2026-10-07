use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::{
    Error, Notification, PersistentIdWindow, Result, RetryPolicy,
    transport::{ReceiverConnector, SessionEvent},
};

pub const DEFAULT_QUEUE_CAPACITY: usize = 64;
pub const DEFAULT_DEDUPE_CAPACITY: usize = 256;
pub const MAX_QUEUE_CAPACITY: usize = 4096;
const HEARTBEAT_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
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
    queue_notify: Arc<Notify>,
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
            self.queue_notify.notify_waiters();
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
    let queue_notify = Arc::new(Notify::new());
    let task_queued_bytes = Arc::clone(&queued_bytes);
    let task_queue_notify = Arc::clone(&queue_notify);
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
                    if bytes > MAX_NOTIFICATION_BYTES {
                        continue;
                    }
                    loop {
                        let notified = task_queue_notify.notified();
                        if reserve_queue_bytes(&task_queued_bytes, bytes) {
                            break;
                        }
                        tokio::select! {
                            _ = &mut shutdown_rx => {
                                let _ = task_readiness.send(Readiness::Stopped);
                                return Ok(());
                            }
                            _ = notified => {}
                        }
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
                        task_queue_notify.notify_waiters();
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
        queue_notify,
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
    let queue_notify = Arc::new(Notify::new());
    let task_queued_bytes = Arc::clone(&queued_bytes);
    let task_queue_notify = Arc::clone(&queue_notify);
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
            // Login success alone does not reset the reconnect budget. A
            // session must process an inbound event before it is healthy.
            let mut processed_event = false;
            let _ = task_readiness.send(Readiness::Ready);
            let heartbeat_interval = session
                .heartbeat_interval()
                .max(std::time::Duration::from_millis(1));
            loop {
                let event = tokio::select! {
                    _ = &mut shutdown_rx => {
                        let _ = task_readiness.send(Readiness::Stopped);
                        return Ok(());
                    }
                    result = tokio::time::timeout(heartbeat_interval, session.receive()) => {
                        match result {
                            Ok(Ok(event)) => event,
                            Ok(Err(error)) => {
                                if let Some(error) =
                                    session_failure(error, &mut attempts, policy, &task_readiness)
                                {
                                    return Err(error);
                                }
                                break;
                            }
                            Err(_) => {
                                let heartbeat_result = tokio::select! {
                                    _ = &mut shutdown_rx => {
                                        let _ = task_readiness.send(Readiness::Stopped);
                                        return Ok(());
                                    }
                                    result = session.send_heartbeat() => result,
                                };
                                if let Err(error) = heartbeat_result {
                                    if let Some(error) =
                                        session_failure(error, &mut attempts, policy, &task_readiness)
                                    {
                                        return Err(error);
                                    }
                                    break;
                                }
                                match tokio::time::timeout(HEARTBEAT_GRACE, async {
                                    tokio::select! {
                                        _ = &mut shutdown_rx => None,
                                        result = session.receive() => Some(result),
                                    }
                                }).await {
                                    Ok(Some(Ok(event))) => event,
                                    Ok(Some(Err(error))) => {
                                        if let Some(error) = session_failure(
                                            error,
                                            &mut attempts,
                                            policy,
                                            &task_readiness,
                                        ) {
                                            return Err(error);
                                        }
                                        break;
                                    }
                                    Ok(None) => {
                                        let _ = task_readiness.send(Readiness::Stopped);
                                        return Ok(());
                                    }
                                    Err(_) => {
                                        attempts = attempts.saturating_add(1);
                                        if attempts > policy.max_attempts() {
                                            let _ = task_readiness.send(Readiness::Stopped);
                                            return Err(Error::LivenessTimeout);
                                        }
                                        let _ = task_readiness.send(Readiness::Starting);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                };
                match event {
                    SessionEvent::Notification(notification) => {
                        if !processed_event {
                            attempts = 0;
                            processed_event = true;
                        }
                        let bytes = notification_bytes(&notification);
                        if bytes > MAX_NOTIFICATION_BYTES {
                            let ack_result = tokio::select! {
                                _ = &mut shutdown_rx => {
                                    let _ = task_readiness.send(Readiness::Stopped);
                                    return Ok(());
                                }
                                result = session.force_acknowledge() => result,
                            };
                            if let Err(error) = ack_result {
                                if let Some(error) =
                                    session_failure(error, &mut attempts, policy, &task_readiness)
                                {
                                    return Err(error);
                                }
                                break;
                            }
                            continue;
                        }
                        if notification
                            .persistent_id
                            .as_deref()
                            .is_some_and(|id| dedupe.contains(id))
                        {
                            let ack_result = tokio::select! {
                                _ = &mut shutdown_rx => {
                                    let _ = task_readiness.send(Readiness::Stopped);
                                    return Ok(());
                                }
                                result = session.force_acknowledge() => result,
                            };
                            if let Err(error) = ack_result {
                                if let Some(error) =
                                    session_failure(error, &mut attempts, policy, &task_readiness)
                                {
                                    return Err(error);
                                }
                                break;
                            }
                            continue;
                        }
                        let permit = tokio::select! {
                            _ = &mut shutdown_rx => {
                                let _ = task_readiness.send(Readiness::Stopped);
                                return Ok(());
                            }
                            result = notifications_tx.reserve() => result,
                        };
                        let Ok(permit) = permit else {
                            let _ = task_readiness.send(Readiness::Stopped);
                            return Err(Error::ChannelClosed);
                        };
                        loop {
                            let notified = task_queue_notify.notified();
                            if reserve_queue_bytes(&task_queued_bytes, bytes) {
                                break;
                            }
                            tokio::select! {
                                _ = &mut shutdown_rx => {
                                    let _ = task_readiness.send(Readiness::Stopped);
                                    return Ok(());
                                }
                                _ = notified => {}
                            }
                        }
                        if let Some(id) = notification.persistent_id.as_deref() {
                            dedupe.observe(id);
                        }
                        permit.send(notification);
                        // Admission succeeded; if shutdown wins here the
                        // unacknowledged persistent ID is redelivered and
                        // deduplicated by the window on the next session.
                        let ack_result = tokio::select! {
                            _ = &mut shutdown_rx => {
                                let _ = task_readiness.send(Readiness::Stopped);
                                return Ok(());
                            }
                            result = session.acknowledge() => result,
                        };
                        if let Err(error) = ack_result {
                            if let Some(error) =
                                session_failure(error, &mut attempts, policy, &task_readiness)
                            {
                                return Err(error);
                            }
                            break;
                        }
                    }
                    SessionEvent::Heartbeat => {
                        if !processed_event {
                            attempts = 0;
                            processed_event = true;
                        }
                    }
                    SessionEvent::Closed => {
                        // A clean session close follows a successfully
                        // processed event, so reconnect failures start fresh.
                        if processed_event {
                            attempts = 0;
                        }
                        let _ = task_readiness.send(Readiness::Starting);
                        break;
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
        queue_notify,
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

fn session_failure(
    error: Error,
    attempts: &mut u32,
    policy: RetryPolicy,
    readiness: &watch::Sender<Readiness>,
) -> Option<Error> {
    *attempts = attempts.saturating_add(1);
    if *attempts > policy.max_attempts() {
        let _ = readiness.send(Readiness::Stopped);
        Some(error)
    } else {
        let _ = readiness.send(Readiness::Starting);
        None
    }
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

    struct AckSession {
        pending: Vec<Notification>,
        acknowledgements: Arc<AtomicUsize>,
    }
    impl crate::transport::McsSession for AckSession {
        fn receive<'a>(&'a mut self) -> crate::transport::BoxFuture<'a, Result<SessionEvent>> {
            Box::pin(async move {
                if let Some(notification) = self.pending.pop() {
                    Ok(SessionEvent::Notification(notification))
                } else {
                    std::future::pending().await
                }
            })
        }

        fn acknowledge<'a>(&'a mut self) -> crate::transport::BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.acknowledgements.fetch_add(1, Ordering::AcqRel);
                Ok(())
            })
        }

        fn send_heartbeat<'a>(&'a mut self) -> crate::transport::BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }

        fn heartbeat_interval(&self) -> std::time::Duration {
            std::time::Duration::from_secs(3600)
        }
    }

    struct AckConnector {
        notifications: Vec<Notification>,
        acknowledgements: Arc<AtomicUsize>,
    }
    impl ReceiverConnector for AckConnector {
        fn connect<'a>(
            &'a self,
            _persistent_ids: &'a [String],
        ) -> crate::transport::BoxFuture<'a, Result<Box<dyn crate::transport::McsSession>>>
        {
            Box::pin(async move {
                Ok(Box::new(AckSession {
                    pending: self.notifications.clone(),
                    acknowledgements: Arc::clone(&self.acknowledgements),
                }) as Box<dyn crate::transport::McsSession>)
            })
        }
    }

    fn no_retry_policy() -> RetryPolicy {
        RetryPolicy::new(std::time::Duration::ZERO, std::time::Duration::ZERO, 0)
            .with_max_attempts(1)
    }

    #[tokio::test]
    async fn acknowledges_after_successful_queue_admission() {
        let acknowledgements = Arc::new(AtomicUsize::new(0));
        let connector = Arc::new(AckConnector {
            notifications: vec![Notification::new(vec![1], Some("one".into()))],
            acknowledgements: Arc::clone(&acknowledgements),
        });
        let mut handle = start_connected(connector, 1, 2, no_retry_policy());
        let received = tokio::time::timeout(std::time::Duration::from_secs(1), handle.recv()).await;
        assert!(received.is_ok());
        let acknowledged = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while acknowledgements.load(Ordering::Acquire) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(acknowledged.is_ok());
        assert_eq!(acknowledgements.load(Ordering::Acquire), 1);
        assert!(handle.shutdown().await.is_ok());
    }

    #[tokio::test]
    async fn full_queue_shutdown_does_not_ack_unadmitted_message() {
        let acknowledgements = Arc::new(AtomicUsize::new(0));
        let connector = Arc::new(AckConnector {
            notifications: vec![
                Notification::new(vec![1], Some("one".into())),
                Notification::new(vec![2], Some("two".into())),
            ],
            acknowledgements: Arc::clone(&acknowledgements),
        });
        let handle = start_connected(connector, 1, 2, no_retry_policy());
        let acknowledged = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while acknowledgements.load(Ordering::Acquire) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(acknowledged.is_ok());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(handle.shutdown().await.is_ok());
        assert_eq!(acknowledgements.load(Ordering::Acquire), 1);
    }

    struct ResetProbeConnector {
        calls: Arc<AtomicUsize>,
        events: mpsc::UnboundedSender<usize>,
    }
    impl ReceiverConnector for ResetProbeConnector {
        fn connect<'a>(
            &'a self,
            _persistent_ids: &'a [String],
        ) -> crate::transport::BoxFuture<'a, Result<Box<dyn crate::transport::McsSession>>>
        {
            Box::pin(async move {
                let call = self.calls.fetch_add(1, Ordering::AcqRel);
                let _ = self.events.send(call);
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
        let (events, mut events_rx) = mpsc::unbounded_channel();
        let connector = Arc::new(ResetProbeConnector {
            calls: Arc::clone(&calls),
            events,
        });
        let policy = RetryPolicy::new(std::time::Duration::ZERO, std::time::Duration::ZERO, 0)
            .with_max_attempts(1);
        let handle = start_connected(connector, 1, 1, policy);
        for expected in 0..3 {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(5), events_rx.recv()).await;
            assert!(matches!(event, Ok(Some(value)) if value == expected));
        }
        assert_eq!(calls.load(Ordering::Acquire), 3);
        let result = handle.shutdown().await;
        assert!(matches!(result, Err(Error::TransportClosed)));
    }
    use std::{
        collections::VecDeque,
        sync::Mutex,
        time::Duration,
    };

    struct SessionConfig {
        events: VecDeque<Result<SessionEvent>>,
        heartbeat_interval: Duration,
        heartbeat_notify: Option<Arc<Notify>>,
        heartbeat_count: Option<Arc<AtomicUsize>>,
        acknowledgements: Option<Arc<AtomicUsize>>,
        force_acknowledgements: Option<Arc<AtomicUsize>>,
        ack_started: Option<Arc<Notify>>,
        stall_ack: bool,
    }

    impl SessionConfig {
        fn new(events: Vec<Result<SessionEvent>>) -> Self {
            Self {
                events: events.into(),
                heartbeat_interval: Duration::from_secs(3600),
                heartbeat_notify: None,
                heartbeat_count: None,
                acknowledgements: None,
                force_acknowledgements: None,
                ack_started: None,
                stall_ack: false,
            }
        }
    }

    struct ScriptSession {
        events: VecDeque<Result<SessionEvent>>,
        heartbeat_interval: Duration,
        heartbeat_notify: Option<Arc<Notify>>,
        heartbeat_count: Option<Arc<AtomicUsize>>,
        acknowledgements: Option<Arc<AtomicUsize>>,
        force_acknowledgements: Option<Arc<AtomicUsize>>,
        ack_started: Option<Arc<Notify>>,
        stall_ack: bool,
    }

    impl From<SessionConfig> for ScriptSession {
        fn from(config: SessionConfig) -> Self {
            Self {
                events: config.events,
                heartbeat_interval: config.heartbeat_interval,
                heartbeat_notify: config.heartbeat_notify,
                heartbeat_count: config.heartbeat_count,
                acknowledgements: config.acknowledgements,
                force_acknowledgements: config.force_acknowledgements,
                ack_started: config.ack_started,
                stall_ack: config.stall_ack,
            }
        }
    }

    impl crate::transport::McsSession for ScriptSession {
        fn receive<'a>(&'a mut self) -> crate::transport::BoxFuture<'a, Result<SessionEvent>> {
            if let Some(event) = self.events.pop_front() {
                return Box::pin(async move { event });
            }
            if let Some(notify) = &self.heartbeat_notify {
                let notify = Arc::clone(notify);
                return Box::pin(async move {
                    notify.notified().await;
                    Ok(SessionEvent::Heartbeat)
                });
            }
            Box::pin(std::future::pending())
        }

        fn acknowledge<'a>(&'a mut self) -> crate::transport::BoxFuture<'a, Result<()>> {
            let acknowledgements = self.acknowledgements.clone();
            let ack_started = self.ack_started.clone();
            let stall = self.stall_ack;
            Box::pin(async move {
                if let Some(notify) = ack_started {
                    notify.notify_one();
                }
                if stall {
                    std::future::pending().await
                } else {
                    if let Some(counter) = acknowledgements {
                        counter.fetch_add(1, Ordering::AcqRel);
                    }
                    Ok(())
                }
            })
        }

        fn force_acknowledge<'a>(&'a mut self) -> crate::transport::BoxFuture<'a, Result<()>> {
            let acknowledgements = self.force_acknowledgements.clone();
            Box::pin(async move {
                if let Some(counter) = acknowledgements {
                    counter.fetch_add(1, Ordering::AcqRel);
                }
                Ok(())
            })
        }

        fn send_heartbeat<'a>(&'a mut self) -> crate::transport::BoxFuture<'a, Result<()>> {
            let notify = self.heartbeat_notify.clone();
            let heartbeat_count = self.heartbeat_count.clone();
            Box::pin(async move {
                if let Some(counter) = heartbeat_count {
                    counter.fetch_add(1, Ordering::AcqRel);
                }
                if let Some(notify) = notify {
                    notify.notify_one();
                }
                Ok(())
            })
        }

        fn heartbeat_interval(&self) -> Duration {
            self.heartbeat_interval
        }
    }


    enum ConnectPlan {
        Session(SessionConfig),
    }

    struct ScriptConnector {
        plans: Mutex<VecDeque<ConnectPlan>>,
        calls: Arc<AtomicUsize>,
        call_notify: Arc<Notify>,
    }

    impl ScriptConnector {
        fn new(plans: Vec<ConnectPlan>) -> (Self, Arc<AtomicUsize>, Arc<Notify>) {
            let calls = Arc::new(AtomicUsize::new(0));
            let call_notify = Arc::new(Notify::new());
            (
                Self {
                    plans: Mutex::new(plans.into()),
                    calls: Arc::clone(&calls),
                    call_notify: Arc::clone(&call_notify),
                },
                calls,
                call_notify,
            )
        }
    }

    impl ReceiverConnector for ScriptConnector {
        fn connect<'a>(
            &'a self,
            _persistent_ids: &'a [String],
        ) -> crate::transport::BoxFuture<'a, Result<Box<dyn crate::transport::McsSession>>>
        {
            self.calls.fetch_add(1, Ordering::AcqRel);
            self.call_notify.notify_waiters();
            let plan = self.plans.lock().ok().and_then(|mut plans| plans.pop_front());
            Box::pin(async move {
                match plan {
                    Some(ConnectPlan::Session(config)) => {
                        Ok(Box::new(ScriptSession::from(config))
                            as Box<dyn crate::transport::McsSession>)
                    }
                    None => Err(Error::TransportClosed),
                }
            })
        }
    }

    async fn wait_for_count(counter: &AtomicUsize, notify: &Notify, expected: usize) {
        loop {
            if counter.load(Ordering::Acquire) >= expected {
                return;
            }
            let notified = notify.notified();
            if counter.load(Ordering::Acquire) >= expected {
                return;
            }
            notified.await;
        }
    }

    async fn wait_for_count_without_notify(counter: &AtomicUsize, expected: usize) {
        for _ in 0..1000 {
            if counter.load(Ordering::Acquire) >= expected {
                return;
            }
            tokio::task::yield_now().await;
        }
        assert!(counter.load(Ordering::Acquire) >= expected);
    }

    fn notification_events(count: usize) -> Vec<Result<SessionEvent>> {
        (0..count)
            .map(|_| {
                Ok(SessionEvent::Notification(Notification::new(
                    vec![0; MAX_NOTIFICATION_BYTES],
                    None,
                )))
            })
            .collect()
    }

    fn no_jitter_policy(max_attempts: u32) -> RetryPolicy {
        RetryPolicy::new(Duration::from_millis(100), Duration::from_secs(1), 0)
            .with_max_attempts(max_attempts)
    }

    #[tokio::test(start_paused = true)]
    async fn receive_error_reconnects_and_next_session_delivers() {
        let first = SessionConfig::new(vec![Err(Error::TransportClosed)]);
        let second_notification = Notification::new(vec![7], Some("next".into()));
        let second = SessionConfig::new(vec![Ok(SessionEvent::Notification(
            second_notification.clone(),
        ))]);
        let (connector, calls, call_notify) =
            ScriptConnector::new(vec![ConnectPlan::Session(first), ConnectPlan::Session(second)]);
        let connector = Arc::new(connector);
        let mut handle = start_connected(connector, 2, 2, no_jitter_policy(3));
        let readiness = handle.readiness_watch();
        assert_eq!(*readiness.borrow(), Readiness::Starting);

        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::Acquire), 1);
        tokio::time::advance(Duration::from_millis(99)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::Acquire), 1);
        tokio::time::advance(Duration::from_millis(1)).await;
        wait_for_count(&calls, &call_notify, 2).await;
        let received = handle.recv().await;
        assert_eq!(received, Some(second_notification));
        assert_eq!(*readiness.borrow(), Readiness::Ready);
        assert_ne!(*readiness.borrow(), Readiness::Stopped);
        assert!(handle.shutdown().await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn login_then_receive_errors_use_growing_backoff_and_stop() {
        let plans = (0..4)
            .map(|_| {
                ConnectPlan::Session(SessionConfig::new(vec![Err(
                    Error::TransportClosed,
                )]))
            })
            .collect();
        let (connector, calls, call_notify) = ScriptConnector::new(plans);
        let connector = Arc::new(connector);
        let handle = start_connected(connector, 1, 1, no_jitter_policy(3));
        let readiness = handle.readiness_watch();

        wait_for_count(&calls, &call_notify, 1).await;
        tokio::task::yield_now().await;
        assert_eq!(*readiness.borrow(), Readiness::Starting);
        tokio::time::advance(Duration::from_millis(99)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::Acquire), 1);
        tokio::time::advance(Duration::from_millis(1)).await;
        wait_for_count(&calls, &call_notify, 2).await;
        tokio::time::advance(Duration::from_millis(199)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::Acquire), 2);
        tokio::time::advance(Duration::from_millis(1)).await;
        wait_for_count(&calls, &call_notify, 3).await;
        tokio::time::advance(Duration::from_millis(399)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::Acquire), 3);
        tokio::time::advance(Duration::from_millis(1)).await;
        wait_for_count(&calls, &call_notify, 4).await;
        let result = handle.shutdown().await;
        assert!(matches!(result, Err(Error::TransportClosed)));
        assert_eq!(*readiness.borrow(), Readiness::Stopped);
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_liveness_prevents_reconnect_and_silence_reconnects() {
        let heartbeat_count = Arc::new(AtomicUsize::new(0));
        let heartbeat_notify = Arc::new(Notify::new());
        let mut live_config = SessionConfig::new(Vec::new());
        live_config.heartbeat_interval = Duration::from_secs(1);
        live_config.heartbeat_notify = Some(Arc::clone(&heartbeat_notify));
        live_config.heartbeat_count = Some(Arc::clone(&heartbeat_count));
        let (connector, calls, call_notify) =
            ScriptConnector::new(vec![ConnectPlan::Session(live_config)]);
        let connector = Arc::new(connector);
        let handle = start_connected(connector, 1, 1, no_jitter_policy(2));
        wait_for_count(&calls, &call_notify, 1).await;
        for expected in 1..=3 {
            tokio::time::advance(Duration::from_secs(1)).await;
            wait_for_count_without_notify(&heartbeat_count, expected).await;
            tokio::task::yield_now().await;
        }
        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert!(handle.shutdown().await.is_ok());

        let silent_config = {
            let mut config = SessionConfig::new(Vec::new());
            config.heartbeat_interval = Duration::from_secs(1);
            config
        };
        let (connector, calls, call_notify) = ScriptConnector::new(vec![
            ConnectPlan::Session(silent_config),
            ConnectPlan::Session(SessionConfig::new(Vec::new())),
        ]);
        let connector = Arc::new(connector);
        let handle = start_connected(connector, 1, 1, no_jitter_policy(2));
        wait_for_count(&calls, &call_notify, 1).await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(100)).await;
        wait_for_count(&calls, &call_notify, 2).await;
        assert!(handle.shutdown().await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn queue_byte_budget_waits_for_consumer_and_shutdown_does_not_ack() {
        let acknowledgements = Arc::new(AtomicUsize::new(0));
        let events = notification_events(9);
        let mut config = SessionConfig::new(events);
        config.acknowledgements = Some(Arc::clone(&acknowledgements));
        let (connector, calls, call_notify) = ScriptConnector::new(vec![ConnectPlan::Session(config)]);
        let connector = Arc::new(connector);
        let mut handle = start_connected(
            connector,
            MAX_QUEUE_CAPACITY,
            16,
            no_jitter_policy(1),
        );
        wait_for_count(&calls, &call_notify, 1).await;
        wait_for_count_without_notify(&acknowledgements, 8).await;
        assert_eq!(acknowledgements.load(Ordering::Acquire), 8);
        assert!(handle.recv().await.is_some());
        for _ in 0..100 {
            if acknowledgements.load(Ordering::Acquire) == 9 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(acknowledgements.load(Ordering::Acquire), 9);
        assert!(handle.shutdown().await.is_ok());

        let acknowledgements = Arc::new(AtomicUsize::new(0));
        let mut config = SessionConfig::new(notification_events(9));
        config.acknowledgements = Some(Arc::clone(&acknowledgements));
        let (connector, calls, call_notify) = ScriptConnector::new(vec![ConnectPlan::Session(config)]);
        let connector = Arc::new(connector);
        let handle = start_connected(
            connector,
            MAX_QUEUE_CAPACITY,
            16,
            no_jitter_policy(1),
        );
        let readiness = handle.readiness_watch();
        wait_for_count(&calls, &call_notify, 1).await;
        wait_for_count_without_notify(&acknowledgements, 8).await;
        let result = handle.shutdown().await;
        assert!(result.is_ok());
        assert_eq!(acknowledgements.load(Ordering::Acquire), 8);
        assert_eq!(*readiness.borrow(), Readiness::Stopped);
    }

    #[tokio::test(start_paused = true)]
    async fn oversized_notification_is_force_acked_and_next_is_delivered() {
        let force_acknowledgements = Arc::new(AtomicUsize::new(0));
        let acknowledgements = Arc::new(AtomicUsize::new(0));
        let oversized = Notification::new(vec![0; MAX_NOTIFICATION_BYTES + 1], None);
        let next = Notification::new(vec![9], Some("next".into()));
        let mut config = SessionConfig::new(vec![
            Ok(SessionEvent::Notification(oversized)),
            Ok(SessionEvent::Notification(next.clone())),
        ]);
        config.force_acknowledgements = Some(Arc::clone(&force_acknowledgements));
        config.acknowledgements = Some(Arc::clone(&acknowledgements));
        let (connector, calls, call_notify) = ScriptConnector::new(vec![ConnectPlan::Session(config)]);
        let connector = Arc::new(connector);
        let mut handle = start_connected(connector, 2, 2, no_jitter_policy(1));
        wait_for_count(&calls, &call_notify, 1).await;
        let received = handle.recv().await;
        assert_eq!(received, Some(next));
        for _ in 0..100 {
            if force_acknowledgements.load(Ordering::Acquire) == 1
                && acknowledgements.load(Ordering::Acquire) == 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(force_acknowledgements.load(Ordering::Acquire), 1);
        assert_eq!(acknowledgements.load(Ordering::Acquire), 1);
        assert!(handle.shutdown().await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_cancels_stalled_acknowledgement() {
        let ack_started = Arc::new(Notify::new());
        let acknowledgements = Arc::new(AtomicUsize::new(0));
        let mut config = SessionConfig::new(vec![Ok(SessionEvent::Notification(
            Notification::new(vec![1], Some("stall".into())),
        ))]);
        config.ack_started = Some(Arc::clone(&ack_started));
        config.acknowledgements = Some(Arc::clone(&acknowledgements));
        config.stall_ack = true;
        let (connector, calls, call_notify) = ScriptConnector::new(vec![ConnectPlan::Session(config)]);
        let connector = Arc::new(connector);
        let mut handle = start_connected(connector, 1, 1, no_jitter_policy(1));
        let readiness = handle.readiness_watch();
        wait_for_count(&calls, &call_notify, 1).await;
        assert!(handle.recv().await.is_some());
        ack_started.notified().await;
        let result = tokio::time::timeout(Duration::from_secs(1), handle.shutdown()).await;
        assert!(matches!(result, Ok(Ok(()))));
        assert_eq!(acknowledgements.load(Ordering::Acquire), 0);
        assert_eq!(*readiness.borrow(), Readiness::Stopped);
    }

    #[test]
    fn rejects_zero_source_capacity() {
        assert!(ReceiverSource::channel(0).is_err());
    }
}
