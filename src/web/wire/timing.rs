//! Monotonic transport clock and bounded heartbeat state (ADR-0036 §5a).
use crate::structured_wire::{HEARTBEAT_INTERVAL_MS, LIVENESS_WINDOW_MS};
use futures_util::future::BoxFuture;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
/// Supplied monotonic clock; deadlines share the same origin as received frames.
pub(crate) trait Clock: Send + Sync {
    /// Current monotonic instant, in the same domain as every deadline.
    fn now(&self) -> Instant;
    /// Wait until the supplied instant without polling or wall-clock conversion.
    fn until(&self, deadline: Instant) -> BoxFuture<'_, ()>;
}
/// Normal runtime clock; no wall-clock synchronization is involved.
pub(super) struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn until(&self, deadline: Instant) -> BoxFuture<'_, ()> {
        Box::pin(tokio::time::sleep_until(deadline.into()))
    }
}
/// Outstanding ping history is bounded by the liveness window, never all traffic.
pub(super) struct Heartbeat {
    pub last_pong: Instant,
    pub next_ping: Instant,
    pending: VecDeque<(String, Instant)>,
    sequence: u64,
}
impl Heartbeat {
    pub fn new(now: Instant) -> Self {
        Self {
            last_pong: now,
            next_ping: now + Duration::from_millis(HEARTBEAT_INTERVAL_MS),
            pending: VecDeque::new(),
            sequence: 0,
        }
    }
    pub fn expires(&self) -> Instant {
        self.last_pong + Duration::from_millis(LIVENESS_WINDOW_MS)
    }
    pub fn ping(&mut self, now: Instant, prefix: &str) -> Option<String> {
        self.sequence = self.sequence.checked_add(1)?;
        let nonce = format!("{prefix}/{}", self.sequence);
        self.pending.retain(|(_, sent)| {
            now.duration_since(*sent).as_millis() < u128::from(LIVENESS_WINDOW_MS)
        });
        if self.pending.len() == 3 {
            self.pending.pop_front();
        }
        self.pending.push_back((nonce.clone(), now));
        self.next_ping = now + Duration::from_millis(HEARTBEAT_INTERVAL_MS);
        Some(nonce)
    }
    pub fn pong(&mut self, nonce: &str, now: Instant) {
        if let Some(at) = self.pending.iter().position(|(n, sent)| {
            n == nonce && now.duration_since(*sent).as_millis() < u128::from(LIVENESS_WINDOW_MS)
        }) {
            self.pending.remove(at);
            self.last_pong = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn heartbeat_outstanding_history_is_bounded_and_late_nonce_is_not_evidence() {
        let now = Instant::now();
        let mut heartbeat = Heartbeat::new(now);
        let first = heartbeat.ping(now, "one").unwrap();
        for i in 1..10 {
            heartbeat
                .ping(now + Duration::from_millis(i * 1000), "one")
                .unwrap();
        }
        assert!(heartbeat.pending.len() <= 3);
        heartbeat.pong(&first, now + Duration::from_millis(10_000));
        assert_eq!(heartbeat.last_pong, now);
        let recent = heartbeat
            .ping(now + Duration::from_millis(15_000), "one")
            .unwrap();
        heartbeat.pong(&recent, now + Duration::from_millis(60_000));
        assert_eq!(heartbeat.last_pong, now);
    }
}
