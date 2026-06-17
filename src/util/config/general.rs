use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    // app config
    pub log_level: String,
    pub log_dir: String,
    pub lifetime: u32,     // in days

    // capture config
    pub capture_target_window_title: String,
}