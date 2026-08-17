use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureConfig {
    pub fps: u32,
    pub duration: u32,       // in seconds
    pub output_dir: String,
    /// Capture the target process' audio alongside the video.
    ///
    /// Defaults to `true` so existing config files keep working; set it to
    /// `false` to record video only.
    #[serde(default = "default_audio")]
    pub audio: bool,
}

fn default_audio() -> bool {
    true
}
