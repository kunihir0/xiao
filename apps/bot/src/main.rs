//! Process composition root for the Discord bot.

use bot_config::Config;
use bot_telemetry::init_from_config;
use bot::{BoxError, Composition, CtrlCSignal};

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

    let composition = Composition::new(CtrlCSignal);
    composition
        .run()
        .await
        .map(|_| ())
        .map_err(|error| Box::new(error) as BoxError)
}
