use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use bot_application::{BoxFuture, PairingSource, PairingSourceError, SourceReadiness};
use bot_domain::Pairing;
use tokio::sync::{mpsc, watch};

pub struct FakePairingSource {
    receiver: mpsc::Receiver<Result<Option<Pairing>, PairingSourceError>>,
    readiness: watch::Receiver<SourceReadiness>,
    readiness_sender: watch::Sender<SourceReadiness>,
    shutdown_called: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct FakePairingSender {
    sender: mpsc::Sender<Result<Option<Pairing>, PairingSourceError>>,
    readiness: watch::Sender<SourceReadiness>,
    shutdown_called: Arc<AtomicBool>,
}

#[derive(Debug)]
pub struct SendError;

impl FakePairingSource {
    pub fn new(capacity: usize) -> (Self, FakePairingSender) {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        let (readiness_sender, readiness) = watch::channel(SourceReadiness::Ready);
        let shutdown_called = Arc::new(AtomicBool::new(false));
        (
            Self {
                receiver,
                readiness: readiness.clone(),
                readiness_sender: readiness_sender.clone(),
                shutdown_called: Arc::clone(&shutdown_called),
            },
            FakePairingSender {
                sender,
                readiness: readiness_sender,
                shutdown_called,
            },
        )
    }

    pub fn with_readiness(self, readiness: SourceReadiness) -> Self {
        self.readiness_sender.send_replace(readiness);
        self
    }

    pub fn shutdown_called(&self) -> bool {
        self.shutdown_called.load(Ordering::Acquire)
    }
}

impl FakePairingSender {
    pub async fn send(&self, pairing: Pairing) -> Result<(), SendError> {
        self.sender
            .send(Ok(Some(pairing)))
            .await
            .map_err(|_| SendError)
    }

    pub async fn send_result(
        &self,
        result: Result<Option<Pairing>, PairingSourceError>,
    ) -> Result<(), SendError> {
        self.sender.send(result).await.map_err(|_| SendError)
    }
    pub fn shutdown_called(&self) -> bool {
        self.shutdown_called.load(Ordering::Acquire)
    }
    pub fn set_readiness(&self, readiness: SourceReadiness) {
        self.readiness.send_replace(readiness);
    }
}

impl PairingSource for FakePairingSource {
    fn readiness(&self) -> SourceReadiness {
        *self.readiness.borrow()
    }

    fn readiness_watch(&self) -> watch::Receiver<SourceReadiness> {
        self.readiness.clone()
    }

    fn next<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<Pairing>, PairingSourceError>> {
        Box::pin(async move {
            match self.receiver.recv().await {
                Some(result) => result,
                None => Err(PairingSourceError::Failed),
            }
        })
    }

    fn shutdown<'a>(&'a mut self) -> BoxFuture<'a, Result<(), PairingSourceError>> {
        Box::pin(async move {
            self.shutdown_called.store(true, Ordering::Release);
            Ok(())
        })
    }
}
