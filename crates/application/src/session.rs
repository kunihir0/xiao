use std::{
    collections::{HashMap, VecDeque},
    hash::{BuildHasher, RandomState},
    sync::Arc,
    time::Duration,
};

use bot_domain::{Pairing, ServerId, SessionId};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinSet,
    time::{Instant, timeout},
};
use tokio_util::sync::CancellationToken;

use crate::{
    PairingSource, PairingSourceError, PairingStore, ServiceError, ServiceReadiness,
    SessionConnector, SessionError, SessionEvent, StoreError,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionState {
    Connecting,
    Ready,
    Reconnecting,
    Closing,
    Failed { reason: SessionError },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSnapshot {
    pub server_id: ServerId,
    pub session_id: SessionId,
    pub state: SessionState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
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
    sessions: watch::Receiver<Vec<SessionSnapshot>>,
}
impl PairingServiceHandle {
    pub fn readiness(&self) -> ServiceReadiness {
        *self.readiness.borrow()
    }
    pub fn readiness_watch(&self) -> watch::Receiver<ServiceReadiness> {
        self.readiness.clone()
    }
    pub fn sessions(&self) -> Vec<SessionSnapshot> {
        self.sessions.borrow().clone()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct EntityKey {
    id: u32,
    kind: Option<String>,
    name: Option<String>,
}
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct StructuralKey {
    server: ServerId,
    player_id: u64,
    token_hash: u64,
    entity: Option<EntityKey>,
}
struct DedupeEntry {
    at: Instant,
    delivery: Option<String>,
    structural: StructuralKey,
}
struct WorkerRecord {
    server: ServerId,
    session_id: SessionId,
    pairing: Pairing,
    state: SessionState,
    cancellation: CancellationToken,
    finished: oneshot::Receiver<Result<(), SessionError>>,
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
    sessions_watch: watch::Sender<Vec<SessionSnapshot>>,
    worker_events: mpsc::Sender<WorkerMessage>,
    worker_rx: mpsc::Receiver<WorkerMessage>,
    workers: JoinSet<()>,
    key_hasher: RandomState,
    sessions: HashMap<ServerId, WorkerRecord>,
    failed_order: VecDeque<ServerId>,
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
        let (sessions_watch, sessions_rx) = watch::channel(Vec::new());
        let capacity = config.max_servers.saturating_mul(4).clamp(1, 1024);
        let (worker_events, worker_rx) = mpsc::channel(capacity);
        (
            Self {
                source,
                connector: Arc::new(connector),
                store: Arc::new(store),
                config,
                readiness,
                sessions_watch,
                worker_events,
                worker_rx,
                workers: JoinSet::new(),
                sessions: HashMap::new(),
                failed_order: VecDeque::new(),
                dedupe: VecDeque::new(),
                next_session_id: 0,
                key_hasher: RandomState::new(),
            },
            PairingServiceHandle {
                readiness: readiness_rx,
                sessions: sessions_rx,
            },
        )
    }

    pub async fn run(mut self, cancellation: CancellationToken) -> Result<(), ServiceError> {
        if let Err(reason) = self.config.validate() {
            self.readiness.send_replace(ServiceReadiness::Failed);
            return Err(ServiceError::Store(StoreError::Invalid(reason.to_owned())));
        }
        if let Err(error) = self.store.load().await {
            self.readiness.send_replace(ServiceReadiness::Failed);
            return Err(ServiceError::Store(error));
        }
        let mut source_readiness = self.source.readiness_watch();
        let initial_readiness = *source_readiness.borrow();
        match initial_readiness {
            crate::SourceReadiness::Ready => {
                self.readiness.send_replace(ServiceReadiness::Ready);
            }
            crate::SourceReadiness::Failed => {
                return self
                    .fail(ServiceError::Source(PairingSourceError::Failed))
                    .await;
            }
            crate::SourceReadiness::Stopped => return self.stop().await,
            crate::SourceReadiness::Starting => {}
        }
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => break self.stop().await,
                readiness = source_readiness.changed() => match readiness {
                    Ok(()) => {
                        let readiness = *source_readiness.borrow();
                        match readiness {
                            crate::SourceReadiness::Ready => {
                                self.readiness.send_replace(ServiceReadiness::Ready);
                            }
                            crate::SourceReadiness::Starting => {
                                self.readiness.send_replace(ServiceReadiness::Starting);
                            }
                            crate::SourceReadiness::Stopped => {
                                break self.stop().await;
                            }
                            crate::SourceReadiness::Failed => {
                                break self
                                    .fail(ServiceError::Source(PairingSourceError::Failed))
                                    .await;
                            }
                        }
                    }
                    Err(_) => break self.fail(ServiceError::SourceClosed).await,
                },
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
            self.failed_order.retain(|failed| failed != &server);
            if record.pairing.same_credentials(&pairing)
                && !matches!(record.state, SessionState::Failed { .. })
            {
                let stored = pairing.clone();
                self.sessions
                    .insert(server.clone(), WorkerRecord { pairing, ..record });
                self.publish_snapshots();
                self.store
                    .upsert(&stored)
                    .await
                    .map_err(ServiceError::Store)?;
                return Ok(());
            }
            self.publish_snapshots();
            self.stop_record(record, false).await?;
        }
        if self.live_session_count() >= self.config.max_servers {
            return Err(ServiceError::Store(StoreError::Unavailable));
        }
        if self.sessions.len() >= self.config.max_servers {
            self.evict_failed_record().await?;
            if self.sessions.len() >= self.config.max_servers {
                return Err(ServiceError::Store(StoreError::Unavailable));
            }
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
            let result = run_worker(
                connector,
                worker_pairing,
                worker_server,
                session_id,
                worker_cancellation,
                config,
                events,
            )
            .await;
            let _ = finished_tx.send(result);
        });
        self.sessions.insert(
            server.clone(),
            WorkerRecord {
                server,
                session_id,
                pairing,
                state: SessionState::Connecting,
                cancellation,
                finished: finished_rx,
                abort: worker,
            },
        );
        self.publish_snapshots();
        Ok(())
    }

    async fn handle_worker_message(&mut self, message: WorkerMessage) {
        let WorkerMessage::Event {
            server,
            session_id,
            event,
        } = message;
        let Some(record) = self.sessions.get_mut(&server) else {
            return;
        };
        if record.session_id != session_id {
            return;
        }
        let failed = match event {
            SessionEvent::Ready => {
                record.state = SessionState::Ready;
                false
            }
            SessionEvent::Reconnecting => {
                record.state = SessionState::Reconnecting;
                false
            }
            SessionEvent::Disconnected => {
                record.state = SessionState::Failed {
                    reason: SessionError::Disconnected,
                };
                true
            }
            SessionEvent::Terminated { error } => {
                record.state = SessionState::Failed {
                    reason: error.unwrap_or(SessionError::Failed),
                };
                true
            }
            SessionEvent::Lagged { .. } => false,
        };
        if failed && !self.failed_order.contains(&server) {
            self.failed_order.push_back(server);
        }
        self.publish_snapshots();
    }

    fn publish_snapshots(&self) {
        let snapshots = self
            .sessions
            .iter()
            .map(|(server_id, record)| SessionSnapshot {
                server_id: server_id.clone(),
                session_id: record.session_id,
                state: record.state.clone(),
            })
            .collect();
        self.sessions_watch.send_replace(snapshots);
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
        let structural = structural_key(pairing, &self.key_hasher);
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
            structural: structural_key(pairing, &self.key_hasher),
        });
    }
    fn allocate_session_id(&mut self) -> SessionId {
        self.next_session_id = self.next_session_id.wrapping_add(1).max(1);
        SessionId::new(self.next_session_id)
    }
    fn live_session_count(&self) -> usize {
        self.sessions
            .values()
            .filter(|record| !matches!(record.state, SessionState::Failed { .. }))
            .count()
    }

    async fn evict_failed_record(&mut self) -> Result<(), ServiceError> {
        let server = loop {
            let Some(candidate) = self.failed_order.pop_front() else {
                return Ok(());
            };
            if self
                .sessions
                .get(&candidate)
                .is_some_and(|record| matches!(record.state, SessionState::Failed { .. }))
            {
                break candidate;
            }
        };
        if let Some(record) = self.sessions.remove(&server) {
            self.stop_record(record, false).await?;
            self.store
                .remove(&server)
                .await
                .map_err(ServiceError::Store)?;
            self.publish_snapshots();
        }
        Ok(())
    }
    async fn stop_record(&self, record: WorkerRecord, shutdown: bool) -> Result<(), ServiceError> {
        record.cancellation.cancel();
        match timeout(self.config.shutdown_timeout, record.finished).await {
            Err(_) if shutdown => {
                record.abort.abort();
                Err(ServiceError::Session {
                    server: record.server,
                    source: SessionError::ShutdownTimeout(self.config.shutdown_timeout),
                })
            }
            Err(_) => {
                record.abort.abort();
                Ok(())
            }
            Ok(Ok(Ok(()))) | Ok(Err(_)) => Ok(()),
            Ok(Ok(Err(error))) if shutdown => Err(ServiceError::Session {
                server: record.server,
                source: error,
            }),
            Ok(Ok(Err(_))) => Ok(()),
        }
    }
    async fn stop(&mut self) -> Result<(), ServiceError> {
        self.readiness.send_replace(ServiceReadiness::Stopping);
        let source_result = timeout(self.config.shutdown_timeout, self.source.shutdown()).await;
        self.failed_order.clear();
        let records: Vec<_> = self.sessions.drain().map(|(_, record)| record).collect();
        self.publish_snapshots();
        let mut failure = match source_result {
            Err(_) => Some(ServiceError::ShutdownTimeout(self.config.shutdown_timeout)),
            Ok(Err(error)) => Some(ServiceError::Source(error)),
            Ok(Ok(())) => None,
        };
        for record in records {
            if failure.is_none() {
                failure = self.stop_record(record, true).await.err();
            } else {
                let _ = self.stop_record(record, true).await;
            }
        }
        while self.workers.join_next().await.is_some() {}
        if let Some(error) = failure {
            self.readiness.send_replace(ServiceReadiness::Failed);
            return Err(error);
        }
        self.readiness.send_replace(ServiceReadiness::Stopped);
        Ok(())
    }
    async fn fail(&mut self, error: ServiceError) -> Result<(), ServiceError> {
        self.readiness.send_replace(ServiceReadiness::Failed);
        let _ = timeout(self.config.shutdown_timeout, self.source.shutdown()).await;
        self.failed_order.clear();
        let records: Vec<_> = self.sessions.drain().map(|(_, record)| record).collect();
        self.publish_snapshots();
        for record in records {
            let _ = self.stop_record(record, false).await;
        }
        while self.workers.join_next().await.is_some() {}
        Err(error)
    }
}
fn structural_key(pairing: &Pairing, key_hasher: &RandomState) -> StructuralKey {
    let token_hash = key_hasher.hash_one(pairing.credentials().player_token().value());
    StructuralKey {
        server: pairing.server_id().clone(),
        player_id: pairing.credentials().player_id().value(),
        token_hash,
        entity: pairing.entity().map(|entity| EntityKey {
            id: entity.id().value(),
            kind: entity.kind().map(str::to_owned),
            name: entity.name().map(str::to_owned),
        }),
    }
}

async fn run_worker<C: SessionConnector + 'static>(
    connector: Arc<C>,
    pairing: Pairing,
    server: ServerId,
    session_id: SessionId,
    cancellation: CancellationToken,
    config: SessionConfig,
    events: mpsc::Sender<WorkerMessage>,
) -> Result<(), SessionError> {
    let mut handle = tokio::select! {
        _ = cancellation.cancelled() => return Ok(()),
        result = timeout(config.connect_timeout, connector.connect(&pairing)) => match result {
            Ok(Ok(handle)) => handle,
            Ok(Err(error)) => {
                let _ = events.send(WorkerMessage::Event {
                    server,
                    session_id,
                    event: SessionEvent::Terminated { error: Some(error) },
                }).await;
                return Ok(());
            }
            Err(_) => {
                let _ = events.send(WorkerMessage::Event {
                    server,
                    session_id,
                    event: SessionEvent::Terminated {
                        error: Some(SessionError::ConnectTimeout(config.connect_timeout)),
                    },
                }).await;
                return Ok(());
            }
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
            _ = cancellation.cancelled() => {
                let result = match timeout(config.shutdown_timeout, handle.shutdown()).await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => Err(error),
                    Err(_) => Err(SessionError::ShutdownTimeout(config.shutdown_timeout)),
                };
                if let Err(error) = &result {
                    let _ = events.send(WorkerMessage::Event {
                        server: server.clone(),
                        session_id,
                        event: SessionEvent::Terminated { error: Some(error.clone()) },
                    }).await;
                }
                return result;
            }
            result = handle.next_event() => match result {
                Ok(Some(event)) => {
                    let terminal = matches!(
                        event,
                        SessionEvent::Disconnected | SessionEvent::Terminated { .. }
                    );
                    if events.send(WorkerMessage::Event {
                        server: server.clone(),
                        session_id,
                        event,
                    }).await.is_err() || terminal {
                        return Ok(());
                    }
                }
                Ok(None) => {
                    let _ = events.send(WorkerMessage::Event {
                        server: server.clone(),
                        session_id,
                        event: SessionEvent::Terminated { error: None },
                    }).await;
                    return Ok(());
                }
                Err(error) => {
                    let _ = events.send(WorkerMessage::Event {
                        server: server.clone(),
                        session_id,
                        event: SessionEvent::Terminated { error: Some(error) },
                    }).await;
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::structural_key;
    use bot_domain::{
        DomainError, EntityId, EntityPairing, MAX_ENTITY_KIND_BYTES, MAX_ENTITY_NAME_BYTES,
        Pairing, PlayerCredentials, PlayerId, PlayerToken, ServerAddress,
    };
    use std::hash::RandomState;

    #[test]
    fn structural_key_debug_does_not_retain_player_token() {
        let Ok(address) = ServerAddress::new("8.8.8.8", 28_015) else {
            return;
        };
        let Ok(player_id) = PlayerId::new(42) else {
            return;
        };
        let pairing = Pairing::new(
            address,
            PlayerCredentials::new(player_id, PlayerToken::new(123_456_789)),
            None,
            None,
        );
        let key = structural_key(&pairing, &RandomState::new());
        assert!(!format!("{key:?}").contains("123456789"));
    }
    #[test]
    fn structural_key_retains_only_bounded_entity_metadata() -> Result<(), DomainError> {
        let address = ServerAddress::new("8.8.8.8", 28_015)?;
        let player_id = PlayerId::new(42)?;
        let entity = EntityPairing::new(
            EntityId::new(1)?,
            Some("k".repeat(MAX_ENTITY_KIND_BYTES)),
            Some("n".repeat(MAX_ENTITY_NAME_BYTES)),
        )?;
        let pairing = Pairing::new(
            address,
            PlayerCredentials::new(player_id, PlayerToken::new(7)),
            Some(entity),
            None,
        );
        let key = structural_key(&pairing, &RandomState::new());
        let entity = key.entity.ok_or(DomainError::Invalid { field: "entity" })?;
        assert_eq!(
            entity.kind.as_deref().map(str::len),
            Some(MAX_ENTITY_KIND_BYTES)
        );
        assert_eq!(
            entity.name.as_deref().map(str::len),
            Some(MAX_ENTITY_NAME_BYTES)
        );
        Ok(())
    }
}
