use std::time::Duration;

use bot_application::{
    PairingService, ServiceError, ServiceReadiness, SessionConfig, SessionError, SessionReadiness,
    SessionState, SourceReadiness, StoreError,
};
use bot_domain::{
    DomainError, Pairing, PlayerCredentials, PlayerId, PlayerToken, ServerAddress, ServerId,
};
use bot_test_support::{
    FakePairingSource, FakePairingStore, FakeSessionConnector, FakeSessionHandle, FakeSessionSender,
};
use tokio_util::sync::CancellationToken;

fn pairing(host: &str, token: i32) -> Result<Pairing, DomainError> {
    Ok(Pairing::new(
        ServerAddress::new(host, 28_082)?,
        PlayerCredentials::new(PlayerId::new(7)?, PlayerToken::new(token)),
        None,
        None,
    ))
}

fn config(max_servers: usize) -> SessionConfig {
    SessionConfig {
        connect_timeout: Duration::from_secs(10),
        shutdown_timeout: Duration::from_secs(2),
        dedupe_capacity: 8,
        dedupe_ttl: Duration::from_secs(30),
        max_servers,
    }
}

async fn settle() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

/// Scripts a live session whose sender is kept alive by the caller.
async fn live(connector: &FakeSessionConnector, server: &ServerId) -> FakeSessionSender {
    let (handle, sender) = FakeSessionHandle::new(8, SessionReadiness::Ready);
    connector.script(server.clone(), handle).await;
    sender
}

fn join_error(_: tokio::task::JoinError) -> DomainError {
    DomainError::Invalid { field: "join" }
}

#[tokio::test(start_paused = true)]
async fn persisted_pairings_are_restored_without_rewriting_the_store() -> Result<(), DomainError> {
    let one = pairing("one.example", 11)?;
    let two = pairing("two.example", 22)?;
    let store = FakePairingStore::with_rows(vec![one.clone(), two.clone()]);
    let (source, _sender) = FakePairingSource::new(8);
    let connector = FakeSessionConnector::new();
    let _one = live(&connector, one.server_id()).await;
    let _two = live(&connector, two.server_id()).await;
    let (service, handle) =
        PairingService::new(source, connector.clone(), store.clone(), config(8));
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(service.run(cancellation.clone()));
    settle().await;

    let mut calls = connector.connect_calls().await;
    calls.sort();
    let mut expected = vec![one.server_id().clone(), two.server_id().clone()];
    expected.sort();
    assert_eq!(calls, expected);
    assert!(store.upserts().await.is_empty(), "restore must not rewrite");
    assert_eq!(handle.readiness(), ServiceReadiness::Ready);
    let sessions = handle.sessions();
    assert_eq!(sessions.len(), 2);
    assert!(sessions.iter().all(|s| s.state == SessionState::Ready));

    cancellation.cancel();
    assert_eq!(task.await.map_err(join_error)?, Ok(()));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn one_failing_restored_server_does_not_block_readiness() -> Result<(), DomainError> {
    let good = pairing("good.example", 1)?;
    let bad = pairing("bad.example", 2)?;
    let store = FakePairingStore::with_rows(vec![good.clone(), bad.clone()]);
    let (source, _sender) = FakePairingSource::new(8);
    let connector = FakeSessionConnector::new();
    let _good = live(&connector, good.server_id()).await;
    connector
        .fail(bad.server_id().clone(), SessionError::InvalidCredentials)
        .await;
    let (service, handle) = PairingService::new(source, connector, store, config(8));
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(service.run(cancellation.clone()));
    settle().await;

    assert_eq!(handle.readiness(), ServiceReadiness::Ready);
    let state_of = |server: &ServerId| {
        handle
            .sessions()
            .into_iter()
            .find(|snapshot| &snapshot.server_id == server)
            .map(|snapshot| snapshot.state)
    };
    assert_eq!(state_of(good.server_id()), Some(SessionState::Ready));
    assert_eq!(
        state_of(bad.server_id()),
        Some(SessionState::Failed {
            reason: SessionError::InvalidCredentials
        })
    );

    cancellation.cancel();
    assert_eq!(task.await.map_err(join_error)?, Ok(()));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn restore_is_attempted_before_ready_and_waits_for_source() -> Result<(), DomainError> {
    let one = pairing("one.example", 1)?;
    let store = FakePairingStore::with_rows(vec![one.clone()]);
    let (source, sender) = FakePairingSource::new(8);
    let source = source.with_readiness(SourceReadiness::Starting);
    let connector = FakeSessionConnector::new();
    let (service, handle) = PairingService::new(source, connector.clone(), store, config(8));
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(service.run(cancellation.clone()));
    settle().await;

    assert_eq!(
        connector.connect_calls().await,
        vec![one.server_id().clone()]
    );
    assert_eq!(handle.readiness(), ServiceReadiness::Starting);
    sender.set_readiness(SourceReadiness::Ready);
    settle().await;
    assert_eq!(handle.readiness(), ServiceReadiness::Ready);

    cancellation.cancel();
    assert_eq!(task.await.map_err(join_error)?, Ok(()));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn redelivered_restored_pairing_does_not_reconnect() -> Result<(), DomainError> {
    let one = pairing("one.example", 1)?;
    let store = FakePairingStore::with_rows(vec![one.clone()]);
    let (source, sender) = FakePairingSource::new(8);
    let connector = FakeSessionConnector::new();
    let (service, _handle) = PairingService::new(source, connector.clone(), store, config(8));
    let cancellation = CancellationToken::new();
    let task = tokio::spawn(service.run(cancellation.clone()));
    settle().await;
    sender
        .send(one.clone())
        .await
        .map_err(|_| DomainError::Invalid { field: "sender" })?;
    settle().await;
    assert_eq!(connector.connect_calls().await.len(), 1);

    cancellation.cancel();
    assert_eq!(task.await.map_err(join_error)?, Ok(()));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn restore_failures_are_typed_and_never_ready() -> Result<(), DomainError> {
    for error in [StoreError::Decryption, StoreError::SecretStorageUnavailable] {
        let store = FakePairingStore::default();
        store.fail_load(error.clone()).await;
        let (source, _sender) = FakePairingSource::new(8);
        let connector = FakeSessionConnector::new();
        let (service, handle) = PairingService::new(source, connector.clone(), store, config(8));
        let result = service.run(CancellationToken::new()).await;
        assert_eq!(result, Err(ServiceError::Restore(error)));
        assert_eq!(handle.readiness(), ServiceReadiness::Failed);
        assert!(connector.connect_calls().await.is_empty());
    }

    let rows = vec![pairing("one.example", 1)?, pairing("two.example", 2)?];
    let (source, _sender) = FakePairingSource::new(8);
    let connector = FakeSessionConnector::new();
    let (service, handle) = PairingService::new(
        source,
        connector.clone(),
        FakePairingStore::with_rows(rows),
        config(1),
    );
    let result = service.run(CancellationToken::new()).await;
    assert!(matches!(
        result,
        Err(ServiceError::Restore(StoreError::Invalid(_)))
    ));
    assert_eq!(handle.readiness(), ServiceReadiness::Failed);
    assert!(connector.connect_calls().await.is_empty());
    Ok(())
}
