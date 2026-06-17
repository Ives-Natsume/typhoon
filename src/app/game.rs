#[derive(Clone)]
pub enum GameState {
    NotStarted,
    MainMenu,
    Playing,
    Paused,
    GameSuccess,
    GameFailure,
    Unknown,
}
