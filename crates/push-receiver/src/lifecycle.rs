use std::fmt;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::{Error, Notification, PersistentIdWindow, Result, RetryPolicy};

pub const DEFAULT_QUEUE_CAPACITY: usize = 64;
pub const DEFAULT_DEDUPE_CAPACITY: usize = 256;
pub const MAX_QUEUE_CAPACITY: usize = 4096;

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
        self.notifications.recv().await
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

pub struct ReceiverSource {
    input: mpsc::Receiver<Result<Notification>>,
}
impl ReceiverSource {
    pub fn channel(capacity: usize) -> Result<(mpsc::Sender<Result<Notification>>, Self)> {
        if capacity == 0 {
            return Err(Error::InvalidRegistration(
                "receiver source capacity must be non-zero".into(),
            ));
        }
        let capacity = capacity.min(MAX_QUEUE_CAPACITY);
        let (sender, input) = mpsc::channel(capacity);
        Ok((sender, Self { input }))
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
    let task_readiness = readiness_tx.clone();
    let task = tokio::spawn(async move {
        let mut source = source.input;
        let mut dedupe = PersistentIdWindow::new(dedupe_capacity);
        let mut attempts: u32 = 0;
        task_readiness
            .send(Readiness::Ready)
            .map_err(|_| Error::ChannelClosed)?;
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => { let _ = task_readiness.send(Readiness::Stopped); return Ok(()); }
                item = source.recv() => {
                    let Some(item) = item else { let _ = task_readiness.send(Readiness::Stopped); return Ok(()); };
                    let notification = match item {
                        Ok(notification) => { attempts = 0; notification }
                        Err(error) => {
                            attempts = attempts.saturating_add(1);
                            if attempts > policy.max_attempts() { let _ = task_readiness.send(Readiness::Stopped); return Err(error); }
                            let _ = task_readiness.send(Readiness::Starting);
                            tokio::time::sleep(policy.delay(attempts, 0)).await;
                            task_readiness.send(Readiness::Ready).map_err(|_| Error::ChannelClosed)?;
                            continue;
                        }
                    };
                    if notification.persistent_id.as_deref().is_some_and(|id| dedupe.observe(id)) { continue; }
                    if notifications_tx.send(notification).await.is_err() { let _ = task_readiness.send(Readiness::Stopped); return Err(Error::ChannelClosed); }
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
    #[test]
    fn rejects_zero_source_capacity() {
        assert!(ReceiverSource::channel(0).is_err());
    }
}
