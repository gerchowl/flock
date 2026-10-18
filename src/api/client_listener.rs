//! Per-server structured listener status, observable without contacting that listener.
use std::sync::{Arc, Mutex, PoisonError};
/// Confirmed listener state; older or non-web servers may omit it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Listening,
    Failed,
    /// Configuration disabled this server's listener.
    Disabled,
    /// A serve or funnel exposure stopped the listener.
    ExposedByServe,
}
/// Shared only by one API server and its listener owner; independent servers never share state.
#[derive(Clone, Debug, Default)]
pub(crate) struct Health(Arc<Mutex<Option<Status>>>);
impl Health {
    /// Read the confirmed state without probing or starting any listener.
    pub(crate) fn status(&self) -> Option<Status> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
    /// Publish a transition independently of the headless loop.
    #[cfg(any(feature = "web", test))]
    pub(crate) fn publish(&self, status: Status) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(status);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independently_owned_servers_do_not_share_listener_status() {
        let one = Health::default();
        let observer = one.clone();
        let two = Health::default();
        one.publish(Status::Listening);
        assert_eq!(observer.status(), Some(Status::Listening));
        two.publish(Status::Failed);
        assert_eq!(observer.status(), Some(Status::Listening));
        assert_eq!(two.status(), Some(Status::Failed));
        assert_eq!(Health::default().status(), None);
    }
}
