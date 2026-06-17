use super::record::Recorder;
use crate::app::Service;
use anyhow::Result;
use async_trait::async_trait;

/// Owns the screen-capture/record pipeline as a supervised service.
///
/// For now `record` is started unconditionally on `start()` (the game is
/// assumed to be running). Once the event/state plane is wired up, start/stop
/// will be driven by `GameState`.
pub struct CaptureService {
    recorder: Option<Recorder>,
}

impl CaptureService {
    pub fn new() -> Self {
        Self { recorder: None }
    }
}

#[async_trait]
impl Service for CaptureService {
    fn name(&self) -> &'static str {
        "capture"
    }

    async fn start(&mut self) -> Result<()> {
        if self.recorder.is_some() {
            return Ok(());
        }
        self.recorder = Some(Recorder::start().await?);
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        // `Recorder::stop` consumes self, so take ownership out of the Option.
        if let Some(recorder) = self.recorder.take() {
            recorder.stop()?;
        }
        Ok(())
    }
}