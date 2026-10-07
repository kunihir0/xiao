use bot_application::{BoxFuture, PairingSource, PairingSourceError, SourceReadiness};
use bot_domain::Pairing;
use tokio::sync::watch;
use tokio_util::task::AbortOnDropHandle;

use crate::{ParseConfig, parse::parse_notification};

pub struct PushPairingSource {
    receiver: Option<push_receiver::ReceiverHandle>,
    config: ParseConfig,
    readiness: watch::Receiver<SourceReadiness>,
    readiness_task: Option<AbortOnDropHandle<()>>,
}

impl PushPairingSource {
    pub fn new(receiver: push_receiver::ReceiverHandle, config: ParseConfig) -> Self {
        let mut receiver_readiness = receiver.readiness_watch();
        let (readiness_tx, readiness) = watch::channel(map_readiness(*receiver_readiness.borrow()));
        let readiness_task = tokio::spawn(async move {
            while receiver_readiness.changed().await.is_ok() {
                let state = map_readiness(*receiver_readiness.borrow());
                if readiness_tx.send(state).is_err() || state == SourceReadiness::Stopped {
                    break;
                }
            }
        });
        Self {
            receiver: Some(receiver),
            config,
            readiness,
            readiness_task: Some(AbortOnDropHandle::new(readiness_task)),
        }
    }
}

impl PairingSource for PushPairingSource {
    fn readiness(&self) -> SourceReadiness {
        *self.readiness.borrow()
    }

    fn readiness_watch(&self) -> watch::Receiver<SourceReadiness> {
        self.readiness.clone()
    }

    fn next<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<Pairing>, PairingSourceError>> {
        Box::pin(async move {
            let receiver = self.receiver.as_mut().ok_or(PairingSourceError::Closed)?;
            loop {
                let Some(notification) = receiver.recv().await else {
                    return Err(PairingSourceError::Closed);
                };
                match parse_notification(&notification, &self.config) {
                    Ok(pairing) => return Ok(pairing),
                    Err(_) => {
                        // Malformed untrusted push data is isolated to this
                        // notification; the receiver remains a live source.
                    }
                }
            }
        })
    }

    fn shutdown<'a>(&'a mut self) -> BoxFuture<'a, Result<(), PairingSourceError>> {
        Box::pin(async move {
            let result = match self.receiver.take() {
                Some(receiver) => receiver
                    .shutdown()
                    .await
                    .map_err(|error| PairingSourceError::Shutdown(error.to_string())),
                None => Ok(()),
            };
            if let Some(task) = self.readiness_task.take() {
                task.abort();
                let _ = task.await;
            }
            result
        })
    }
}

fn map_readiness(readiness: push_receiver::Readiness) -> SourceReadiness {
    match readiness {
        push_receiver::Readiness::Starting => SourceReadiness::Starting,
        push_receiver::Readiness::Ready => SourceReadiness::Ready,
        push_receiver::Readiness::Stopped => SourceReadiness::Stopped,
    }
}
#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn synthetic_receiver_notification_becomes_typed_pairing() {
        let (sender, receiver_source) = push_receiver::ReceiverSource::channel(1).expect("channel");
        let receiver = push_receiver::start(receiver_source, 1, 8);
        let mut source = PushPairingSource::new(receiver, ParseConfig::default());
        let mut notification =
            push_receiver::Notification::new(Vec::new(), Some("delivery-typed".to_owned()));
        notification.app_data.push(push_receiver::AppData {
            key: "pairing".to_owned(),
            value: r#"{"ip":"8.8.8.8","port":28015,"playerId":42,"playerToken":7,"type":"server"}"#
                .to_owned(),
        });
        sender.send(Ok(notification)).await.expect("notification");

        let pairing = source
            .next()
            .await
            .expect("source result")
            .expect("pairing");
        assert_eq!(pairing.address().host(), "8.8.8.8");
        assert_eq!(pairing.address().port(), 28015);
        assert_eq!(
            pairing.delivery_id().map(|delivery| delivery.as_str()),
            Some("delivery-typed")
        );
        source.shutdown().await.expect("source shutdown");
    }
}
