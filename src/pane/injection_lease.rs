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

use std::cell::RefCell;
use std::time::Instant;

use bytes::{Bytes, BytesMut};

/// Past this many held bytes the lease ends early rather than growing, so a
/// huge paste reaches the pane instead of memory.
const MAX_HELD_BYTES: usize = 256 * 1024;

#[derive(Debug, Default)]
pub(crate) struct InjectionLease {
    held: RefCell<Option<Held>>,
}

#[derive(Debug)]
struct Held {
    token: String,
    until: Instant,
    buffered: BytesMut,
}

impl InjectionLease {
    /// Take the lease for `token` until `until`. Another injection's live
    /// lease refuses it. An expired one is ended first, and its held bytes are
    /// returned to be written before anything else.
    pub(crate) fn acquire(
        &self,
        token: &str,
        until: Instant,
        now: Instant,
    ) -> Result<Option<Bytes>, ()> {
        let mut held = self.held.borrow_mut();
        if held.as_ref().is_some_and(|lease| lease.until > now) {
            return Err(());
        }
        let flush = held.take().map(|lease| lease.buffered.freeze());
        *held = Some(Held {
            token: token.to_owned(),
            until,
            buffered: BytesMut::new(),
        });
        Ok(flush.filter(|bytes| !bytes.is_empty()))
    }

    /// Hold `bytes` if the lease is live, or return what must be written now:
    /// anything held by a lease that has just run out, then `bytes`.
    pub(crate) fn intercept(&self, bytes: Bytes, now: Instant) -> Option<Bytes> {
        let mut held = self.held.borrow_mut();
        match held.as_mut() {
            Some(lease)
                if lease.until > now && lease.buffered.len() + bytes.len() <= MAX_HELD_BYTES =>
            {
                lease.buffered.extend_from_slice(&bytes);
                None
            }
            Some(_) => {
                let mut out = held.take().map(|lease| lease.buffered).unwrap_or_default();
                out.extend_from_slice(&bytes);
                Some(out.freeze())
            }
            None => Some(bytes),
        }
    }

    /// End `token`'s lease and return what it held. A stale token (its lease
    /// already expired and was retaken) releases nothing.
    pub(crate) fn release(&self, token: &str) -> Option<Bytes> {
        let mut held = self.held.borrow_mut();
        if held.as_ref().is_some_and(|lease| lease.token == token) {
            return held.take().map(|lease| lease.buffered.freeze());
        }
        None
    }

    /// End the lease if it ran out by `now`, returning what it held (possibly
    /// nothing) so the caller can write it. `None` means nothing ended.
    pub(crate) fn expire(&self, now: Instant) -> Option<Bytes> {
        let mut held = self.held.borrow_mut();
        if held.as_ref().is_some_and(|lease| lease.until <= now) {
            return held.take().map(|lease| lease.buffered.freeze());
        }
        None
    }

    /// End whatever lease is held, for a server about to stop or hand off.
    pub(crate) fn release_all(&self) -> Option<Bytes> {
        self.held
            .borrow_mut()
            .take()
            .map(|lease| lease.buffered.freeze())
    }

    pub(crate) fn until(&self) -> Option<Instant> {
        self.held.borrow().as_ref().map(|lease| lease.until)
    }

    /// Does `token` still hold a live lease? An injection checks this before
    /// each step it takes on the operator's behalf.
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
