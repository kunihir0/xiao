#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use tokio::sync::Mutex;

use bot_application::{
    BoxFuture, CompanionRegistrar, CredentialId, CredentialState, DeviceId, EnrollmentError,
    EnrollmentKind, FcmSecrets, OneShotToken, PushCredential, PushCredentialStore,
    RegistrationError, SteamAuthToken, SteamAuthTokenSource, StoreError,
    ensure_companion_registration, reregister_companion,
};
use bot_domain::SteamId;
use bot_test_support::FakePushCredentialStore as MemoryCredentialStore;

#[derive(Default)]
struct RegistrarState {
    creates: AtomicUsize,
    registrations: Mutex<Vec<(String, DeviceId, String)>>,
    failures: Mutex<Vec<RegistrationError>>,
}

#[derive(Clone, Default)]
struct FakeRegistrar(Arc<RegistrarState>);

impl FakeRegistrar {
    async fn fail_next(&self, error: RegistrationError) {
        self.0.failures.lock().await.push(error);
    }
    fn creates(&self) -> usize {
        self.0.creates.load(Ordering::SeqCst)
    }
    async fn registrations(&self) -> Vec<(String, DeviceId, String)> {
        self.0.registrations.lock().await.clone()
    }
}

impl CompanionRegistrar for FakeRegistrar {
    fn create_fcm_credentials<'a>(
        &'a self,
    ) -> BoxFuture<'a, Result<FcmSecrets, RegistrationError>> {
        Box::pin(async move {
            let n = self.0.creates.fetch_add(1, Ordering::SeqCst) as u64 + 1;
            FcmSecrets::new(n, 100 + n, format!("fcm-token-{n}"), [n as u8; 32], [1; 16])
                .map_err(|_| RegistrationError::InvalidResponse("fake"))
        })
    }
    fn register<'a>(
        &'a self,
        auth_token: &'a SteamAuthToken,
        device_id: &'a DeviceId,
        fcm: &'a FcmSecrets,
    ) -> BoxFuture<'a, Result<(), RegistrationError>> {
        Box::pin(async move {
            if let Some(error) = self.0.failures.lock().await.pop() {
                return Err(error);
            }
            self.0.registrations.lock().await.push((
                auth_token.expose_secret().to_owned(),
                device_id.clone(),
                fcm.registration_token().to_owned(),
            ));
            Ok(())
        })
    }
}

#[derive(Clone, Default)]
struct FakeTokens {
    loads: Arc<AtomicUsize>,
    missing: Arc<Mutex<bool>>,
}

impl SteamAuthTokenSource for FakeTokens {
    fn load<'a>(&'a self) -> BoxFuture<'a, Result<SteamAuthToken, RegistrationError>> {
        Box::pin(async move {
            self.loads.fetch_add(1, Ordering::SeqCst);
            if *self.missing.lock().await {
                return Err(RegistrationError::AuthTokenUnavailable("missing"));
            }
            SteamAuthToken::new("steam-token".to_owned())
                .map_err(|_| RegistrationError::AuthTokenUnavailable("invalid"))
        })
    }
}

fn steam() -> SteamId {
    SteamId::new(76_561_198_000_000_001).unwrap()
}

#[tokio::test]
async fn fresh_enrollment_registers_once_then_restores_without_network() {
    let store = MemoryCredentialStore::default();
    let registrar = FakeRegistrar::default();
    let tokens = FakeTokens::default();

    let first = ensure_companion_registration(&store, &registrar, &tokens, steam())
        .await
        .unwrap();
    assert_eq!(first.kind, EnrollmentKind::Registered);
    assert_eq!(first.credential.state, CredentialState::Active);
    assert_eq!(registrar.creates(), 1);
    let registrations = registrar.registrations().await;
    assert_eq!(registrations.len(), 1);
    assert_eq!(registrations[0].0, "steam-token");
    assert_eq!(registrations[0].1, first.credential.device_id);
    assert_eq!(registrations[0].2, "fcm-token-1");

    // Restart: a fresh call with the same store is a pure restore.
    let second = ensure_companion_registration(&store, &registrar, &tokens, steam())
        .await
        .unwrap();
    assert_eq!(second.kind, EnrollmentKind::Restored);
    assert_eq!(second.credential, first.credential);
    assert_eq!(registrar.creates(), 1);
    assert_eq!(registrar.registrations().await.len(), 1);
    assert_eq!(tokens.loads.load(Ordering::SeqCst), 1);
    assert_eq!(store.rows().await.len(), 1);
}

#[tokio::test]
async fn rejected_token_requires_reenrollment_and_resume_reuses_identity() {
    let store = MemoryCredentialStore::default();
    let registrar = FakeRegistrar::default();
    let tokens = FakeTokens::default();
    registrar
        .fail_next(RegistrationError::ReenrollmentRequired)
        .await;

    let error = ensure_companion_registration(&store, &registrar, &tokens, steam())
        .await
        .unwrap_err();
    assert_eq!(
        error,
        EnrollmentError::Registration(RegistrationError::ReenrollmentRequired)
    );
    let rows = store.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, CredentialState::Pending);
    let pending = rows[0].clone();

    // Operator supplies a fresh token; the next start resumes the same
    // pending identity instead of minting a new FCM registration/device.
    let resumed = ensure_companion_registration(&store, &registrar, &tokens, steam())
        .await
        .unwrap();
    assert_eq!(resumed.kind, EnrollmentKind::Resumed);
    assert_eq!(resumed.credential.id, pending.id);
    assert_eq!(resumed.credential.device_id, pending.device_id);
    assert_eq!(registrar.creates(), 1);
    assert_eq!(store.rows().await.len(), 1);
    assert_eq!(store.rows().await[0].state, CredentialState::Active);
}

#[tokio::test]
async fn missing_auth_token_keeps_pending_and_fails_typed() {
    let store = MemoryCredentialStore::default();
    let registrar = FakeRegistrar::default();
    let tokens = FakeTokens::default();
    *tokens.missing.lock().await = true;

    let error = ensure_companion_registration(&store, &registrar, &tokens, steam())
        .await
        .unwrap_err();
    assert_eq!(
        error,
        EnrollmentError::Registration(RegistrationError::AuthTokenUnavailable("missing"))
    );
    assert!(registrar.registrations().await.is_empty());
    assert_eq!(store.rows().await[0].state, CredentialState::Pending);
}

#[tokio::test]
async fn undecryptable_store_fails_closed_without_registration() {
    let store = MemoryCredentialStore::default();
    store.fail_load_active(StoreError::Decryption).await;
    let registrar = FakeRegistrar::default();
    let tokens = FakeTokens::default();

    let error = ensure_companion_registration(&store, &registrar, &tokens, steam())
        .await
        .unwrap_err();
    assert_eq!(error, EnrollmentError::Store(StoreError::Decryption));
    assert_eq!(registrar.creates(), 0);
    assert_eq!(tokens.loads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn retire_removes_local_generation() {
    let store = MemoryCredentialStore::default();
    let registrar = FakeRegistrar::default();
    let tokens = FakeTokens::default();
    let outcome = ensure_companion_registration(&store, &registrar, &tokens, steam())
        .await
        .unwrap();
    store.retire(outcome.credential.id).await.unwrap();
    assert!(store.rows().await.is_empty());
    // Idempotent.
    store.retire(outcome.credential.id).await.unwrap();
}

#[test]
fn errors_never_print_tokens() {
    let error = EnrollmentError::Registration(RegistrationError::ReenrollmentRequired);
    assert!(!error.to_string().contains("steam-token"));
    let credential_debug = format!(
        "{:?}",
        FcmSecrets::new(1, 2, "secret-registration".to_owned(), [9; 32], [8; 16]).unwrap()
    );
    assert!(!credential_debug.contains("secret-registration"));
}

fn active_credential(steam_id: SteamId) -> PushCredential {
    PushCredential {
        id: CredentialId::generate(),
        steam_id,
        device_id: DeviceId::generate(),
        state: CredentialState::Active,
        fcm: FcmSecrets::new(77, 88, "existing-fcm-token".to_owned(), [7; 32], [8; 16]).unwrap(),
    }
}

#[tokio::test]
async fn reregister_reuses_device_and_fcm_identity_without_store_writes() {
    let existing = active_credential(steam());
    let store = MemoryCredentialStore::with_rows(vec![existing.clone()]);
    let registrar = FakeRegistrar::default();
    let tokens = OneShotToken::new(SteamAuthToken::new("fresh-token".to_owned()).unwrap());

    let outcome = reregister_companion(&store, &registrar, &tokens, steam())
        .await
        .unwrap();

    assert_eq!(outcome.kind, EnrollmentKind::Reregistered);
    assert_eq!(outcome.credential, existing);
    assert_eq!(outcome.credential.state, CredentialState::Active);
    let registrations = registrar.registrations().await;
    assert_eq!(
        registrations,
        vec![(
            "fresh-token".to_owned(),
            existing.device_id.clone(),
            "existing-fcm-token".to_owned()
        )]
    );
    // No new FCM material and no store writes.
    assert_eq!(registrar.creates(), 0);
    assert_eq!(store.writes().await, 0);
    assert_eq!(store.rows().await, vec![existing]);
}

#[tokio::test]
async fn reregister_without_active_credential_is_a_typed_error() {
    let registrar = FakeRegistrar::default();
    let tokens = FakeTokens::default();

    // Nothing stored.
    let store = MemoryCredentialStore::default();
    let error = reregister_companion(&store, &registrar, &tokens, steam())
        .await
        .unwrap_err();
    assert_eq!(error, EnrollmentError::NoActiveCredential);

    // A pending generation and a different account's active one do not count.
    let mut pending = active_credential(steam());
    pending.state = CredentialState::Pending;
    let other = active_credential(SteamId::new(76_561_198_000_000_009).unwrap());
    let store = MemoryCredentialStore::with_rows(vec![pending, other]);
    let error = reregister_companion(&store, &registrar, &tokens, steam())
        .await
        .unwrap_err();
    assert_eq!(error, EnrollmentError::NoActiveCredential);

    assert_eq!(tokens.loads.load(Ordering::SeqCst), 0);
    assert!(registrar.registrations().await.is_empty());
    assert_eq!(store.writes().await, 0);
}

#[tokio::test]
async fn reregister_consumes_the_one_shot_token_once() {
    let store = MemoryCredentialStore::with_rows(vec![active_credential(steam())]);
    let registrar = FakeRegistrar::default();
    let tokens = OneShotToken::new(SteamAuthToken::new("fresh-token".to_owned()).unwrap());

    reregister_companion(&store, &registrar, &tokens, steam())
        .await
        .unwrap();
    let error = reregister_companion(&store, &registrar, &tokens, steam())
        .await
        .unwrap_err();

    assert_eq!(
        error,
        EnrollmentError::Registration(RegistrationError::AuthTokenUnavailable("consumed"))
    );
    assert_eq!(registrar.registrations().await.len(), 1);
}

#[tokio::test]
async fn reregister_propagates_registrar_errors_and_leaves_the_store_alone() {
    let existing = active_credential(steam());
    let store = MemoryCredentialStore::with_rows(vec![existing.clone()]);
    let registrar = FakeRegistrar::default();
    registrar
        .fail_next(RegistrationError::ReenrollmentRequired)
        .await;
    let tokens = FakeTokens::default();

    let error = reregister_companion(&store, &registrar, &tokens, steam())
        .await
        .unwrap_err();

    assert_eq!(
        error,
        EnrollmentError::Registration(RegistrationError::ReenrollmentRequired)
    );
    assert_eq!(store.rows().await, vec![existing]);
    assert_eq!(store.writes().await, 0);
}

#[tokio::test]
async fn reregister_propagates_store_errors_before_any_network_call() {
    let store = MemoryCredentialStore::with_rows(vec![active_credential(steam())]);
    store.fail_load_active(StoreError::Decryption).await;
    let registrar = FakeRegistrar::default();
    let tokens = FakeTokens::default();

    let error = reregister_companion(&store, &registrar, &tokens, steam())
        .await
        .unwrap_err();

    assert_eq!(error, EnrollmentError::Store(StoreError::Decryption));
    assert_eq!(tokens.loads.load(Ordering::SeqCst), 0);
    assert!(registrar.registrations().await.is_empty());
}
