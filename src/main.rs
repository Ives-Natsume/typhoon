use typhoon_rs::util::{
    logging,
    config,
};
use typhoon_rs::app::App;
use tracing;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = config::load_config().await.expect("Failed to load config");
    config::GLOBAL_CONFIG.set(std::sync::Arc::new(tokio::sync::RwLock::new(config.clone()))).expect("Failed to set global config");

    let _logging_guard = logging::init_logging(&config.general.log_dir, "typhoon_rs", &config.general.log_level);
    tracing::info!("Typhoon started");

    let capture_dir = &config.capture.output_dir;
    std::fs::create_dir_all(capture_dir).expect("Failed to create capture directory");

    App::new().await?.run().await?;

    Ok(())
}
