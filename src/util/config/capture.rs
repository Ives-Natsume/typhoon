use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureConfig {
    pub fps: u32,
    pub duration: u32,       // in seconds
    pub output_dir: String,
}
