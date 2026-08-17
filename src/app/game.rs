/// High-level state of the automation target (the game).
///
/// Published through a [`watch`](tokio::sync::watch) channel in
/// [`crate::app::AppContext`]: perception services write transitions, while
/// policy/executor services react via `Receiver::changed`. `Copy` + `PartialEq`
/// keep change-detection and transition logging cheap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum GameState {
    NotStarted,
    MainMenu,
    Playing,
    Paused,
    GameSuccess,
    GameFailure,
    /// Nothing perceived yet — the initial state before any recognition.
    #[default]
    Unknown,
}
