//! Flock's hold on a pane's operator input for one guarded injection (#850).
//!
//! While flock types a sentence and presses Enter for it, a keystroke from the
//! operator (an attached client, a paste, a `pane.send-*` call) would land in
//! the middle of the sentence or after it and before the Enter. The lease
//! keeps those bytes in order in memory instead, and hands them back when the
//! injection ends, so nothing interleaves and nothing is dropped.
//!
//! The lease is bounded by `until` whatever happens to the injection: the
//! runtime checks it on every write and the app loop expires it on its own
//! deadline, so an injection whose owner vanished cannot leave a pane frozen.
//! Esc or Ctrl-C from the operator ends it at once: those keys are how a
//! human stops what is happening in a pane, and they must not wait.
//!
//! Held bytes that meet a full PTY channel when the lease ends are not
//! dropped. They wait in a backlog, and every later operator write queues
//! behind them until the loop drains it, so the order still holds.
//! Tracked requests stay separate from operator bytes: the receipt gate owns
//! their payload, deadline and cancellation. Expired/cancelled tickets are inert.
//! Ownership transitions retire held/backlogged tracked requests before replaying
//! operator bytes; they never migrate an uncorrelated write to the next owner.

use crate::pty::write_receipt::{WriteCompletion, WriteFailure};
use bytes::{Bytes, BytesMut};
use std::{cell::RefCell, collections::VecDeque, time::Instant};

// Reaching this byte bound ends the lease and flushes live entries, as before.
const MAX_HELD_BYTES: usize = 256 * 1024;
// This is a warning threshold, not permission to discard live input.
const BACKLOG_WARN_BYTES: usize = 1024 * 1024;

/// Keep tracked requests separate: their gate owns the payload and cancellation.
#[derive(Debug)]
pub(crate) enum QueuedInput {
    Operator(Bytes),
    Tracked(WriteCompletion),
}
pub(crate) type InputBatch = VecDeque<QueuedInput>;
/// Preserve legacy operator bursts without ever merging a tracked request.
fn push_input(batch: &mut InputBatch, input: QueuedInput) {
    match (batch.back_mut(), input) {
        (Some(QueuedInput::Operator(previous)), QueuedInput::Operator(bytes)) => {
            let mut joined = std::mem::take(previous)
                .try_into_mut()
                .unwrap_or_else(|bytes| BytesMut::from(bytes.as_ref()));
            joined.extend_from_slice(&bytes);
            *previous = joined.freeze();
        }
        (_, input) => batch.push_back(input),
    }
}

impl From<Bytes> for QueuedInput {
    fn from(bytes: Bytes) -> Self {
        Self::Operator(bytes)
    }
}
impl QueuedInput {
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Operator(bytes) => bytes.len(),
            Self::Tracked(ticket) => ticket.pending_bytes(),
        }
    }
    pub(super) fn pending(&self) -> bool {
        match self {
            Self::Operator(bytes) => !bytes.is_empty(),
            Self::Tracked(ticket) => ticket.is_pending(),
        }
    }
}
#[derive(Debug, Default)]
pub(crate) struct InjectionLease {
    held: RefCell<Option<Held>>,
    backlog: RefCell<InputBatch>,
}

/// Esc or Ctrl-C as a terminal sends it: legacy bytes, or the kitty
/// keyboard protocol's CSI u form with no other modifier.
pub(crate) fn is_interrupt(bytes: &[u8]) -> bool {
    if bytes == b"\x1b" || bytes == b"\x03" {
        return true;
    }
    let Some(body) = bytes
        .strip_prefix(b"\x1b[")
        .and_then(|body| body.strip_suffix(b"u"))
        .and_then(|body| std::str::from_utf8(body).ok())
    else {
        return false;
    };
    let mut fields = body.split(';');
    let code = fields.next().unwrap_or_default();
    let modifiers = fields
        .next()
        .and_then(|field| field.split(':').next())
        .unwrap_or("1");
    let event = body
        .split(';')
        .nth(1)
        .and_then(|field| field.split(':').nth(1))
        .unwrap_or("1");
    // A key release (event type 3) is not a request to stop.
    event != "3" && ((code == "27" && modifiers == "1") || (code == "99" && modifiers == "5"))
}

#[derive(Debug)]
struct Held {
    token: String,
    until: Instant,
    buffered: InputBatch,
}

impl InjectionLease {
    pub(crate) fn acquire(
        &self,
        token: &str,
        until: Instant,
        now: Instant,
    ) -> Result<Option<InputBatch>, ()> {
        let mut held = self.held.borrow_mut();
        if held.as_ref().is_some_and(|lease| lease.until > now) {
            return Err(());
        }
        let flush = held.take().map(|lease| lease.buffered);
        *held = Some(Held {
            token: token.to_owned(),
            until,
            buffered: InputBatch::new(),
        });
        Ok(flush.filter(|batch| !batch.is_empty()))
    }
    pub(crate) fn intercept(
        &self,
        input: impl Into<QueuedInput>,
        now: Instant,
    ) -> Option<InputBatch> {
        let input = input.into();
        if !input.pending() {
            return None;
        }
        let mut held = self.held.borrow_mut();
        let interrupt = matches!(&input, QueuedInput::Operator(bytes) if is_interrupt(bytes));
        if let Some(lease) = held.as_mut() {
            lease.buffered.retain(QueuedInput::pending);
            let len: usize = lease.buffered.iter().map(QueuedInput::len).sum();
            if lease.until > now && len + input.len() <= MAX_HELD_BYTES && !interrupt {
                push_input(&mut lease.buffered, input);
                return None;
            }
        }
        let mut batch = held.take().map(|lease| lease.buffered).unwrap_or_default();
        push_input(&mut batch, input);
        Some(batch)
    }
    pub(crate) fn release(&self, token: &str) -> Option<InputBatch> {
        let mut held = self.held.borrow_mut();
        if held.as_ref().is_some_and(|lease| lease.token == token) {
            return held.take().map(|lease| lease.buffered);
        }
        None
    }
    pub(crate) fn expire(&self, now: Instant) -> Option<InputBatch> {
        let mut held = self.held.borrow_mut();
        if held.as_ref().is_some_and(|lease| lease.until <= now) {
            return held.take().map(|lease| lease.buffered);
        }
        None
    }
    /// Retire tracked input with its transition cause; keep operator order.
    pub(crate) fn release_all(&self, reason: WriteFailure) -> Option<InputBatch> {
        let keep_operator = |input: &QueuedInput| match input {
            QueuedInput::Operator(_) => true,
            QueuedInput::Tracked(ticket) => {
                ticket.retire(reason);
                false
            }
        };
        self.backlog.borrow_mut().retain(keep_operator);
        self.held
            .borrow_mut()
            .take()
            .map(|lease| lease.buffered.into_iter().filter(keep_operator).collect())
    }
    pub(crate) fn behind_backlog(&self, batch: InputBatch) -> InputBatch {
        let mut backlog = self.backlog.borrow_mut();
        for input in batch {
            push_input(&mut backlog, input);
        }
        std::mem::take(&mut *backlog)
    }
    pub(crate) fn defer(&self, mut batch: InputBatch) -> bool {
        let mut backlog = self.backlog.borrow_mut();
        batch.append(&mut *backlog);
        *backlog = batch;
        backlog.iter().map(QueuedInput::len).sum::<usize>() > BACKLOG_WARN_BYTES
    }
    pub(crate) fn take_backlog(&self) -> Option<InputBatch> {
        let mut backlog = self.backlog.borrow_mut();
        backlog.retain(QueuedInput::pending);
        (!backlog.is_empty()).then(|| std::mem::take(&mut *backlog))
    }
    pub(crate) fn has_backlog(&self) -> bool {
        self.backlog.borrow_mut().retain(QueuedInput::pending);
        !self.backlog.borrow().is_empty()
    }
    pub(crate) fn until(&self) -> Option<Instant> {
        self.held.borrow().as_ref().map(|lease| lease.until)
    }
    pub(crate) fn holds(&self, token: &str, now: Instant) -> bool {
        self.held
            .borrow()
            .as_ref()
            .is_some_and(|lease| lease.token == token && lease.until > now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // Preserve the existing byte-stream assertions across packet boundaries.
    #[derive(Default)]
    struct InjectionLease(super::InjectionLease);
    fn flatten(batch: InputBatch) -> Bytes {
        Bytes::from(
            batch
                .into_iter()
                .flat_map(|input| match input {
                    QueuedInput::Operator(bytes) => bytes.to_vec(),
                    QueuedInput::Tracked(_) => panic!("legacy fixture contains a ticket"),
                })
                .collect::<Vec<_>>(),
        )
    }
    impl InjectionLease {
        fn acquire(&self, token: &str, until: Instant, now: Instant) -> Result<Option<Bytes>, ()> {
            self.0
                .acquire(token, until, now)
                .map(|batch| batch.map(flatten))
        }
        fn intercept(&self, bytes: Bytes, now: Instant) -> Option<Bytes> {
            self.0.intercept(bytes, now).map(flatten)
        }
        fn release(&self, token: &str) -> Option<Bytes> {
            self.0.release(token).map(flatten)
        }
        fn expire(&self, now: Instant) -> Option<Bytes> {
            self.0.expire(now).map(flatten)
        }
        fn release_all(&self) -> Option<Bytes> {
            self.0.release_all(WriteFailure::OwnerChanged).map(flatten)
        }
        fn behind_backlog(&self, bytes: Bytes) -> Bytes {
            flatten(self.0.behind_backlog([QueuedInput::Operator(bytes)].into()))
        }
        fn defer(&self, bytes: Bytes) -> bool {
            self.0.defer([QueuedInput::Operator(bytes)].into())
        }
        fn take_backlog(&self) -> Option<Bytes> {
            self.0.take_backlog().map(flatten)
        }
        fn has_backlog(&self) -> bool {
            self.0.has_backlog()
        }
        fn until(&self) -> Option<Instant> {
            self.0.until()
        }
        fn holds(&self, token: &str, now: Instant) -> bool {
            self.0.holds(token, now)
        }
    }

    fn bytes(text: &'static str) -> Bytes {
        Bytes::from_static(text.as_bytes())
    }

    #[test]
    fn no_lease_passes_input_through() {
        let lease = InjectionLease::default();
        assert_eq!(
            lease.intercept(bytes("a"), Instant::now()),
            Some(bytes("a"))
        );
    }

    #[test]
    fn held_input_comes_back_in_order_on_release() {
        let now = Instant::now();
        let lease = InjectionLease::default();
        lease
            .acquire("t", now + Duration::from_secs(5), now)
            .unwrap();
        for key in ["h", "e", "\x1b[A", "llo"] {
            assert_eq!(lease.intercept(Bytes::from(key), now), None);
        }
        assert_eq!(
            lease.release("other"),
            None,
            "a stale token releases nothing"
        );
        assert_eq!(lease.release("t"), Some(bytes("he\x1b[Allo")));
        assert_eq!(lease.intercept(bytes("x"), now), Some(bytes("x")));
    }

    #[test]
    fn a_lease_ends_on_its_bound_without_its_owner() {
        let now = Instant::now();
        let until = now + Duration::from_secs(7);
        let lease = InjectionLease::default();
        lease.acquire("t", until, now).unwrap();
        assert_eq!(lease.intercept(bytes("a"), now), None);
        assert_eq!(lease.expire(until - Duration::from_millis(1)), None);
        assert_eq!(lease.expire(until), Some(bytes("a")));
        assert_eq!(lease.until(), None);
        // A write after the bound flushes the held bytes first.
        lease.acquire("u", until, now).unwrap();
        assert_eq!(lease.intercept(bytes("b"), now), None);
        assert_eq!(lease.intercept(bytes("c"), until), Some(bytes("bc")));
        assert!(!lease.holds("u", until));
    }

    #[test]
    fn a_live_lease_refuses_a_second_injection() {
        let now = Instant::now();
        let lease = InjectionLease::default();
        lease
            .acquire("t", now + Duration::from_secs(1), now)
            .unwrap();
        assert!(lease
            .acquire("u", now + Duration::from_secs(1), now)
            .is_err());
        assert_eq!(lease.intercept(bytes("a"), now), None);
        let later = now + Duration::from_secs(1);
        assert_eq!(
            lease.acquire("u", later + Duration::from_secs(1), later),
            Ok(Some(bytes("a")))
        );
        assert!(lease.holds("u", later));
    }

    #[test]
    fn esc_or_ctrl_c_ends_the_lease_at_once() {
        for key in [
            &b"\x1b"[..],
            b"\x03",
            b"\x1b[27u",
            b"\x1b[27;1:1u",
            b"\x1b[99;5u",
        ] {
            let now = Instant::now();
            let lease = InjectionLease::default();
            lease
                .acquire("t", now + Duration::from_secs(5), now)
                .unwrap();
            assert_eq!(lease.intercept(bytes("a"), now), None);
            let out = lease.intercept(Bytes::copy_from_slice(key), now);
            assert_eq!(
                out.as_deref(),
                Some([b"a", key].concat().as_slice()),
                "{key:?}"
            );
            assert_eq!(lease.until(), None, "{key:?}");
        }
        for not_an_interrupt in [
            &b"\x1b[A"[..],
            b"\x1b[27;1:3u",
            b"\x1b[99u",
            b"\x1b[I",
            b"\x1b[<0;3;4M",
        ] {
            assert!(!is_interrupt(not_an_interrupt), "{not_an_interrupt:?}");
        }
    }

    #[test]
    fn a_backlog_keeps_its_place_ahead_of_later_writes() {
        let lease = InjectionLease::default();
        assert_eq!(lease.behind_backlog(bytes("x")), bytes("x"));
        assert!(!lease.defer(bytes("ab")));
        assert!(lease.has_backlog());
        assert_eq!(lease.behind_backlog(bytes("c")), bytes("abc"));
        assert!(!lease.has_backlog());
        lease.defer(bytes("c"));
        lease.defer(bytes("ab"));
        assert_eq!(lease.take_backlog(), Some(bytes("abc")));
        assert_eq!(lease.take_backlog(), None);
    }

    #[test]
    fn an_overflowing_buffer_ends_the_lease_instead_of_dropping() {
        let now = Instant::now();
        let lease = InjectionLease::default();
        lease
            .acquire("t", now + Duration::from_secs(5), now)
            .unwrap();
        assert_eq!(lease.intercept(bytes("a"), now), None);
        let big = Bytes::from(vec![b'x'; MAX_HELD_BYTES]);
        let out = lease.intercept(big, now).expect("written, not held");
        assert_eq!(out.len(), MAX_HELD_BYTES + 1);
        assert_eq!(out[0], b'a');
        assert_eq!(lease.until(), None);
        assert_eq!(lease.release_all(), None);
    }
}
