use bot_application::{BoxFuture, PairingSource, PairingSourceError, SourceReadiness};
use bot_domain::Pairing;

use crate::{ParseConfig, parse::parse_notification};

pub struct PushPairingSource {
    receiver: Option<push_receiver::ReceiverHandle>,
    config: ParseConfig,
}

impl PushPairingSource {
    pub fn new(receiver: push_receiver::ReceiverHandle, config: ParseConfig) -> Self {
        Self {
            receiver: Some(receiver),
            config,
        }
    }
}

impl PairingSource for PushPairingSource {
    fn readiness(&self) -> SourceReadiness {
        match self
            .receiver
            .as_ref()
            .map(push_receiver::ReceiverHandle::readiness)
        {
            Some(push_receiver::Readiness::Starting) => SourceReadiness::Starting,
            Some(push_receiver::Readiness::Ready) => SourceReadiness::Ready,
            None | Some(push_receiver::Readiness::Stopped) => SourceReadiness::Stopped,
        }
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
            let Some(receiver) = self.receiver.take() else {
                return Ok(());
            };
            receiver
                .shutdown()
                .await
                .map_err(|error| PairingSourceError::Shutdown(error.to_string()))
        })
    }
}
