//! Process composition root for the Discord bot.

use std::error::Error;

use bot_config::Config;
use bot_db::{Db, DbConfig};
use bot::{Composition, CtrlCSignal};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("bot startup failed: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn Error + Send + Sync>> {
    let config = Config::from_env()?;
    let _telemetry = bot_telemetry::init_from_config(&config)?;
    let database_config = DbConfig::new(config.database_url())?;
    let database = Db::connect(&database_config).await?;
    database.initialize().await?;

    // Database startup is complete before the owned application lifecycle is
    // created. Discord components are registered by their future slice.
    let composition = Composition::new(CtrlCSignal);
    composition.run().await?;
    Ok(())
}
