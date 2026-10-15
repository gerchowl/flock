//! Completion and cancellation of a queued PTY write.
use std::io;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

#[derive(Debug, Default)]
struct WriteState {
    started: bool,
    cancelled: bool,
}

#[derive(Debug)]
struct SharedWrite {
    completion: Mutex<mpsc::Receiver<Instant>>,
    state: Mutex<WriteState>,
}

#[derive(Debug, Clone)]
pub(crate) struct WriteReceipt(Arc<SharedWrite>);

#[derive(Debug)]
pub(crate) struct WriteCompletion {
    completion: mpsc::Sender<Instant>,
    shared: Arc<SharedWrite>,
}

impl WriteReceipt {
    pub(crate) fn channel() -> (WriteCompletion, Self) {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(SharedWrite {
            completion: Mutex::new(rx),
            state: Mutex::new(WriteState::default()),
        });
        (
            WriteCompletion {
                completion: tx,
                shared: shared.clone(),
            },
            Self(shared),
        )
    }

    /// Empty means still queued. Disconnection means discarded without completion.
    pub(crate) fn try_completed(&self) -> Result<Instant, mpsc::TryRecvError> {
        self.0
            .completion
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .try_recv()
    }

    /// Retract a write only if no byte reached the PTY. The writer holds the
    /// same lock across its nonblocking syscall, so cancellation cannot race it.
    pub(crate) fn cancel_unstarted(&self) -> bool {
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.started {
            return false;
        }
        state.cancelled = true;
        true
    }
}

impl WriteCompletion {
    /// None means cancellation won before any bytes were written.
    pub(crate) fn write(
        &self,
        write: impl FnOnce() -> io::Result<usize>,
    ) -> Option<io::Result<usize>> {
        let mut state = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.cancelled {
            return None;
        }
        let result = write();
        if matches!(result, Ok(n) if n > 0) {
            state.started = true;
        }
        Some(result)
    }

    pub(crate) fn send(&self, at: Instant) -> Result<(), mpsc::SendError<Instant>> {
        self.completion.send(at)
    }
}
