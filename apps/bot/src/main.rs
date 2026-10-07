//! Process composition root for the Discord bot.

use std::sync::Arc;

use bot::{BoxError, Composition, CtrlCSignal};
use bot_application::{MemoryPairingStore, PairingService, SessionConfig};
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
        let cancellation = composition.cancellation_token();
        composition.register("rustplus-pairing", async move {
            service
                .run(cancellation)
                .await
                .map_err(|error| Box::new(error) as BoxError)
        })?;
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
