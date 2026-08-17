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
use tokio::sync::watch;

pub struct AppContext {
    /// Single source of truth for the high-level game state. Perception
    /// services publish transitions here; consumers take their own receiver via
    /// [`AppContext::subscribe_state`] and react on change.
    state_tx: watch::Sender<GameState>,
    pub task_manager: Arc<TaskManager>,
}

impl AppContext {
    pub async fn new() -> Result<Self> {
        let (state_tx, _state_rx) = watch::channel(GameState::default());
        let task_manager = Arc::new(TaskManager::new());

        Ok(Self {
            state_tx,
            task_manager,
        })
    }

    /// Publish a game state. Receivers are only woken when the value actually
    /// changes, so `changed()` downstream fires once per real transition.
    pub fn set_state(&self, state: GameState) {
        self.state_tx.send_if_modified(|current| {
            if *current == state {
                return false;
            }
            let previous = *current;
            *current = state;
            tracing::info!(from = ?previous, to = ?state, "game state transition");
            true
        });
    }

    /// Cheap snapshot of the current game state.
    pub fn state(&self) -> GameState {
        *self.state_tx.borrow()
    }

    /// Subscribe to state changes. Each consumer owns its own receiver and
    /// awaits `Receiver::changed`.
    pub fn subscribe_state(&self) -> watch::Receiver<GameState> {
        self.state_tx.subscribe()
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