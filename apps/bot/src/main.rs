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

async fn run() -> Result<(), BoxError> {
    let config = Config::from_env().map_err(|error| Box::new(error) as BoxError)?;
    let _telemetry = init_from_config(&config).map_err(|error| Box::new(error) as BoxError)?;
    let database_config = DbConfig::new(config.database_url())?;
    let database = Db::connect(&database_config).await?;
    database.initialize().await?;

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
}
