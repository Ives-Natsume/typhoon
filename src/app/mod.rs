pub mod supervisor;
pub mod game;
pub mod task;
pub mod app;

pub use app::App;

use self::{
    game::GameState,
    task::TaskManager,
};
use async_trait::async_trait;
use anyhow::Result;
use std::sync::Arc;

pub struct AppContext {
    pub game_state: Arc<GameState>,
    pub task_manager: Arc<TaskManager>,
}

impl AppContext {
    pub async fn new() -> Result<Self> {
        let game_state = Arc::new(GameState::NotStarted);
        let task_manager = Arc::new(TaskManager::new());

        Ok(Self {
            game_state,
            task_manager,
        })
    }
}

/// A supervised, long-lived module of the system.
///
/// The supervisor only drives lifecycle through this trait; data channels are
/// injected into concrete services at construction time, never through here.
#[async_trait]
pub trait Service: Send {
    /// Human-readable name, used in lifecycle logs.
    fn name(&self) -> &'static str;

    async fn start(&mut self) -> Result<()>;

    async fn stop(&mut self) -> Result<()>;
}