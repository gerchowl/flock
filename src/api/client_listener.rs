//! Process-local structured listener status, observable without contacting that listener.
use std::sync::{Mutex, PoisonError};
/// A configured listener's confirmed state; absence means disabled or unavailable on older servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Listening,
    Failed,
}
static STATUS: Mutex<Option<Status>> = Mutex::new(None);
/// Return the last confirmed state, without probing or starting any listener.
pub(crate) fn status() -> Option<Status> {
    *STATUS.lock().unwrap_or_else(PoisonError::into_inner)
}
/// Publish a transition from the listener owner, independently of the headless loop.
#[cfg(feature = "web")]
pub(crate) fn publish(status: Status) {
    *STATUS.lock().unwrap_or_else(PoisonError::into_inner) = Some(status);
}
