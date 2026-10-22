//! Process composition root for the Discord bot.

#[cfg(feature = "live-acceptance")]
use std::path::Path;
#[cfg(feature = "live-acceptance")]
use std::path::PathBuf;
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
#[cfg(feature = "live-acceptance")]
use bot_discord::acceptance::DiscordAcceptanceClient;
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
#[cfg(feature = "live-acceptance")]
use zeroize::Zeroizing;

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
        let Some(legacy) = config.legacy_account() else {
            return Ok(None);
        };
        let steam_id = SteamId::new(legacy.steam_id())?;
        let credentials =
            PostgresPushCredentialStore::new(Arc::clone(&database), Arc::clone(&keys));
        let registrar = FacepunchRegistrar::new(
            config.fcm_sender_id().to_owned(),
            CompanionEndpoints::production(),
        )?;
        let tokens = FileSteamAuthToken::new(legacy.token_file().to_path_buf());
        restore_fcm_connector(
            &credentials,
            &registrar,
            &tokens,
            steam_id,
            config.fcm_sender_id(),
        )
        .await
        .map(Some)
    };
    let fcm = tokio::select! {
        () = cancellation.cancelled() => return Ok(()),
        result = startup => match result? {
            Some(connector) => Arc::new(connector),
            None => {
                info!(
                    "Rust+ companion configured without a legacy account; awaiting /login (not yet available; it will also need LOGIN_PUBLIC_HOST)"
                );
                return Ok(());
            }
        },
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

#[cfg(feature = "live-acceptance")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiscordAcceptanceAction {
    ListGuilds,
    Inspect,
    EnsureGuildPing,
    CleanupGuildPing { command_id: u64 },
    VerifyAll,
}

#[cfg(feature = "live-acceptance")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscordAcceptanceArgs {
    pub guild_id: Option<u64>,
    pub application_id: Option<u64>,
    pub action: DiscordAcceptanceAction,
    pub env_file: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BotMode {
    Run {
        migrate_on_startup: bool,
    },
    MigrateOnly,
    #[cfg(feature = "live-acceptance")]
    DiscordAcceptance(DiscordAcceptanceArgs),
}

#[cfg(feature = "live-acceptance")]
fn parse_discord_acceptance_args(args: &[String]) -> Result<DiscordAcceptanceArgs, String> {
    let mut guild_id = None;
    let mut application_id = None;
    let mut action_kind = "verify-all";
    let mut command_id = None;
    let mut env_file = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--guild-id" => {
                i += 1;
                let val = args.get(i).ok_or("--guild-id requires a value")?;
                guild_id = Some(
                    val.parse::<u64>()
                        .map_err(|e| format!("invalid guild-id: {e}"))?,
                );
            }
            "--application-id" => {
                i += 1;
                let val = args.get(i).ok_or("--application-id requires a value")?;
                application_id = Some(
                    val.parse::<u64>()
                        .map_err(|e| format!("invalid application-id: {e}"))?,
                );
            }
            "--action" => {
                i += 1;
                let val = args.get(i).ok_or("--action requires a value")?;
                action_kind = val.as_str();
            }
            "--command-id" => {
                i += 1;
                let val = args.get(i).ok_or("--command-id requires a value")?;
                command_id = Some(
                    val.parse::<u64>()
                        .map_err(|e| format!("invalid command-id: {e}"))?,
                );
            }
            "--env-file" => {
                i += 1;
                let val = args.get(i).ok_or("--env-file requires a value")?;
                env_file = Some(PathBuf::from(val));
            }
            other => return Err(format!("unknown command line argument: {other}")),
        }
        i += 1;
    }

    let action = match action_kind {
        "list-guilds" => DiscordAcceptanceAction::ListGuilds,
        "inspect" => DiscordAcceptanceAction::Inspect,
        "ensure-guild-ping" => DiscordAcceptanceAction::EnsureGuildPing,
        "cleanup-guild-ping" => {
            let cid = command_id.ok_or("--command-id is required for cleanup-guild-ping action")?;
            DiscordAcceptanceAction::CleanupGuildPing { command_id: cid }
        }
        "verify-all" => DiscordAcceptanceAction::VerifyAll,
        other => {
            return Err(format!(
                "unknown action: {other} (expected list-guilds, inspect, ensure-guild-ping, cleanup-guild-ping, verify-all)"
            ));
        }
    };

    Ok(DiscordAcceptanceArgs {
        guild_id,
        application_id,
        action,
        env_file,
    })
}

pub fn parse_bot_mode(
    args: &[String],
    default_migrate_on_startup: bool,
) -> Result<BotMode, String> {
    #[cfg(feature = "live-acceptance")]
    if let Some(first) = args.first()
        && first == "discord-acceptance"
    {
        return parse_discord_acceptance_args(&args[1..]).map(BotMode::DiscordAcceptance);
    }

    let mut migrate_only = false;
    let mut migrate_on_startup = default_migrate_on_startup;

    for arg in args {
        match arg.as_str() {
            "migrate" | "--migrate" => {
                migrate_only = true;
            }
            "--no-startup-migrations" | "--no-migrate" | "--skip-migrations" => {
                migrate_on_startup = false;
            }
            "--startup-migrations" | "--migrate-on-startup" => {
                migrate_on_startup = true;
            }
            _ => {
                return Err(format!("unknown command line argument: {arg}"));
            }
        }
    }

    if migrate_only {
        Ok(BotMode::MigrateOnly)
    } else {
        Ok(BotMode::Run { migrate_on_startup })
    }
}

fn create_db_config(
    database_url: &str,
    tls_config: &bot_config::DatabaseTlsConfig,
) -> Result<DbConfig, BoxError> {
    match tls_config {
        bot_config::DatabaseTlsConfig::VerifyFull { ca_file } => {
            DbConfig::verify_full(database_url, ca_file).map_err(|e| Box::new(e) as BoxError)
        }
        bot_config::DatabaseTlsConfig::Insecure => {
            DbConfig::insecure_for_local_dev(database_url).map_err(|e| Box::new(e) as BoxError)
        }
    }
}

async fn run_migrations_command() -> Result<(), BoxError> {
    let migration_config =
        bot_config::MigrationConfig::from_env().map_err(|error| Box::new(error) as BoxError)?;
    let db_config = create_db_config(
        migration_config.database_url(),
        migration_config.database_tls(),
    )?;
    let database = Db::connect(&db_config).await?;
    info!("running database migrations");
    let report = database.run_migrations().await?;
    info!(
        applied = report.applied(),
        "database migrations completed successfully"
    );
    database.verify_schema_current().await?;
    info!("verified database schema is current");
    Ok(())
}

#[cfg(feature = "live-acceptance")]
fn resolve_discord_token(env_file_path: Option<&Path>) -> Result<Zeroizing<String>, BoxError> {
    if let Ok(token) = std::env::var("DISCORD_TOKEN")
        && !token.trim().is_empty()
    {
        if env_file_path.is_some() {
            return Err("DISCORD_TOKEN must not be set when --env-file is used".into());
        }
        return Ok(Zeroizing::new(token.trim().to_owned()));
    }
    if let Ok(file_path) = std::env::var("DISCORD_TOKEN_FILE")
        && !file_path.trim().is_empty()
    {
        let secret = bot_config::read_secret_file(file_path.trim(), "DISCORD_TOKEN_FILE")
            .map_err(|e| Box::new(e) as BoxError)?;
        return Ok(secret);
    }
    if let Some(path) = env_file_path {
        let contents = bot_config::read_secret_file(path, "DISCORD_ACCEPTANCE_ENV_FILE")
            .map_err(|e| Box::new(e) as BoxError)?;
        for line in contents.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("DISCORD_TOKEN=") {
                let token = rest.trim().trim_matches('"').trim_matches('\'');
                if !token.is_empty() {
                    return Ok(Zeroizing::new(token.to_owned()));
                }
            }
        }
    }
    Err(
        "DISCORD_TOKEN, DISCORD_TOKEN_FILE, or --env-file containing DISCORD_TOKEN is required"
            .into(),
    )
}

#[cfg(feature = "live-acceptance")]
async fn run_discord_acceptance(args: DiscordAcceptanceArgs) -> Result<(), BoxError> {
    let token = resolve_discord_token(args.env_file.as_deref())?;
    let client = DiscordAcceptanceClient::new(&token);

    match args.action {
        DiscordAcceptanceAction::ListGuilds => {
            let guilds = client.list_guilds().await?;
            println!(
                "{}",
                serde_json::json!({"status":"pass","action":"list_guilds","guilds": guilds.iter().map(|(id,name)| serde_json::json!({"id":id,"name":name})).collect::<Vec<_>>()})
            );
        }
        _ => {
            let guild_id = match args.guild_id {
                Some(id) => id,
                None => {
                    let guilds = client.list_guilds().await?;
                    match guilds.as_slice() {
                        [(id, _)] => *id,
                        [] => return Err("bot is not a member of any guilds".into()),
                        _ => {
                            return Err(
                                "bot is a member of multiple guilds; please specify --guild-id"
                                    .into(),
                            );
                        }
                    }
                }
            };

            match args.action {
                DiscordAcceptanceAction::ListGuilds => unreachable!(),
                DiscordAcceptanceAction::Inspect => {
                    let identity = client
                        .verify_identity(args.application_id, guild_id)
                        .await?;
                    let global_ping = client.count_global_ping().await?;
                    let guild_ping = client.count_guild_ping(guild_id).await?;
                    println!(
                        "{}",
                        serde_json::json!({
                            "status":"pass","action":"inspect",
                            "application_id":identity.application_id,
                            "bot_username":identity.bot_username,
                            "bot_user_id":identity.bot_user_id,
                            "guild_id":identity.guild_id,
                            "guild_name":identity.guild_name,
                            "global_ping_count":global_ping,
                            "guild_ping_count":guild_ping
                        })
                    );
                }
                DiscordAcceptanceAction::EnsureGuildPing => {
                    let (command_id, created) = client.ensure_guild_ping(guild_id).await?;
                    println!(
                        "{}",
                        serde_json::json!({"status":"pass","action":"ensure_guild_ping","guild_id":guild_id,"command_id":command_id,"created":created})
                    );
                }
                DiscordAcceptanceAction::CleanupGuildPing { command_id } => {
                    client
                        .verify_guild_ping_command(guild_id, command_id)
                        .await?;
                    client.delete_guild_command(guild_id, command_id).await?;
                    println!(
                        "{}",
                        serde_json::json!({"status":"pass","action":"cleanup_guild_ping","guild_id":guild_id,"command_id":command_id})
                    );
                }
                DiscordAcceptanceAction::VerifyAll => {
                    let identity = client
                        .verify_identity(args.application_id, guild_id)
                        .await?;
                    client.verify_nonexistent_command_error().await?;
                    let global_ping = client.count_global_ping().await?;
                    let (command_id, created) = client.ensure_guild_ping(guild_id).await?;
                    let guild_ping = client.count_guild_ping(guild_id).await?;
                    println!(
                        "{}",
                        serde_json::json!({
                            "status":"pass","action":"verify_all",
                            "application_id":identity.application_id,
                            "bot_username":identity.bot_username,
                            "bot_user_id":identity.bot_user_id,
                            "guild_id":identity.guild_id,
                            "guild_name":identity.guild_name,
                            "global_ping_count":global_ping,
                            "guild_ping_count":guild_ping,
                            "guild_ping_id":command_id,
                            "guild_ping_created":created
                        })
                    );
                }
            }
        }
    }
    Ok(())
}

async fn run() -> Result<(), BoxError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode =
        parse_bot_mode(&args, true).map_err(Box::<dyn std::error::Error + Send + Sync>::from)?;

    match mode {
        BotMode::MigrateOnly => run_migrations_command().await,
        #[cfg(feature = "live-acceptance")]
        BotMode::DiscordAcceptance(args) => run_discord_acceptance(args).await,
        BotMode::Run {
            migrate_on_startup: requested_migrate_on_startup,
        } => {
            let config = Config::from_env().map_err(|error| Box::new(error) as BoxError)?;
            let _telemetry =
                init_from_config(&config).map_err(|error| Box::new(error) as BoxError)?;
            let migrate_on_startup = if requested_migrate_on_startup {
                config.database_migrate_on_startup()
            } else {
                false
            };
            let database_config = create_db_config(config.database_url(), config.database_tls())?;
            let database = Arc::new(Db::connect(&database_config).await?);
            if migrate_on_startup {
                info!("startup migrations enabled: initializing database");
                let report = database.initialize().await?;
                info!(
                    applied = report.applied(),
                    "startup database initialization complete"
                );
            } else {
                info!("startup migrations disabled: verifying schema is current");
                database.verify_and_ping().await?;
                info!("database schema verified current and readiness ping succeeded");
            }

            // Database startup is complete before Discord is constructed.
            let mut composition = Composition::new(CtrlCSignal);
            if let Some(rustplus_config) = config.rustplus() {
                // Fail closed before starting components when the keyring is
                // absent, malformed, or not owner-only.
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
            let discord =
                DiscordHandle::new(DiscordConfig::new(config.discord_token().to_owned())?);
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
    }
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

    #[test]
    fn parse_bot_mode_defaults_to_config_setting() -> Result<(), String> {
        assert_eq!(
            parse_bot_mode(&[], true)?,
            BotMode::Run {
                migrate_on_startup: true
            }
        );
        assert_eq!(
            parse_bot_mode(&[], false)?,
            BotMode::Run {
                migrate_on_startup: false
            }
        );
        Ok(())
    }

    #[test]
    fn parse_bot_mode_migrate_subcommand_selects_migrate_only() -> Result<(), String> {
        for arg in ["migrate", "--migrate"] {
            assert_eq!(
                parse_bot_mode(&[arg.to_owned()], true)?,
                BotMode::MigrateOnly
            );
            assert_eq!(
                parse_bot_mode(&[arg.to_owned()], false)?,
                BotMode::MigrateOnly
            );
        }
        Ok(())
    }

    #[test]
    fn parse_bot_mode_startup_migration_flags_override_default() -> Result<(), String> {
        for arg in [
            "--no-startup-migrations",
            "--no-migrate",
            "--skip-migrations",
        ] {
            assert_eq!(
                parse_bot_mode(&[arg.to_owned()], true)?,
                BotMode::Run {
                    migrate_on_startup: false
                }
            );
        }
        for arg in ["--startup-migrations", "--migrate-on-startup"] {
            assert_eq!(
                parse_bot_mode(&[arg.to_owned()], false)?,
                BotMode::Run {
                    migrate_on_startup: true
                }
            );
        }
        Ok(())
    }

    #[test]
    fn parse_bot_mode_rejects_unknown_argument() {
        let result = parse_bot_mode(&["--invalid-arg".to_owned()], true);
        assert!(matches!(&result, Err(msg) if msg.contains("unknown command line argument")));
    }

    #[cfg(feature = "live-acceptance")]
    #[test]
    fn parse_bot_mode_discord_acceptance_valid() -> Result<(), String> {
        let args = vec![
            "discord-acceptance".to_owned(),
            "--guild-id".to_owned(),
            "123456789012345678".to_owned(),
            "--application-id".to_owned(),
            "987654321098765432".to_owned(),
            "--action".to_owned(),
            "inspect".to_owned(),
        ];
        let mode = parse_bot_mode(&args, true)?;
        assert_eq!(
            mode,
            BotMode::DiscordAcceptance(DiscordAcceptanceArgs {
                guild_id: Some(123456789012345678),
                application_id: Some(987654321098765432),
                action: DiscordAcceptanceAction::Inspect,
                env_file: None,
            })
        );
        Ok(())
    }

    #[cfg(feature = "live-acceptance")]
    #[test]
    fn parse_bot_mode_discord_acceptance_optional_guild_id() -> Result<(), String> {
        let args = vec![
            "discord-acceptance".to_owned(),
            "--action".to_owned(),
            "list-guilds".to_owned(),
        ];
        let mode = parse_bot_mode(&args, true)?;
        assert_eq!(
            mode,
            BotMode::DiscordAcceptance(DiscordAcceptanceArgs {
                guild_id: None,
                application_id: None,
                action: DiscordAcceptanceAction::ListGuilds,
                env_file: None,
            })
        );
        Ok(())
    }
    #[cfg(feature = "live-acceptance")]
    #[test]
    fn parse_bot_mode_discord_acceptance_cleanup_action() -> Result<(), String> {
        let args = vec![
            "discord-acceptance".to_owned(),
            "--guild-id".to_owned(),
            "100".to_owned(),
            "--action".to_owned(),
            "cleanup-guild-ping".to_owned(),
            "--command-id".to_owned(),
            "200".to_owned(),
        ];
        let mode = parse_bot_mode(&args, true)?;
        assert_eq!(
            mode,
            BotMode::DiscordAcceptance(DiscordAcceptanceArgs {
                guild_id: Some(100),
                application_id: None,
                action: DiscordAcceptanceAction::CleanupGuildPing { command_id: 200 },
                env_file: None,
            })
        );
        Ok(())
    }

    #[test]
    fn acceptance_json_serializes_control_characters() {
        let value = serde_json::json!({"guild_name":"quote\" newline\n control\u{0001}"});
        let encoded = value.to_string();
        assert!(encoded.contains("\\\""));
        assert!(encoded.contains("\\n"));
        assert!(encoded.contains("\\u0001"));
        assert!(!encoded.contains('\u{0001}'));
    }
}
