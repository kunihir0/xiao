use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use bot_domain::{Pairing, ServerId, SessionId};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinSet,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

use crate::{
    PairingSource, PairingStore, ServiceError, ServiceReadiness, SessionConnector, SessionError,
    SessionEvent, StoreError,
};

#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub connect_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub dedupe_capacity: usize,
    pub dedupe_ttl: Duration,
    pub max_servers: usize,
}
impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(15),
            shutdown_timeout: Duration::from_secs(5),
            dedupe_capacity: 256,
            dedupe_ttl: Duration::from_secs(900),
            max_servers: 64,
        }
    }
}
impl SessionConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.connect_timeout.is_zero() || self.shutdown_timeout.is_zero() {
            return Err("timeouts must be positive");
        }
        if self.connect_timeout < self.shutdown_timeout {
            return Err("connect timeout must not be shorter than shutdown timeout");
        }
        if self.dedupe_capacity == 0 || self.dedupe_ttl.is_zero() || self.max_servers == 0 {
            return Err("capacities and dedupe TTL must be positive");
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct PairingServiceHandle {
    readiness: watch::Receiver<ServiceReadiness>,
}
impl PairingServiceHandle {
    pub fn readiness(&self) -> ServiceReadiness {
        *self.readiness.borrow()
    }
    pub fn readiness_watch(&self) -> watch::Receiver<ServiceReadiness> {
        self.readiness.clone()
    }
}

struct DedupeEntry {
    at: Instant,
    delivery: Option<String>,
    structural: String,
}
struct WorkerRecord {
    session_id: SessionId,
    pairing: Pairing,
    cancellation: CancellationToken,
    finished: oneshot::Receiver<()>,
    abort: tokio::task::AbortHandle,
}
enum WorkerMessage {
    Event {
        server: ServerId,
        session_id: SessionId,
        event: SessionEvent,
    },
}

pub struct PairingService<S, C> {
    source: S,
    connector: Arc<C>,
    store: Arc<dyn PairingStore>,
    config: SessionConfig,
    readiness: watch::Sender<ServiceReadiness>,
    worker_events: mpsc::Sender<WorkerMessage>,
    worker_rx: mpsc::Receiver<WorkerMessage>,
    workers: JoinSet<()>,
    sessions: HashMap<ServerId, WorkerRecord>,
    dedupe: VecDeque<DedupeEntry>,
    next_session_id: u64,
}

impl<S, C> PairingService<S, C>
where
    S: PairingSource + 'static,
    C: SessionConnector + 'static,
{
    pub fn new(
        source: S,
        connector: C,
        store: impl PairingStore + 'static,
        config: SessionConfig,
    ) -> (Self, PairingServiceHandle) {
        let (readiness, readiness_rx) = watch::channel(ServiceReadiness::Starting);
        let capacity = config.max_servers.saturating_mul(4).clamp(1, 1024);
        let (worker_events, worker_rx) = mpsc::channel(capacity);
        (
            Self {
                source,
                connector: Arc::new(connector),
                store: Arc::new(store),
                config,
                readiness,
                worker_events,
                worker_rx,
                workers: JoinSet::new(),
                sessions: HashMap::new(),
                dedupe: VecDeque::new(),
                next_session_id: 0,
            },
            PairingServiceHandle {
                readiness: readiness_rx,
            },
        )
    }

    pub async fn run(mut self, cancellation: CancellationToken) -> Result<(), ServiceError> {
        if let Err(reason) = self.config.validate() {
            self.readiness.send_replace(ServiceReadiness::Failed);
            return Err(ServiceError::Store(StoreError::Invalid(reason.to_owned())));
        }
        self.store.load().await.map_err(ServiceError::Store)?;
        if self.source.readiness() == crate::SourceReadiness::Ready {
            self.readiness.send_replace(ServiceReadiness::Ready);
        }
        loop {
            if self.source.readiness() == crate::SourceReadiness::Ready {
                self.readiness.send_replace(ServiceReadiness::Ready);
            }
            tokio::select! {
                _ = cancellation.cancelled() => break self.stop().await,
                message = self.worker_rx.recv() => if let Some(message) = message { self.handle_worker_message(message).await },
                result = self.source.next() => match result {
                    Ok(Some(pairing)) => match self.handle_pairing(pairing).await {
                        Ok(()) | Err(ServiceError::Store(StoreError::Unavailable)) => {}
                        Err(error) => break self.fail(error).await,
                    },
                    Ok(None) => break self.fail(ServiceError::SourceClosed).await,
                    Err(error) => break self.fail(ServiceError::Source(error)).await,
                },
            }
        }
    }

    async fn handle_pairing(&mut self, pairing: Pairing) -> Result<(), ServiceError> {
        if self.is_duplicate(&pairing) {
            return Ok(());
        }
        self.record_dedupe(&pairing);
        let server = pairing.server_id().clone();
        if let Some(record) = self.sessions.remove(&server) {
            if record.pairing.same_credentials(&pairing) {
                let stored = pairing.clone();
                self.sessions
                    .insert(server.clone(), WorkerRecord { pairing, ..record });
                self.store
                    .upsert(&stored)
                    .await
                    .map_err(ServiceError::Store)?;
                return Ok(());
            }
            self.stop_record(record, false).await?;
        }
        if self.sessions.len() >= self.config.max_servers {
            return Err(ServiceError::Store(StoreError::Unavailable));
        }
        self.store
            .upsert(&pairing)
            .await
            .map_err(ServiceError::Store)?;
        let session_id = self.allocate_session_id();
        let cancellation = CancellationToken::new();
        let (finished_tx, finished_rx) = oneshot::channel();
        let connector = Arc::clone(&self.connector);
        let config = self.config.clone();
        let events = self.worker_events.clone();
        let worker_pairing = pairing.clone();
        let worker_server = server.clone();
        let worker_cancellation = cancellation.clone();
        let worker = self.workers.spawn(async move {
            run_worker(
                connector,
                worker_pairing,
                worker_server,
                session_id,
                worker_cancellation,
                config,
                events,
            )
            .await;
            let _ = finished_tx.send(());
        });
        self.sessions.insert(
            server,
            WorkerRecord {
                session_id,
                pairing,
                cancellation,
                finished: finished_rx,
                abort: worker,
            },
        );
        Ok(())
    }

    async fn handle_worker_message(&mut self, message: WorkerMessage) {
        let WorkerMessage::Event {
            server,
            session_id,
            event,
        } = message;
        let Some(record) = self.sessions.get(&server) else {
            return;
        };
        if record.session_id != session_id {
            return;
        }
        if matches!(
            event,
            SessionEvent::Disconnected | SessionEvent::Terminated { .. }
        ) {
            self.sessions.remove(&server);
        }
    }
    fn is_duplicate(&mut self, pairing: &Pairing) -> bool {
        let now = Instant::now();
        while self
            .dedupe
            .front()
            .is_some_and(|entry| now.duration_since(entry.at) > self.config.dedupe_ttl)
        {
            self.dedupe.pop_front();
        }
        let structural = structural_key(pairing);
        let delivery = pairing.delivery_id().map(|id| id.as_str().to_owned());
        self.dedupe.iter().any(|entry| {
            (delivery.is_some() && entry.delivery.as_deref() == delivery.as_deref())
                || entry.structural == structural
        })
    }
    fn record_dedupe(&mut self, pairing: &Pairing) {
        while self.dedupe.len() >= self.config.dedupe_capacity {
            self.dedupe.pop_front();
        }
        self.dedupe.push_back(DedupeEntry {
            at: Instant::now(),
            delivery: pairing.delivery_id().map(|id| id.as_str().to_owned()),
            structural: structural_key(pairing),
        });
    }
    fn allocate_session_id(&mut self) -> SessionId {
        self.next_session_id = self.next_session_id.wrapping_add(1).max(1);
        SessionId::new(self.next_session_id)
    }
    async fn stop_record(&self, record: WorkerRecord, shutdown: bool) -> Result<(), ServiceError> {
        record.cancellation.cancel();
        if timeout(self.config.shutdown_timeout, record.finished)
            .await
            .is_err()
        {
            record.abort.abort();
            if shutdown {
                return Err(ServiceError::ShutdownTimeout(self.config.shutdown_timeout));
            }
        }
        Ok(())
    }
    async fn stop(&mut self) -> Result<(), ServiceError> {
        self.readiness.send_replace(ServiceReadiness::Stopping);
        let source_result = timeout(self.config.shutdown_timeout, self.source.shutdown()).await;
        let mut timed_out = source_result.is_err();
        let records: Vec<_> = self.sessions.drain().map(|(_, record)| record).collect();
        for record in records {
            if self.stop_record(record, true).await.is_err() {
                timed_out = true;
            }
        }
        while self.workers.join_next().await.is_some() {}
        if timed_out {
            self.readiness.send_replace(ServiceReadiness::Failed);
            return Err(ServiceError::ShutdownTimeout(self.config.shutdown_timeout));
        }
        match source_result {
            Ok(Ok(())) => {
                self.readiness.send_replace(ServiceReadiness::Stopped);
                Ok(())
            }
            Ok(Err(error)) => {
                self.readiness.send_replace(ServiceReadiness::Failed);
                Err(ServiceError::Source(error))
            }
            Err(_) => {
                self.readiness.send_replace(ServiceReadiness::Failed);
                Err(ServiceError::ShutdownTimeout(self.config.shutdown_timeout))
            }
        }
    }
    async fn fail(&mut self, error: ServiceError) -> Result<(), ServiceError> {
        self.readiness.send_replace(ServiceReadiness::Failed);
        let _ = timeout(self.config.shutdown_timeout, self.source.shutdown()).await;
        let records: Vec<_> = self.sessions.drain().map(|(_, record)| record).collect();
        for record in records {
            let _ = self.stop_record(record, false).await;
        }
        while self.workers.join_next().await.is_some() {}
        Err(error)
    }
}

fn structural_key(pairing: &Pairing) -> String {
    let entity = pairing
        .entity()
        .map(|entity| {
            format!(
                "{}:{}:{}",
                entity.id().value(),
                entity.kind().unwrap_or_default(),
                entity.name().unwrap_or_default()
            )
        })
        .unwrap_or_default();
    format!(
        "{}:{}:{}:{}",
        pairing.server_id().as_str(),
        pairing.credentials().player_id().value(),
        pairing.credentials().player_token().value(),
        entity
    )
}

async fn run_worker<C: SessionConnector + 'static>(
    connector: Arc<C>,
    pairing: Pairing,
    server: ServerId,
    session_id: SessionId,
    cancellation: CancellationToken,
    config: SessionConfig,
    events: mpsc::Sender<WorkerMessage>,
) {
    let mut handle = tokio::select! {
        _ = cancellation.cancelled() => return,
        result = timeout(config.connect_timeout, connector.connect(&pairing)) => match result {
            Ok(Ok(handle)) => handle,
            Ok(Err(error)) => { let _ = events.send(WorkerMessage::Event { server, session_id, event: SessionEvent::Terminated { error: Some(error) } }).await; return; }
            Err(_) => { let _ = events.send(WorkerMessage::Event { server, session_id, event: SessionEvent::Terminated { error: Some(SessionError::ConnectTimeout(config.connect_timeout)) } }).await; return; }
        }
    };
    let _ = events
        .send(WorkerMessage::Event {
            server: server.clone(),
            session_id,
            event: SessionEvent::Ready,
        })
        .await;
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => { let result = timeout(config.shutdown_timeout, handle.shutdown()).await; if let Ok(Err(error)) = result { let _ = events.send(WorkerMessage::Event { server: server.clone(), session_id, event: SessionEvent::Terminated { error: Some(error) }}).await; } else if result.is_err() { let _ = events.send(WorkerMessage::Event { server: server.clone(), session_id, event: SessionEvent::Terminated { error: Some(SessionError::ShutdownTimeout(config.shutdown_timeout)) }}).await; } return; }
            result = handle.next_event() => match result {
                Ok(Some(event)) => { let terminal = matches!(event, SessionEvent::Disconnected | SessionEvent::Terminated { .. }); if events.send(WorkerMessage::Event { server: server.clone(), session_id, event }).await.is_err() || terminal { return; } }
                Ok(None) => { let _ = events.send(WorkerMessage::Event { server: server.clone(), session_id, event: SessionEvent::Terminated { error: None }}).await; return; }
                Err(error) => { let _ = events.send(WorkerMessage::Event { server: server.clone(), session_id, event: SessionEvent::Terminated { error: Some(error) }}).await; return; }
            }
        }
    }
}
