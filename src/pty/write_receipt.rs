//! Completion, payload ownership and cancellation of a queued PTY write.
use bytes::Bytes;
use std::io;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

/// Why a tracked write stopped; byte evidence survives every terminal cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteFailure {
    Cancelled,
    Deadline,
    WriterBroken,
    OwnerChanged,
    PaneGone,
    WriteFailed,
}

/// Exact encoded-byte evidence retained after the payload or writer disappears.
// The API boundary in #1638 consumes all fields, including late-full evidence.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WriteProgress {
    pub bytes_written: usize,
    pub bytes_total: usize,
    pub completed_at: Option<Instant>,
    pub failure: Option<WriteFailure>,
    pub remainder_discarded: bool,
}

#[derive(Debug)]
struct WriteState {
    payload: Bytes,
    progress: WriteProgress,
    deadline: Option<Instant>,
}

impl WriteState {
    /// ADR-0028 §5a: completion at the deadline is timely on both gate edges.
    fn expired(&self, at: Instant) -> bool {
        self.deadline.is_some_and(|deadline| at > deadline)
    }
    /// The syscall may begin at the deadline; an instant after it must refuse.
    fn before_syscall(&mut self, at: Instant) -> bool {
        if self.progress.failure.is_some() || self.progress.completed_at.is_some() {
            return false;
        }
        if self.expired(at) {
            self.fail(WriteFailure::Deadline);
            return false;
        }
        true
    }

    /// Record the actual final-write instant before deciding whether it is timely.
    fn finish_write(&mut self, at: Instant) -> bool {
        self.progress.completed_at = Some(at);
        self.payload = Bytes::new();
        if self.expired(at) {
            self.fail(WriteFailure::Deadline);
            return false;
        }
        true
    }

    fn fail(&mut self, reason: WriteFailure) {
        self.progress.failure.get_or_insert(reason);
        self.payload = Bytes::new();
        self.progress.remainder_discarded = true;
    }
}

#[derive(Debug)]
struct SharedWrite {
    completion: Mutex<mpsc::Receiver<Instant>>,
    state: Mutex<WriteState>,
}

#[derive(Debug, Clone)]
pub(crate) struct WriteReceipt(Arc<SharedWrite>);

/// The queued ticket retains authority, but the gate owns the only payload.
#[derive(Debug)]
pub(crate) struct WriteCompletion {
    completion: mpsc::Sender<Instant>,
    shared: Arc<SharedWrite>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteDisposition {
    Cancelled,
    Partial,
    Completed,
}

impl WriteReceipt {
    pub(crate) fn channel(bytes: Bytes, deadline: Option<Instant>) -> (WriteCompletion, Self) {
        let (tx, rx) = mpsc::channel();
        let progress = WriteProgress {
            bytes_written: 0,
            bytes_total: bytes.len(),
            completed_at: None,
            failure: None,
            remainder_discarded: false,
        };
        let shared = Arc::new(SharedWrite {
            completion: Mutex::new(rx),
            state: Mutex::new(WriteState {
                payload: bytes,
                progress,
                deadline,
            }),
        });
        (
            WriteCompletion {
                completion: tx,
                shared: shared.clone(),
            },
            Self(shared),
        )
    }

    /// Snapshot byte evidence, retiring an expired pending request under its gate.
    pub(crate) fn progress(&self) -> WriteProgress {
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.progress.completed_at.is_none() && state.expired(Instant::now()) {
            state.fail(WriteFailure::Deadline);
        }
        state.progress
    }

    /// Observe expiry under the writer gate even when a live producer is silent.
    /// A timely completion stays successful when observed after its deadline.
    pub(crate) fn try_completed(&self) -> Result<Instant, mpsc::TryRecvError> {
        if self.progress().failure.is_some() {
            return Err(mpsc::TryRecvError::Disconnected);
        }
        self.0
            .completion
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .try_recv()
    }

    /// Cancel under the syscall gate, removing the remainder even after a prefix.
    pub(crate) fn cancel(&self) -> WriteDisposition {
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.progress.completed_at.is_some() && state.progress.failure.is_none() {
            return WriteDisposition::Completed;
        }
        state.fail(WriteFailure::Cancelled);
        if state.progress.bytes_written > 0 {
            WriteDisposition::Partial
        } else {
            WriteDisposition::Cancelled
        }
    }
}

impl WriteCompletion {
    /// Retire authority and release bytes without losing the actual terminal cause.
    pub(crate) fn retire(&self, reason: WriteFailure) {
        let mut state = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.progress.completed_at.is_none() {
            state.fail(reason);
        }
    }

    /// Prune cancelled/expired tickets before replay without borrowing their bytes.
    pub(crate) fn is_pending(&self) -> bool {
        let mut state = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.progress.failure.is_some() || state.progress.completed_at.is_some() {
            return false;
        }
        if state.expired(Instant::now()) {
            state.fail(WriteFailure::Deadline);
        }
        state.progress.failure.is_none()
    }

    pub(crate) fn pending_bytes(&self) -> usize {
        let state = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
        state
            .payload
            .len()
            .saturating_sub(state.progress.bytes_written)
    }

    /// Recover a refused admission's bytes before dropping its ticket.
    pub(crate) fn take_payload(&self) -> Bytes {
        let mut state = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
        std::mem::take(&mut state.payload)
    }

    /// Deadline, syscall, count and final timestamp share the cancellation gate.
    /// None means a terminal ticket: its payload can never be written again.
    pub(crate) fn write(
        &self,
        write: impl FnOnce(&[u8]) -> io::Result<usize>,
    ) -> Option<io::Result<usize>> {
        let mut state = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
        if !state.before_syscall(Instant::now()) {
            return None;
        }
        let result = if state.payload.is_empty() {
            Ok(0)
        } else {
            write(&state.payload[state.progress.bytes_written..])
        };
        match &result {
            Ok(n) => {
                state.progress.bytes_written += n;
                if state.progress.bytes_written == state.progress.bytes_total {
                    let at = Instant::now();
                    if state.finish_write(at) {
                        let _ = self.completion.send(at);
                    }
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => state.fail(WriteFailure::WriteFailed),
        }
        Some(result)
    }
}

impl Drop for WriteCompletion {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.progress.completed_at.is_none() {
            // Losing the write owner removes this pane's remaining write authority.
            state.fail(WriteFailure::PaneGone);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    fn tracked(deadline: Option<Instant>) -> (WriteCompletion, WriteReceipt) {
        WriteReceipt::channel(Bytes::from_static(b"paste"), deadline)
    }

    #[test]
    fn partial_cancel_keeps_prefix_evidence_and_discards_the_tail() {
        let (mut socket, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let (completion, receipt) = tracked(None);
        assert_eq!(
            completion
                .write(|bytes| socket.write(&bytes[..2]))
                .unwrap()
                .unwrap(),
            2
        );
        let mut prefix = [0; 2];
        peer.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"pa");
        assert_eq!(receipt.cancel(), WriteDisposition::Partial);
        assert!(completion.write(|bytes| socket.write(bytes)).is_none());
        let progress = receipt.progress();
        assert_eq!((progress.bytes_written, progress.bytes_total), (2, 5));
        assert!(progress.remainder_discarded);
        assert_eq!(progress.failure, Some(WriteFailure::Cancelled));
        peer.set_nonblocking(true).unwrap();
        assert_eq!(
            peer.read(&mut prefix).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    struct OwnedPayload(Arc<AtomicBool>);
    impl AsRef<[u8]> for OwnedPayload {
        fn as_ref(&self) -> &[u8] {
            b"paste"
        }
    }
    impl Drop for OwnedPayload {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn cancellation_releases_payload_while_the_queued_ticket_stays_alive() {
        let dropped = Arc::new(AtomicBool::new(false));
        let (completion, receipt) =
            WriteReceipt::channel(Bytes::from_owner(OwnedPayload(dropped.clone())), None);
        let (tx, rx) = mpsc::channel();
        tx.send(completion).unwrap();
        assert_eq!(receipt.cancel(), WriteDisposition::Cancelled);
        assert!(
            dropped.load(Ordering::SeqCst),
            "inert channel entry retains no live payload"
        );
        assert!(rx
            .recv()
            .unwrap()
            .write(|_| panic!("cancelled payload written"))
            .is_none());
        assert_eq!(receipt.progress().bytes_written, 0);
    }

    #[test]
    fn expired_ticket_checks_deadline_before_the_syscall_under_contention() {
        let (completion, receipt) = tracked(Some(Instant::now() + Duration::from_secs(30)));
        let mut state = receipt.0.state.lock().unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            let result = completion.write(|_| panic!("expired ticket reached syscall"));
            done_tx.send(result).unwrap();
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        state.deadline = Some(Instant::now());
        drop(state);
        assert!(done_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .is_none());
        writer.join().unwrap();
        let progress = receipt.progress();
        assert_eq!(progress.failure, Some(WriteFailure::Deadline));
        assert_eq!(progress.bytes_written, 0);
        assert!(progress.remainder_discarded);
    }

    #[test]
    fn empty_and_completed_writes_win_over_later_cancellation() {
        for bytes in [Bytes::new(), Bytes::from_static(b"paste")] {
            let total = bytes.len();
            let (completion, receipt) = WriteReceipt::channel(bytes, None);
            assert_eq!(
                completion
                    .write(|chunk| {
                        assert!(total > 0, "empty write must not invoke a syscall");
                        Ok(chunk.len())
                    })
                    .unwrap()
                    .unwrap(),
                total
            );
            let at = receipt.try_completed().unwrap();
            assert_eq!(receipt.cancel(), WriteDisposition::Completed);
            assert!(completion
                .write(|_| panic!("completed request replayed"))
                .is_none());
            assert_eq!(receipt.try_completed(), Err(mpsc::TryRecvError::Empty));
            drop(completion);
            let progress = receipt.progress();
            assert_eq!(progress.completed_at, Some(at));
            assert_eq!(progress.bytes_written, total);
            assert_eq!(progress.failure, None);
        }
    }

    #[test]
    fn writer_loss_preserves_each_requests_count_and_releases_payloads() {
        let (first, first_receipt) = tracked(None);
        let (second, second_receipt) = tracked(None);
        first.write(|_| Ok(2)).unwrap().unwrap();
        drop(first);
        drop(second);
        for (receipt, written) in [(first_receipt, 2), (second_receipt, 0)] {
            assert_eq!(
                receipt.try_completed(),
                Err(mpsc::TryRecvError::Disconnected)
            );
            let progress = receipt.progress();
            assert_eq!(progress.bytes_written, written);
            assert_eq!(progress.failure, Some(WriteFailure::PaneGone));
            assert!(progress.remainder_discarded);
        }
    }

    fn assert_late_syscall(count: usize) {
        let deadline = Instant::now() + Duration::from_millis(200);
        let (completion, receipt) = tracked(Some(deadline));
        let (mut socket, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
        let (hold, release) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            let result = completion.write(|bytes| {
                let written = socket.write(&bytes[..count])?;
                // Use the real clock to cover the syscall-return boundary without a product clock hook.
                // This bounded receive holds the producer alive through its actual deadline.
                assert_eq!(
                    release.recv_timeout(deadline.saturating_duration_since(Instant::now())),
                    Err(mpsc::RecvTimeoutError::Timeout)
                );
                Ok(written)
            });
            assert!(completion
                .write(|_| panic!("late syscall allowed another write"))
                .is_none());
            done_tx.send(result).unwrap();
        });
        let mut observed = vec![0; count];
        peer.read_exact(&mut observed).unwrap();
        assert_eq!(&observed, &b"paste"[..count]);
        assert_eq!(
            done_rx
                .recv_timeout(Duration::from_secs(4))
                .unwrap()
                .unwrap()
                .unwrap(),
            count
        );
        writer.join().unwrap();
        drop(hold);
        let progress = receipt.progress();
        assert_eq!((progress.bytes_written, progress.bytes_total), (count, 5));
        if count == 5 {
            assert!(progress.completed_at.unwrap() > deadline);
        } else {
            assert_eq!(progress.completed_at, None);
        }
        assert_eq!(progress.failure, Some(WriteFailure::Deadline));
        assert!(
            progress.remainder_discarded,
            "including a full late write with no remainder"
        );
        assert_eq!(
            receipt.try_completed(),
            Err(mpsc::TryRecvError::Disconnected)
        );
        assert_eq!(receipt.cancel(), WriteDisposition::Partial);
    }

    #[test]
    fn late_partial_syscall_discards_the_tail_before_another_write() {
        assert_late_syscall(2);
    }

    #[test]
    fn late_full_syscall_retains_full_count_without_success() {
        assert_late_syscall(5);
    }

    #[test]
    fn live_silent_producer_is_cancelled_at_its_deadline() {
        let (completion, receipt) = tracked(Some(Instant::now()));
        assert!(completion
            .write(|_| panic!("expired silent producer resumed"))
            .is_none());
        assert_eq!(
            receipt.try_completed(),
            Err(mpsc::TryRecvError::Disconnected)
        );
        assert_eq!(receipt.progress().failure, Some(WriteFailure::Deadline));
        assert_eq!(completion.pending_bytes(), 0);
    }
    #[test]
    fn write_deadline_edge_is_inclusive_before_and_after_syscall() {
        let deadline = Instant::now();
        for (begin, finish, allowed, timely) in [
            (deadline, deadline, true, true),
            (deadline - Duration::from_nanos(1), deadline, true, true),
            (deadline, deadline + Duration::from_nanos(1), true, false),
            (deadline + Duration::from_nanos(1), deadline, false, false),
        ] {
            let (ticket, receipt) = tracked(Some(deadline));
            let (mut socket, mut peer) = UnixStream::pair().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut state = receipt.0.state.lock().unwrap();
            assert_eq!(state.before_syscall(begin), allowed);
            if allowed {
                let n = socket.write(&state.payload).unwrap();
                state.progress.bytes_written += n;
                assert_eq!(state.finish_write(finish), timely);
                let mut observed = [0; 5];
                peer.read_exact(&mut observed).unwrap();
                assert_eq!(&observed, b"paste");
                assert_eq!(state.progress.bytes_written, 5);
                assert_eq!(state.progress.completed_at, Some(finish));
            } else {
                assert_eq!(state.progress.bytes_written, 0);
                assert!(state.payload.is_empty());
            }
            assert_eq!(
                state.progress.failure,
                (!timely).then_some(WriteFailure::Deadline)
            );
            assert!(
                !state.before_syscall(finish),
                "terminal write cannot repeat"
            );
            drop(state);
            drop(ticket);
        }
    }

    #[test]
    fn live_silent_writer_hits_its_own_deadline_before_resuming() {
        let deadline = Instant::now() + Duration::from_millis(200);
        let (completion, receipt) = tracked(Some(deadline));
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let result = completion.write(|_| panic!("cancelled silent producer replayed input"));
            done_tx.send(result.is_none()).unwrap();
        });
        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(receipt.try_completed(), Err(mpsc::TryRecvError::Empty));
        assert_eq!(
            done_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        // The producer is still alive and has not attempted a syscall. Observing
        // expiry must retire the request without an explicit caller cancellation.
        let outcome = receipt.try_completed();
        let progress = receipt.progress();
        release_tx.send(()).unwrap();
        assert!(done_rx.recv_timeout(Duration::from_secs(2)).unwrap());
        writer.join().unwrap();
        assert_eq!(outcome, Err(mpsc::TryRecvError::Disconnected));
        assert_eq!(progress.failure, Some(WriteFailure::Deadline));
        assert!(progress.remainder_discarded);
        assert_eq!(progress.bytes_written, 0);
    }
}
