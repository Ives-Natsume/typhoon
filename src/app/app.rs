use super::supervisor::Supervisor;
use super::AppContext;
use crate::backend::capture::service::CaptureService;
use anyhow::Result;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};

pub struct App {
    supervisor: Supervisor,
}

impl App {
    pub async fn new() -> Result<Self> {
        let context = Arc::new(AppContext::new().await?);

        let mut supervisor = Supervisor::new(context);
        supervisor.register(CaptureService::new());

        Ok(Self { supervisor })
    }

    /// Run the full lifecycle: start every service, block until a shutdown
    /// signal (Enter or Ctrl-C), then stop every service in reverse order.
    pub async fn run(mut self) -> Result<()> {
        self.supervisor.start_all().await?;
        tracing::info!("All services started. Press Enter (or Ctrl-C) to stop.");

        wait_for_shutdown().await;

        tracing::info!("Shutdown signal received, stopping services...");
        self.supervisor.stop_all().await?;
        tracing::info!("All services stopped.");
        Ok(())
    }
}

/// Resolve when the user presses Enter or sends Ctrl-C.
async fn wait_for_shutdown() {
    let enter = async {
        let mut line = String::new();
        let mut reader = BufReader::new(tokio::io::stdin());
        let _ = reader.read_line(&mut line).await;
    };

    tokio::select! {
        _ = enter => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}