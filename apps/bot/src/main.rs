//! Process composition root for the Discord bot.

use bot::{BoxError, Composition, CtrlCSignal};
use bot_config::Config;
use bot_db::{Db, DbConfig};
use bot_discord::{DiscordConfig, DiscordHandle};
use bot_telemetry::init_from_config;

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
