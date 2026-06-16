pub mod capture;
pub mod general;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use capture::CaptureConfig;
use general::GeneralConfig;
use tokio::fs;
use std::sync::Arc;
use tokio::sync::{OnceCell, RwLock};

pub static GLOBAL_CONFIG: OnceCell<Arc<RwLock<Config>>> = OnceCell::const_new();

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub general: GeneralConfig,
    pub capture: CaptureConfig,
}

pub async fn load_config() -> Result<Config> {
    let config_str = fs::read_to_string("config.toml").await?;
    let config: Config = toml::from_str(&config_str)?;
    Ok(config)
}

pub async fn read_config() -> tokio::sync::RwLockReadGuard<'static, Config> {
    GLOBAL_CONFIG.get().expect("Config not initialized").read().await
}

pub async fn write_config() -> tokio::sync::RwLockWriteGuard<'static, Config> {
    GLOBAL_CONFIG.get().expect("Config not initialized").write().await
}