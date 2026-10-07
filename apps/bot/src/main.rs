//! Process composition root for the Discord bot.

use std::sync::Arc;

use bot::{BoxError, Composition, CompositionError, CtrlCSignal, ShutdownSignal};
use bot_application::{
    MemoryPairingStore, PairingService, PairingSource, SessionConfig, SessionConnector,
};
use bot_config::Config;
use bot_db::{Db, DbConfig};
use bot_discord::{DiscordConfig, DiscordHandle};
use bot_telemetry::init_from_config;
use push_receiver::{FcmConnector, ProductionConfig};
use rustplus_adapter::{ParseConfig, PushPairingSource, RustPlusSessionConnector};
use rustplus_client::ReconnectPolicy;
use tracing::info;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("bot startup or lifecycle failure: {error}");
        std::process::exit(1);
    }
}

fn register_pairing_service<Signal, Source, Connector>(
    composition: &mut Composition<Signal>,
    enabled: bool,
    service: PairingService<Source, Connector>,
) -> Result<(), CompositionError>
where
    Signal: ShutdownSignal,
    Source: PairingSource + Sync + 'static,
    Connector: SessionConnector + 'static,
{
    if !enabled {
        return Ok(());
    }
    let cancellation = composition.cancellation_token();
    composition.register("rustplus-pairing", async move {
        service
            .run(cancellation)
            .await
            .map_err(|error| Box::new(error) as BoxError)
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BotMode {
    Run { migrate_on_startup: bool },
    MigrateOnly,
}

pub fn parse_bot_mode(
    args: &[String],
    default_migrate_on_startup: bool,
) -> Result<BotMode, String> {
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

async fn run() -> Result<(), BoxError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode =
        parse_bot_mode(&args, true).map_err(Box::<dyn std::error::Error + Send + Sync>::from)?;

    match mode {
        BotMode::MigrateOnly => run_migrations_command().await,
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
            let database = Db::connect(&database_config).await?;
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
                let fcm = Arc::new(FcmConnector::new(rustplus_config.fcm_sender_id())?);
                let receiver_config = ProductionConfig::default();
                let receiver = push_receiver::start_connected(
                    fcm,
                    receiver_config.queue_capacity,
                    receiver_config.dedupe_capacity,
                    receiver_config.retry,
                );
                let source = PushPairingSource::new(receiver, ParseConfig::default());
                let connector = RustPlusSessionConnector::new(ReconnectPolicy::default())?;
                let (service, _service_handle) = PairingService::new(
                    source,
                    connector,
                    MemoryPairingStore::default(),
                    SessionConfig::default(),
                );
                register_pairing_service(&mut composition, true, service)?;
            } else {
                info!("Rust+ pairing disabled: no RUSTPLUS_FCM_SENDER_ID configured");
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
mod tests {
    use super::*;
    use bot::{Readiness, channel_signal};
    use bot_test_support::{FakePairingSource, FakeSessionConnector};

    fn service() -> (
        PairingService<FakePairingSource, FakeSessionConnector>,
        bot_test_support::FakePairingSender,
    ) {
        let (source, sender) = FakePairingSource::new(1);
        let (service, _) = PairingService::new(
            source,
            FakeSessionConnector::new(),
            MemoryPairingStore::default(),
            SessionConfig::default(),
        );
        (service, sender)
    }

    #[tokio::test]
    async fn enabled_pairing_service_is_joined_on_channel_shutdown() -> Result<(), BoxError> {
        let (mut trigger, signal) = channel_signal();
        let mut composition = Composition::new(signal);
        let (service, _sender) = service();
        register_pairing_service(&mut composition, true, service)?;
        let task = tokio::spawn(composition.run());
        trigger.trigger()?;
        let report = task.await??;
        assert_eq!(report.readiness, Readiness::Stopped);
        assert!(report.shutdown_requested);
        assert_eq!(report.joined_tasks, 1);
        Ok(())
    }

    #[tokio::test]
    async fn disabled_pairing_service_is_not_registered() -> Result<(), BoxError> {
        let (mut trigger, signal) = channel_signal();
        let mut composition = Composition::new(signal);
        let (service, _sender) = service();
        register_pairing_service(&mut composition, false, service)?;
        trigger.trigger()?;
        let report = composition.run().await?;
        assert_eq!(report.readiness, Readiness::Stopped);
        assert!(report.shutdown_requested);
        assert_eq!(report.joined_tasks, 0);
        Ok(())
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
}
