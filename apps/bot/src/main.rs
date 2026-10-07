//! Process composition root for the Discord bot.

use std::sync::Arc;

use bot::{BoxError, Composition, CtrlCSignal};
use bot_application::{
    CompanionRegistrar, PairingService, PushCredentialStore, SessionConfig, SteamAuthTokenSource,
    ensure_companion_registration,
};
use bot_config::{Config, RustPlusConfig};
use bot_db::{
    Db, DbConfig, FileKeyring, KeyProvider, PostgresPairingStore, PostgresPushCredentialStore,
    rewrap_secrets,
};
use bot_discord::{DiscordConfig, DiscordHandle};
use bot_domain::SteamId;
use bot_telemetry::init_from_config;
use push_receiver::{FcmConnector, ProductionConfig};
use rustplus_adapter::{
    CompanionEndpoints, FacepunchRegistrar, FileSteamAuthToken, ParseConfig, PushPairingSource,
    RustPlusSessionConnector, snapshot_from_fcm_secrets,
};
use rustplus_client::ReconnectPolicy;
use tokio_util::sync::CancellationToken;
use tracing::info;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("bot startup or lifecycle failure: {error}");
        std::process::exit(1);
    }
}

/// Restores the active companion registration (no network) or completes
/// enrollment, then builds an FCM connector from the persisted snapshot so
/// Google check-in/register is never repeated for a restored credential.
async fn restore_fcm_connector(
    credentials: &dyn PushCredentialStore,
    registrar: &dyn CompanionRegistrar,
    tokens: &dyn SteamAuthTokenSource,
    steam_id: SteamId,
    fcm_sender_id: &str,
) -> Result<FcmConnector, BoxError> {
    let outcome = ensure_companion_registration(credentials, registrar, tokens, steam_id).await?;
    info!(
        kind = ?outcome.kind,
        credential = %outcome.credential.id,
        "Rust+ companion credentials ready"
    );
    let snapshot = snapshot_from_fcm_secrets(&outcome.credential.fcm)?;
    Ok(FcmConnector::from_snapshot(fcm_sender_id, snapshot)?)
}

/// Owns the whole Rust+ lifecycle: secret rewrap, credential restore or
/// enrollment, then the pairing service. Cancellation is honoured during
/// startup as well as while running.
async fn run_rustplus(
    database: Arc<Db>,
    keys: Arc<dyn KeyProvider>,
    config: RustPlusConfig,
    cancellation: CancellationToken,
) -> Result<(), BoxError> {
    let startup = async {
        let rewrapped = rewrap_secrets(&database, keys.as_ref()).await?;
        info!(
            credentials = rewrapped.credentials,
            pairings = rewrapped.pairings,
            "secret data keys wrapped with the active key"
        );
        let steam_id = SteamId::new(config.steam_id())?;
        let credentials =
            PostgresPushCredentialStore::new(Arc::clone(&database), Arc::clone(&keys));
        let registrar = FacepunchRegistrar::new(
            config.fcm_sender_id().to_owned(),
            CompanionEndpoints::production(),
        )?;
        let tokens = FileSteamAuthToken::new(config.steam_auth_token_file().to_path_buf());
        restore_fcm_connector(
            &credentials,
            &registrar,
            &tokens,
            steam_id,
            config.fcm_sender_id(),
        )
        .await
    };
    let fcm = tokio::select! {
        () = cancellation.cancelled() => return Ok(()),
        result = startup => Arc::new(result?),
    };
    let receiver_config = ProductionConfig::default();
    let receiver = push_receiver::start_connected(
        fcm,
        receiver_config.queue_capacity,
        receiver_config.dedupe_capacity,
        receiver_config.retry,
    );
    let source = PushPairingSource::new(receiver, ParseConfig::default());
    let connector = RustPlusSessionConnector::new(ReconnectPolicy::default())?;
    let store = PostgresPairingStore::new(database, keys);
    let (service, _service_handle) =
        PairingService::new(source, connector, store, SessionConfig::default());
    service
        .run(cancellation)
        .await
        .map_err(|error| Box::new(error) as BoxError)
}

async fn run() -> Result<(), BoxError> {
    let config = Config::from_env().map_err(|error| Box::new(error) as BoxError)?;
    let _telemetry = init_from_config(&config).map_err(|error| Box::new(error) as BoxError)?;
    let database_config = DbConfig::new(config.database_url())?;
    let database = Arc::new(Db::connect(&database_config).await?);
    database.initialize().await?;

    // Database startup is complete before Discord is constructed.
    let mut composition = Composition::new(CtrlCSignal);
    if let Some(rustplus_config) = config.rustplus() {
        // Fail closed before any component starts when the keyring is absent,
        // malformed, or readable by other users.
        let keys: Arc<dyn KeyProvider> = Arc::new(FileKeyring::load(
            rustplus_config.keyring_file(),
            rustplus_config.active_key_id(),
        )?);
        let cancellation = composition.cancellation_token();
        composition.register(
            "rustplus-pairing",
            run_rustplus(
                Arc::clone(&database),
                keys,
                rustplus_config.clone(),
                cancellation,
            ),
        )?;
    } else {
        info!("Rust+ pairing disabled: no Rust+ companion configuration");
    }
    let discord = DiscordHandle::new(DiscordConfig::new(config.discord_token().to_owned())?);
    let cancellation = composition.cancellation_token();

    composition.register("discord", async move {
        discord
            .run(cancellation)
            .await
            .map_err(|error| Box::new(error) as BoxError)
    })?;
    composition
        .run()
        .await
        .map(|_| ())
        .map_err(|error| Box::new(error) as BoxError)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use bot_application::{
        BoxFuture, CredentialId, CredentialState, DeviceId, FcmSecrets, PushCredential,
        RegistrationError, SteamAuthToken, StoreError,
    };

    use super::*;

    struct ActiveOnlyStore(PushCredential);

    impl PushCredentialStore for ActiveOnlyStore {
        fn load_active<'a>(
            &'a self,
            _steam_id: SteamId,
        ) -> BoxFuture<'a, Result<Option<PushCredential>, StoreError>> {
            Box::pin(async move { Ok(Some(self.0.clone())) })
        }
        fn load_pending<'a>(
            &'a self,
            _steam_id: SteamId,
        ) -> BoxFuture<'a, Result<Option<PushCredential>, StoreError>> {
            Box::pin(async { Err(StoreError::Unavailable) })
        }
        fn save_pending<'a>(
            &'a self,
            _credential: &'a PushCredential,
        ) -> BoxFuture<'a, Result<(), StoreError>> {
            Box::pin(async { Err(StoreError::Unavailable) })
        }
        fn promote_active<'a>(
            &'a self,
            _id: CredentialId,
        ) -> BoxFuture<'a, Result<(), StoreError>> {
            Box::pin(async { Err(StoreError::Unavailable) })
        }
        fn retire<'a>(&'a self, _id: CredentialId) -> BoxFuture<'a, Result<(), StoreError>> {
            Box::pin(async { Err(StoreError::Unavailable) })
        }
    }

    #[derive(Default)]
    struct CountingNetwork(AtomicUsize);

    impl CompanionRegistrar for CountingNetwork {
        fn create_fcm_credentials<'a>(
            &'a self,
        ) -> BoxFuture<'a, Result<FcmSecrets, RegistrationError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(RegistrationError::Transport("test")) })
        }
        fn register<'a>(
            &'a self,
            _auth_token: &'a SteamAuthToken,
            _device_id: &'a DeviceId,
            _fcm: &'a FcmSecrets,
        ) -> BoxFuture<'a, Result<(), RegistrationError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(RegistrationError::Transport("test")) })
        }
    }

    impl SteamAuthTokenSource for CountingNetwork {
        fn load<'a>(&'a self) -> BoxFuture<'a, Result<SteamAuthToken, RegistrationError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(RegistrationError::AuthTokenUnavailable("test")) })
        }
    }

    #[tokio::test]
    async fn active_credential_restores_connector_without_any_registration() {
        let steam_id = SteamId::new(76_561_198_000_000_001).unwrap();
        let fcm = FcmSecrets::new(41, 42, "restored-token".to_owned(), [5; 32], [6; 16]).unwrap();
        let store = ActiveOnlyStore(PushCredential {
            id: CredentialId::generate(),
            steam_id,
            device_id: DeviceId::generate(),
            state: CredentialState::Active,
            fcm,
        });
        let network = CountingNetwork::default();

        let connector = restore_fcm_connector(&store, &network, &network, steam_id, "976529667804")
            .await
            .unwrap();

        assert_eq!(network.0.load(Ordering::SeqCst), 0);
        let snapshot = connector.snapshot().await.unwrap();
        assert_eq!(snapshot.checkin.android_id(), 41);
        assert_eq!(snapshot.checkin.security_token(), 42);
        assert_eq!(snapshot.registration_token.as_str(), "restored-token");
        assert_eq!(snapshot.keys.private_key_bytes(), Some([5; 32]));
        assert_eq!(snapshot.keys.auth_secret_bytes(), Some([6; 16]));
        assert!(!format!("{connector:?}").contains("restored-token"));
    }
}
