use std::{
    future::pending,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use bot_application::{
    BoxFuture, PairingService, PairingSourceError, ServiceError, ServiceReadiness, SessionConfig,
    SessionConnector, SessionError, SessionEvent, SessionHandle, SessionReadiness,
};
use bot_domain::{
    DeliveryId, DomainError, EntityId, EntityPairing, Pairing, PlayerCredentials, PlayerId,
    PlayerToken, ServerAddress, ServerId,
};
use bot_test_support::{FakePairingSource, FakePairingStore};
use tokio::{sync::Mutex, time::advance};
use tokio_util::sync::CancellationToken;

// The test-support connector is intentionally used through the public application ports.
// The small local connectors below only model timeout and ordering behavior unavailable from
// the normal scripted fake.

fn pairing(
    host: &str,
    player: u64,
    token: i32,
    entity: Option<u32>,
    delivery: Option<&str>,
) -> Result<Pairing, DomainError> {
    let entity = entity
        .map(|id| {
            EntityPairing::new(
                EntityId::new(id)?,
                Some("building".to_owned()),
                Some(format!("entity-{id}")),
            )
        })
        .transpose()?;
    let delivery = delivery.map(DeliveryId::new).transpose()?;
    Ok(Pairing::new(
        ServerAddress::new(host, 28_082)?,
        PlayerCredentials::new(PlayerId::new(player)?, PlayerToken::new(token)),
        entity,
        delivery,
    ))
}

fn config() -> SessionConfig {
    SessionConfig {
        connect_timeout: Duration::from_secs(10),
        shutdown_timeout: Duration::from_secs(2),
        dedupe_capacity: 8,
        dedupe_ttl: Duration::from_secs(30),
        max_servers: 8,
    }
}

async fn settle() {
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
}

async fn calls_at_least<C>(connector: &C, expected: usize) -> Vec<ServerId>
where
    C: Calls,
{
    for _ in 0..64 {
        let calls = connector.calls().await;
        if calls.len() >= expected {
            return calls;
        }
        tokio::task::yield_now().await;
    }
    connector.calls().await
}

trait Calls {
    fn calls(&self) -> BoxFuture<'_, Vec<ServerId>>;
}

impl Calls for bot_test_support::FakeSessionConnector {
    fn calls(&self) -> BoxFuture<'_, Vec<ServerId>> {
        Box::pin(self.connect_calls())
    }
}

impl Calls for BlockingConnector {
    fn calls(&self) -> BoxFuture<'_, Vec<ServerId>> {
        Box::pin(async { self.calls.lock().await.clone() })
    }
}

impl Calls for OrderingConnector {
    fn calls(&self) -> BoxFuture<'_, Vec<ServerId>> {
        Box::pin(async { self.calls.lock().await.clone() })
    }
}

struct BlockingConnector {
    calls: Arc<Mutex<Vec<ServerId>>>,
}

impl BlockingConnector {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl SessionConnector for BlockingConnector {
    fn connect<'a>(
        &'a self,
        pairing: &'a Pairing,
    ) -> BoxFuture<'a, Result<Box<dyn SessionHandle>, SessionError>> {
        Box::pin(async move {
            self.calls.lock().await.push(pairing.server_id().clone());
            pending::<Result<Box<dyn SessionHandle>, SessionError>>().await
        })
    }
}

struct EventHandle {
    events: Arc<Mutex<Vec<&'static str>>>,
}

impl SessionHandle for EventHandle {
    fn readiness(&self) -> SessionReadiness {
        SessionReadiness::Ready
    }

    fn next_event<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<SessionEvent>, SessionError>> {
        Box::pin(pending())
    }

    fn shutdown<'a>(&'a mut self) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            self.events.lock().await.push("shutdown");
            Ok(())
        })
    }
}

#[derive(Clone)]
struct OrderingConnector {
    calls: Arc<Mutex<Vec<ServerId>>>,
    events: Arc<Mutex<Vec<&'static str>>>,
}

impl OrderingConnector {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    async fn events(&self) -> Vec<&'static str> {
        self.events.lock().await.clone()
    }
}

impl SessionConnector for OrderingConnector {
    fn connect<'a>(
        &'a self,
        pairing: &'a Pairing,
    ) -> BoxFuture<'a, Result<Box<dyn SessionHandle>, SessionError>> {
        Box::pin(async move {
            self.calls.lock().await.push(pairing.server_id().clone());
            self.events.lock().await.push("connect");
            Ok(Box::new(EventHandle {
                events: Arc::clone(&self.events),
            }) as Box<dyn SessionHandle>)
        })
    }
}

struct StalledHandle;

impl SessionHandle for StalledHandle {
    fn readiness(&self) -> SessionReadiness {
        SessionReadiness::Ready
    }

    fn next_event<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<SessionEvent>, SessionError>> {
        Box::pin(pending())
    }

    fn shutdown<'a>(&'a mut self) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(pending())
    }
}

#[derive(Clone)]
struct ReplacementConnector {
    calls: Arc<Mutex<Vec<ServerId>>>,
    count: Arc<AtomicUsize>,
}

impl ReplacementConnector {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            count: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Calls for ReplacementConnector {
    fn calls(&self) -> BoxFuture<'_, Vec<ServerId>> {
        Box::pin(async { self.calls.lock().await.clone() })
    }
}

impl SessionConnector for ReplacementConnector {
    fn connect<'a>(
        &'a self,
        pairing: &'a Pairing,
    ) -> BoxFuture<'a, Result<Box<dyn SessionHandle>, SessionError>> {
        Box::pin(async move {
            self.calls.lock().await.push(pairing.server_id().clone());
            if self.count.fetch_add(1, Ordering::AcqRel) == 0 {
                Ok(Box::new(StalledHandle) as Box<dyn SessionHandle>)
            } else {
                Ok(Box::new(EventHandle {
                    events: Arc::new(Mutex::new(Vec::new())),
                }) as Box<dyn SessionHandle>)
            }
        })
    }
}

#[tokio::test(start_paused = true)]
async fn one_pairing_becomes_active_and_readiness_is_truthful() -> Result<(), DomainError> {
    let (source, sender) = FakePairingSource::new(8);
    let connector = bot_test_support::FakeSessionConnector::new();
    let store = FakePairingStore::default();
    let (service, handle) = PairingService::new(source, connector.clone(), store, config());
    assert_eq!(handle.readiness(), ServiceReadiness::Starting);
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(service.run(cancellation.clone()));
    settle().await;
    assert_eq!(handle.readiness(), ServiceReadiness::Ready);

    sender
        .send(pairing("one.example", 1, 11, Some(1), Some("one"))?)
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    assert_eq!(calls_at_least(&connector, 1).await.len(), 1);
    assert_eq!(handle.readiness(), ServiceReadiness::Ready);

    cancellation.cancel();
    let result = task
        .await
        .map_err(|_| DomainError::Invalid { field: "join" })?;
    assert_eq!(result, Ok(()));
    assert!(sender.shutdown_called());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn duplicate_delivery_and_structural_pairings_do_not_connect_twice() -> Result<(), DomainError>
{
    let (source, sender) = FakePairingSource::new(8);
    let connector = bot_test_support::FakeSessionConnector::new();
    let (service, _) = PairingService::new(
        source,
        connector.clone(),
        FakePairingStore::default(),
        config(),
    );
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(service.run(cancellation.clone()));
    let first = pairing("duplicate.example", 2, 22, Some(2), Some("same"))?;
    sender
        .send(first.clone())
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    assert_eq!(calls_at_least(&connector, 1).await.len(), 1);
    sender
        .send(first)
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    sender
        .send(pairing("duplicate.example", 2, 22, Some(2), Some("new"))?)
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    settle().await;
    assert_eq!(connector.connect_calls().await.len(), 1);
    cancellation.cancel();
    assert_eq!(
        task.await
            .map_err(|_| DomainError::Invalid { field: "join" })?,
        Ok(())
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn entity_only_update_reuses_the_unchanged_session() -> Result<(), DomainError> {
    let (source, sender) = FakePairingSource::new(8);
    let connector = bot_test_support::FakeSessionConnector::new();
    let store = FakePairingStore::default();
    let first = pairing("entity.example", 3, 33, Some(3), Some("first"))?;
    let (session, _session_events) =
        bot_test_support::FakeSessionHandle::new(8, SessionReadiness::Ready);
    connector.script(first.server_id().clone(), session).await;
    let (service, _) = PairingService::new(source, connector.clone(), store.clone(), config());
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(service.run(cancellation.clone()));
    sender
        .send(first)
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    assert_eq!(calls_at_least(&connector, 1).await.len(), 1);
    settle().await;
    sender
        .send(pairing("entity.example", 3, 33, Some(4), Some("second"))?)
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    settle().await;
    assert_eq!(connector.connect_calls().await.len(), 1);
    assert_eq!(store.upserts().await.len(), 2);
    cancellation.cancel();
    assert_eq!(
        task.await
            .map_err(|_| DomainError::Invalid { field: "join" })?,
        Ok(())
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn credential_replacement_shuts_down_before_opening_new_session() -> Result<(), DomainError> {
    let (source, sender) = FakePairingSource::new(8);
    let connector = OrderingConnector::new();
    let (service, _) = PairingService::new(
        source,
        connector.clone(),
        FakePairingStore::default(),
        config(),
    );
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(service.run(cancellation.clone()));
    sender
        .send(pairing("replace.example", 4, 44, Some(4), Some("first"))?)
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    assert_eq!(calls_at_least(&connector, 1).await.len(), 1);
    sender
        .send(pairing("replace.example", 5, 55, Some(4), Some("second"))?)
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    assert_eq!(calls_at_least(&connector, 2).await.len(), 2);
    assert_eq!(
        connector.events().await,
        vec!["connect", "shutdown", "connect"]
    );
    cancellation.cancel();
    assert_eq!(
        task.await
            .map_err(|_| DomainError::Invalid { field: "join" })?,
        Ok(())
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn terminal_session_failure_allows_a_later_update_to_connect() -> Result<(), DomainError> {
    let (source, sender) = FakePairingSource::new(8);
    let connector = bot_test_support::FakeSessionConnector::new();
    let (first_handle, first_events) =
        bot_test_support::FakeSessionHandle::new(8, SessionReadiness::Ready);
    connector
        .script(
            pairing("terminal.example", 6, 66, Some(6), None)?
                .server_id()
                .clone(),
            first_handle,
        )
        .await;
    let (service, _) = PairingService::new(
        source,
        connector.clone(),
        FakePairingStore::default(),
        config(),
    );
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(service.run(cancellation.clone()));
    let first = pairing("terminal.example", 6, 66, Some(6), None)?;
    sender
        .send(first)
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    assert_eq!(calls_at_least(&connector, 1).await.len(), 1);
    first_events
        .send(SessionEvent::Terminated {
            error: Some(SessionError::Transport),
        })
        .await
        .map_err(|_| DomainError::Invalid { field: "event" })?;
    settle().await;
    sender
        .send(pairing("terminal.example", 6, 66, Some(7), None)?)
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    assert_eq!(calls_at_least(&connector, 2).await.len(), 2);
    cancellation.cancel();
    assert_eq!(
        task.await
            .map_err(|_| DomainError::Invalid { field: "join" })?,
        Ok(())
    );
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn reconnect_events_retain_the_same_session_id() -> Result<(), DomainError> {
    let (source, sender) = FakePairingSource::new(8);
    let connector = bot_test_support::FakeSessionConnector::new();
    let first = pairing("reconnect.example", 6, 66, Some(6), None)?;
    let (session, events) = bot_test_support::FakeSessionHandle::new(8, SessionReadiness::Ready);
    connector.script(first.server_id().clone(), session).await;
    let (service, handle) = PairingService::new(
        source,
        connector.clone(),
        FakePairingStore::default(),
        config(),
    );
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(service.run(cancellation.clone()));
    sender
        .send(first)
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    assert_eq!(calls_at_least(&connector, 1).await.len(), 1);
    settle().await;
    let ready = handle.sessions();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].state, bot_application::SessionState::Ready);
    let session_id = ready[0].session_id;
    events
        .send(SessionEvent::Reconnecting)
        .await
        .map_err(|_| DomainError::Invalid { field: "event" })?;
    settle().await;
    let reconnecting = handle.sessions();
    assert_eq!(reconnecting[0].session_id, session_id);
    assert_eq!(
        reconnecting[0].state,
        bot_application::SessionState::Reconnecting
    );
    events
        .send(SessionEvent::Ready)
        .await
        .map_err(|_| DomainError::Invalid { field: "event" })?;
    settle().await;
    assert_eq!(handle.sessions()[0].session_id, session_id);
    assert_eq!(
        handle.sessions()[0].state,
        bot_application::SessionState::Ready
    );
    cancellation.cancel();
    assert_eq!(
        task.await
            .map_err(|_| DomainError::Invalid { field: "join" })?,
        Ok(())
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn cancellation_shuts_down_every_session_and_the_source() -> Result<(), DomainError> {
    let (source, sender) = FakePairingSource::new(8);
    let connector = OrderingConnector::new();
    let (service, _) = PairingService::new(
        source,
        connector.clone(),
        FakePairingStore::default(),
        config(),
    );
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(service.run(cancellation.clone()));
    sender
        .send(pairing("cancel-a.example", 7, 77, None, Some("a"))?)
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    sender
        .send(pairing("cancel-b.example", 8, 88, None, Some("b"))?)
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    assert_eq!(calls_at_least(&connector, 2).await.len(), 2);
    cancellation.cancel();
    assert_eq!(
        task.await
            .map_err(|_| DomainError::Invalid { field: "join" })?,
        Ok(())
    );
    assert!(sender.shutdown_called());
    let events = connector.events().await;
    assert_eq!(
        events.iter().filter(|event| **event == "shutdown").count(),
        2
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn source_failure_sets_failed_readiness_and_returns_typed_error() -> Result<(), DomainError> {
    let (source, sender) = FakePairingSource::new(8);
    let (service, handle) = PairingService::new(
        source,
        bot_test_support::FakeSessionConnector::new(),
        FakePairingStore::default(),
        config(),
    );
    let task = tokio::spawn(service.run(CancellationToken::new()));
    sender
        .send_result(Err(PairingSourceError::Receiver("boom".to_owned())))
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    let result = task
        .await
        .map_err(|_| DomainError::Invalid { field: "join" })?;
    assert_eq!(
        result,
        Err(ServiceError::Source(PairingSourceError::Receiver(
            "boom".to_owned()
        )))
    );
    assert_eq!(handle.readiness(), ServiceReadiness::Failed);
    assert!(sender.shutdown_called());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn connect_timeout_removes_connecting_entry_and_drops_connect_task() -> Result<(), DomainError>
{
    let (source, sender) = FakePairingSource::new(8);
    let connector = BlockingConnector::new();
    let (service, _) =
        PairingService::new(source, connector, FakePairingStore::default(), config());
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(service.run(cancellation.clone()));
    sender
        .send(pairing("timeout.example", 9, 99, Some(9), Some("first"))?)
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    settle().await;
    advance(Duration::from_secs(10)).await;
    settle().await;
    sender
        .send(pairing("timeout.example", 9, 99, Some(10), Some("second"))?)
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    settle().await;
    cancellation.cancel();
    let _ = task
        .await
        .map_err(|_| DomainError::Invalid { field: "join" })?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn replacement_shutdown_timeout_clears_registry_entry() -> Result<(), DomainError> {
    let (source, sender) = FakePairingSource::new(8);
    let connector = ReplacementConnector::new();
    let (service, _) = PairingService::new(
        source,
        connector.clone(),
        FakePairingStore::default(),
        config(),
    );
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(service.run(cancellation.clone()));
    sender
        .send(pairing(
            "shutdown-timeout.example",
            10,
            100,
            Some(10),
            Some("first"),
        )?)
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    assert_eq!(calls_at_least(&connector, 1).await.len(), 1);
    sender
        .send(pairing(
            "shutdown-timeout.example",
            11,
            101,
            Some(10),
            Some("second"),
        )?)
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    settle().await;
    advance(Duration::from_secs(2)).await;
    settle().await;
    assert_eq!(calls_at_least(&connector, 2).await.len(), 2);
    cancellation.cancel();
    let _ = task
        .await
        .map_err(|_| DomainError::Invalid { field: "join" })?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn dedupe_capacity_allows_a_later_identical_push() -> Result<(), DomainError> {
    let (source, sender) = FakePairingSource::new(16);
    let connector = bot_test_support::FakeSessionConnector::new();
    let mut test_config = config();
    test_config.dedupe_capacity = 1;
    let first = pairing("dedupe-a.example", 12, 120, Some(12), None)?;
    let (first_handle, first_events) =
        bot_test_support::FakeSessionHandle::new(8, SessionReadiness::Ready);
    connector
        .script(first.server_id().clone(), first_handle)
        .await;
    let (service, _) = PairingService::new(
        source,
        connector.clone(),
        FakePairingStore::default(),
        test_config,
    );
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(service.run(cancellation.clone()));
    sender
        .send(first.clone())
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    assert_eq!(calls_at_least(&connector, 1).await.len(), 1);
    first_events
        .send(SessionEvent::Terminated { error: None })
        .await
        .map_err(|_| DomainError::Invalid { field: "event" })?;
    settle().await;
    sender
        .send(pairing("dedupe-b.example", 13, 130, Some(13), None)?)
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    assert_eq!(calls_at_least(&connector, 2).await.len(), 2);
    sender
        .send(first)
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    assert_eq!(calls_at_least(&connector, 3).await.len(), 3);
    cancellation.cancel();
    assert_eq!(
        task.await
            .map_err(|_| DomainError::Invalid { field: "join" })?,
        Ok(())
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn dedupe_ttl_expiry_allows_a_later_identical_push() -> Result<(), DomainError> {
    let (source, sender) = FakePairingSource::new(16);
    let connector = bot_test_support::FakeSessionConnector::new();
    let mut test_config = config();
    test_config.dedupe_ttl = Duration::from_secs(5);
    let first = pairing("dedupe-ttl.example", 12, 120, Some(12), None)?;
    let (first_handle, first_events) =
        bot_test_support::FakeSessionHandle::new(8, SessionReadiness::Ready);
    connector
        .script(first.server_id().clone(), first_handle)
        .await;
    let (service, _) = PairingService::new(
        source,
        connector.clone(),
        FakePairingStore::default(),
        test_config,
    );
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(service.run(cancellation.clone()));
    sender
        .send(first.clone())
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    assert_eq!(calls_at_least(&connector, 1).await.len(), 1);
    first_events
        .send(SessionEvent::Terminated { error: None })
        .await
        .map_err(|_| DomainError::Invalid { field: "event" })?;
    settle().await;
    advance(Duration::from_secs(6)).await;
    sender
        .send(first)
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    assert_eq!(calls_at_least(&connector, 2).await.len(), 2);
    cancellation.cancel();
    assert_eq!(
        task.await
            .map_err(|_| DomainError::Invalid { field: "join" })?,
        Ok(())
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn max_servers_does_not_open_a_second_session() -> Result<(), DomainError> {
    let (source, sender) = FakePairingSource::new(8);
    let connector = bot_test_support::FakeSessionConnector::new();
    let first = pairing("max-a.example", 14, 140, None, Some("a"))?;
    let (session, _session_events) =
        bot_test_support::FakeSessionHandle::new(8, SessionReadiness::Ready);
    connector.script(first.server_id().clone(), session).await;
    let mut test_config = config();
    test_config.max_servers = 1;
    let (service, _) = PairingService::new(
        source,
        connector.clone(),
        FakePairingStore::default(),
        test_config,
    );
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(service.run(cancellation.clone()));
    sender
        .send(pairing("max-a.example", 14, 140, None, Some("a"))?)
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    assert_eq!(calls_at_least(&connector, 1).await.len(), 1);
    settle().await;
    sender
        .send(pairing("max-b.example", 15, 150, None, Some("b"))?)
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    settle().await;
    assert_eq!(connector.connect_calls().await.len(), 1);
    cancellation.cancel();
    assert_eq!(
        task.await
            .map_err(|_| DomainError::Invalid { field: "join" })?,
        Ok(())
    );
    Ok(())
}

#[test]
fn session_config_validation_rejects_zero_and_unordered_values() {
    let invalid = [
        SessionConfig {
            connect_timeout: Duration::ZERO,
            ..SessionConfig::default()
        },
        SessionConfig {
            shutdown_timeout: Duration::ZERO,
            ..SessionConfig::default()
        },
        SessionConfig {
            connect_timeout: Duration::from_secs(1),
            shutdown_timeout: Duration::from_secs(2),
            ..SessionConfig::default()
        },
        SessionConfig {
            dedupe_capacity: 0,
            ..SessionConfig::default()
        },
        SessionConfig {
            dedupe_ttl: Duration::ZERO,
            ..SessionConfig::default()
        },
        SessionConfig {
            max_servers: 0,
            ..SessionConfig::default()
        },
    ];
    for config in invalid {
        assert!(config.validate().is_err());
    }
}
