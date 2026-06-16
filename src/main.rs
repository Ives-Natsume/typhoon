use typhoon_rs::utils::{
    logging,
    config,
};
use typhoon_rs::backend::capture;
use tracing;

#[tokio::main]
async fn main() {
    let config = config::load_config().await.expect("Failed to load config");
    config::GLOBAL_CONFIG.set(std::sync::Arc::new(tokio::sync::RwLock::new(config.clone()))).expect("Failed to set global config");

    let _logging_guard = logging::init_logging(&config.general.log_dir, "typhoon_rs", &config.general.log_level);
    tracing::info!("Typhoon started");

    let title = &config.general.capture_target_window_title;
    let capture_dir = &config.capture.output_dir;
    std::fs::create_dir_all(capture_dir).expect("Failed to create capture directory");
    
    match capture::window::window_detect(title) {
        Ok(window) => {
            tracing::info!("Successfully detected window for title '{}'", title);
            if let Err(e) = capture::window::window_capture(&window, capture_dir).await {
                tracing::error!("Failed to capture window for title '{}': {}", title, e);
            } else {
                tracing::info!("Successfully captured window for title '{}'", title);
            }
        },
        Err(e) => {
            tracing::error!("Failed to detect window for title '{}': {}", title, e);
        },
    }
}
