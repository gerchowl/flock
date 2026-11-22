//! Test-only observation of real permission-book acquisition attempts.
use super::*;

impl InputHandle {
    /// Observe a real book acquisition attempt without changing its outcome.
    pub(super) fn observe_permission_attempt(&self, contended: bool) {
        if let Some(sender) = self.permission_attempt.lock().take() {
            sender.send(contended).unwrap();
        }
    }
}
