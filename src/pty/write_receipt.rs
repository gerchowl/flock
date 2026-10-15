//! Completion and cancellation of a queued PTY write.
use std::io;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

#[derive(Debug, Default)]
struct WriteState {
    started: bool,
    cancelled: bool,
    completed: bool,
}

#[derive(Debug)]
struct SharedWrite {
    completion: Mutex<mpsc::Receiver<Instant>>,
    state: Mutex<WriteState>,
}

#[derive(Debug, Clone)]
pub(crate) struct WriteReceipt(Arc<SharedWrite>);

/// Writer-owned acknowledgement and cancellation gate for one tracked payload.
#[derive(Debug)]
pub(crate) struct WriteCompletion {
    completion: mpsc::Sender<Instant>,
    shared: Arc<SharedWrite>,
}

/// What cancellation can guarantee about the payload's effect on the PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteDisposition {
    Cancelled,
    Partial,
    Completed,
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
    /// The result distinguishes an untouched payload from partial and full writes.
    pub(crate) fn cancel(&self) -> WriteDisposition {
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.completed {
            WriteDisposition::Completed
        } else if state.started {
            WriteDisposition::Partial
        } else {
            state.cancelled = true;
            WriteDisposition::Cancelled
        }
    }
}

impl WriteCompletion {
    /// None means cancellation won before any bytes were written. `remaining`
    /// is the unsent payload length, so the final syscall records completion
    /// before releasing the cancellation gate.
    pub(crate) fn write(
        &self,
        remaining: usize,
        write: impl FnOnce() -> io::Result<usize>,
    ) -> Option<io::Result<usize>> {
        let mut state = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.cancelled {
            return None;
        }
        let result = write();
        if matches!(result, Ok(n) if n > 0) {
            state.started = true;
            state.completed = matches!(result, Ok(n) if n >= remaining);
        }
        Some(result)
    }

    /// Publish the timestamp of the final successful syscall. Completion state
    /// is retained independently of whether the receiver has polled its receipt.
    pub(crate) fn send(&self, at: Instant) -> Result<(), mpsc::SendError<Instant>> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .completed = true;
        self.completion.send(at)
    }
}
