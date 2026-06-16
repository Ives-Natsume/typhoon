use std::{path::Path, time::Duration};
use anyhow::Result;
use windows_capture::{
    capture::{Context, GraphicsCaptureApiHandler},
    encoder::ImageFormat,
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    window::Window,
    settings::{
        ColorFormat, CursorCaptureSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
        DirtyRegionSettings
    },
};
use crate::utils::config;

/// Window information for display purposes
pub struct WindowHandler {
    filename: String,
    saved: bool,
}

impl GraphicsCaptureApiHandler for WindowHandler {
    type Flags = String; 
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(WindowHandler {
            filename: ctx.flags,
            saved: false,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if !self.saved {
            frame.save_as_image(&self.filename, ImageFormat::Png)?;
            tracing::info!("Screenshot saved: {}", self.filename);
            self.saved = true;

            capture_control.stop();
        }

        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        tracing::info!("Captured window has been closed.");
        Ok(())
    }
}

pub fn window_detect(
    process_name: &str,
) -> Result<Window> {
    let target_window = Window::from_contains_name(process_name);
    match target_window {
        Ok(window) => Ok(window),
        Err(_) => Err(anyhow::anyhow!("No target window found")),
    }
}

pub async fn window_capture(window: &Window, output_dir: &str) -> Result<()> {
    let filename = format!("screenshot_{}.png", chrono::Local::now().format("%Y%m%d_%H%M%S"));
    let full_path = Path::new(output_dir).join(&filename);
    let full_path_str = full_path.to_string_lossy().to_string();

    let fps = config::read_config().await.capture.fps;
    let duration_nanos = Duration::from_nanos(1_000_000_000 / fps as u64);

    let settings = Settings::new(
        *window,
        // Default cursor capture settings (capture the cursor)
        CursorCaptureSettings::Default,
        // Default draw border settings (do not draw borders)
        DrawBorderSettings::Default,
        // Default secondary window settings (capture only the primary window)
        SecondaryWindowSettings::Default,
        // Default minimum update interval settings
        MinimumUpdateIntervalSettings::Custom(duration_nanos),
        // Default dirty region settings (capture the entire window)
        DirtyRegionSettings::Default,
        // RGBA8 color format
        ColorFormat::Rgba8,
        full_path_str,
    );

    WindowHandler::start(settings)?;

    Ok(())
}

fn _sanitize_filename(name: &str) -> String {
    name.replace(['|', '\\', ':', '/', '*', '?', '"', '<', '>'], "_")
}

fn _match_keyword(keyword: &str, win_title: &str, app_name: &str) -> bool {
    let keyword = keyword.to_lowercase();
    win_title.to_lowercase().contains(&keyword) || app_name.to_lowercase().contains(&keyword)
}