use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use bot_application::{
    BoxFuture, SessionConnector, SessionError, SessionEvent, SessionHandle, SessionReadiness,
};
use bot_domain::{Pairing, ServerId};
use tokio::sync::{Mutex, mpsc};

pub struct FakeSessionHandle {
    events: mpsc::Receiver<Result<Option<SessionEvent>, SessionError>>,
    readiness: SessionReadiness,
    shutdown_called: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct FakeSessionSender {
    sender: mpsc::Sender<Result<Option<SessionEvent>, SessionError>>,
    shutdown_called: Arc<AtomicBool>,
}

impl FakeSessionHandle {
    pub fn new(capacity: usize, readiness: SessionReadiness) -> (Self, FakeSessionSender) {
        let (sender, events) = mpsc::channel(capacity.max(1));
        let shutdown_called = Arc::new(AtomicBool::new(false));
        (
            Self {
                events,
                readiness,
                shutdown_called: Arc::clone(&shutdown_called),
            },
            FakeSessionSender {
                sender,
                shutdown_called,
            },
        )
    }

    pub fn shutdown_called(&self) -> bool {
        self.shutdown_called.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
pub struct FakeSessionSendError;

impl FakeSessionSender {
    pub fn shutdown_called(&self) -> bool {
        self.shutdown_called.load(Ordering::Acquire)
    }

    pub async fn send(&self, event: SessionEvent) -> Result<(), FakeSessionSendError> {
        self.sender
            .send(Ok(Some(event)))
            .await
            .map_err(|_| FakeSessionSendError)
    }
    pub async fn send_result(
        &self,
        event: Result<Option<SessionEvent>, SessionError>,
    ) -> Result<(), FakeSessionSendError> {
        self.sender
            .send(event)
            .await
            .map_err(|_| FakeSessionSendError)
    }
}

impl SessionHandle for FakeSessionHandle {
    fn readiness(&self) -> SessionReadiness {
        self.readiness
    }
    fn next_event<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<SessionEvent>, SessionError>> {
        Box::pin(async move {
            match self.events.recv().await {
                Some(event) => event,
                None => Ok(None),
            }
        })
    }
    fn shutdown<'a>(&'a mut self) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            self.shutdown_called.store(true, Ordering::Release);
            Ok(())
        })
    }
}

type SessionScript = Result<FakeSessionHandle, SessionError>;

#[derive(Clone)]
pub struct FakeSessionConnector {
    calls: Arc<Mutex<Vec<ServerId>>>,
    scripts: Arc<Mutex<HashMap<ServerId, Vec<SessionScript>>>>,
}

impl Default for FakeSessionConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeSessionConnector {
    pub fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            scripts: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    pub async fn script(&self, server: ServerId, handle: FakeSessionHandle) {
        self.scripts
            .lock()
            .await
            .entry(server)
            .or_default()
            .push(Ok(handle));
    }
    pub async fn fail(&self, server: ServerId, error: SessionError) {
        self.scripts
            .lock()
            .await
            .entry(server)
            .or_default()
            .push(Err(error));
    }
    pub async fn connect_calls(&self) -> Vec<ServerId> {
        self.calls.lock().await.clone()
    }
}

impl SessionConnector for FakeSessionConnector {
    fn connect<'a>(
        &'a self,
        pairing: &'a Pairing,
    ) -> BoxFuture<'a, Result<Box<dyn SessionHandle>, SessionError>> {
        Box::pin(async move {
            let server = pairing.server_id().clone();
            self.calls.lock().await.push(server.clone());
            let scripted = self
                .scripts
                .lock()
                .await
                .get_mut(&server)
                .and_then(|values| {
                    if values.is_empty() {
                        None
                    } else {
                        Some(values.remove(0))
                    }
                });
            match scripted
                .unwrap_or_else(|| Ok(FakeSessionHandle::new(8, SessionReadiness::Ready).0))
            {
                Ok(handle) => Ok(Box::new(handle) as Box<dyn SessionHandle>),
                Err(error) => Err(error),
            }
        })
    }
}
