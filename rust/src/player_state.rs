#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerState {
  Idle,
  Playing,
  Paused,
  Stopped,
}