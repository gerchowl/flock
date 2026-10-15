//! Completion of a queued PTY write, measured after its final successful syscall.
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

#[derive(Debug, Clone)]
pub(crate) struct WriteReceipt(Arc<Mutex<mpsc::Receiver<Instant>>>);

impl WriteReceipt {
    pub(crate) fn channel() -> (mpsc::Sender<Instant>, Self) {
        let (tx, rx) = mpsc::channel();
        (tx, Self(Arc::new(Mutex::new(rx))))
    }

    /// Empty means still queued. Disconnection means discarded without completion.
    pub(crate) fn try_completed(&self) -> Result<Instant, mpsc::TryRecvError> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).try_recv()
    }
}
