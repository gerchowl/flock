//! Bounded, process-local browser codes (ADR-0036 §5). Slice 2 mounts the exchange.
use crate::api::client_tokens::Bearer;
use crate::structured::permissions::auth::tokens::TokenError;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Live browser-code storage bound, independent of failed exchanges.
const MAX_LIVE_CODES: usize = 128;
/// Code lifetime, measured only on the monotonic clock.
const CODE_LIFETIME: Duration = Duration::from_secs(60);
/// Reject ambiguous, wildcard, remote, or ephemeral dogfood endpoints.
pub(crate) fn endpoint(bind: Option<SocketAddr>) -> Option<String> {
    bind.filter(|a| a.ip() == Ipv4Addr::LOCALHOST && a.port() != 0)
        .map(|a| format!("http://{a}/"))
}
/// Shared with the self-contained exchange router; neither Debug nor persistence exposes codes.
#[derive(Clone, Default)]
pub(crate) struct Codes(Arc<Mutex<Registry>>);
#[derive(Default)]
struct Registry {
    pending: HashMap<[u8; 32], (Instant, Bearer)>,
    endpoint: Option<SocketAddr>,
}
impl Codes {
    /// Current live endpoint is independent of configuration reloads.
    pub(crate) fn bound_endpoint(&self) -> Option<SocketAddr> {
        self.0.lock().ok()?.endpoint
    }
    /// Publish the actual OS bind before operator requests can mint browser URLs.
    #[cfg(feature = "web")]
    pub(crate) fn set_endpoint(&self, bind: SocketAddr) -> Result<(), TokenError> {
        self.0.lock().map_err(|_| TokenError::Busy)?.endpoint = Some(bind);
        Ok(())
    }
    /// Retain at most 128 live handoffs, with a hard 60-second lifetime.
    pub(crate) fn mint(&self, code: String, token: Bearer, now: Instant) -> Result<(), TokenError> {
        let code: [u8; 32] = Sha256::digest(code.as_bytes()).into();
        let mut state = self.0.lock().map_err(|_| TokenError::Busy)?;
        state.pending.retain(|_, (expiry, _)| now < *expiry);
        if state.pending.len() >= MAX_LIVE_CODES || state.pending.contains_key(&code) {
            return Err(TokenError::Busy);
        }
        state.pending.insert(code, (now + CODE_LIFETIME, token));
        Ok(())
    }
    // Slice 2 (#1828) mounts the exchange only in web-enabled builds.
    #[cfg_attr(not(feature = "web"), allow(dead_code))]
    /// Consume once; unknown and expired failures are indistinguishable.
    /// Invalid attempts never lock out a valid code. No credential is logged.
    pub(crate) fn exchange(&self, code: &str, now: Instant) -> Option<Bearer> {
        let mut state = self.0.lock().ok()?;
        let entry = state
            .pending
            .remove(&<[u8; 32]>::from(Sha256::digest(code.as_bytes())));
        state.pending.retain(|_, (expiry, _)| now < *expiry);
        entry
            .filter(|(expiry, _)| now < *expiry)
            .map(|(_, token)| token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn token() -> Bearer {
        Bearer("fixture credential".into())
    }
    #[test]
    fn codes_are_single_use_and_expire_at_exactly_sixty_seconds() {
        let codes = Codes::default();
        let now = Instant::now();
        codes.mint("one".into(), token(), now).unwrap();
        assert_eq!(codes.exchange("one", now).unwrap().0, "fixture credential");
        assert!(codes.exchange("one", now).is_none());
        codes.mint("boundary".into(), token(), now).unwrap();
        assert!(codes
            .exchange("boundary", now + Duration::from_secs(60))
            .is_none());
        codes.mint("before".into(), token(), now).unwrap();
        assert!(codes
            .exchange("before", now + Duration::from_millis(59999))
            .is_some());
        assert!(codes.exchange("unknown", now).is_none());
    }
    #[test]
    fn junk_exchanges_cannot_lock_out_the_operator() {
        let codes = Codes::default();
        let now = Instant::now();
        codes.mint("valid".into(), token(), now).unwrap();
        for _ in 0..4096 {
            assert!(codes.exchange("unknown", now).is_none());
        }
        assert_eq!(
            codes.exchange("valid", now).unwrap().0,
            "fixture credential"
        );
        assert!(codes.exchange("valid", now).is_none());
    }
    #[test]
    fn live_codes_are_bounded_and_expired_codes_are_reclaimed() {
        let codes = Codes::default();
        let now = Instant::now();
        for i in 0..128 {
            codes.mint(i.to_string(), token(), now).unwrap();
        }
        assert!(codes.mint("overflow".into(), token(), now).is_err());
        codes
            .mint("fresh".into(), token(), now + Duration::from_secs(60))
            .unwrap();
    }
    #[test]
    fn dogfood_endpoint_is_explicit_ipv4_loopback_with_nonzero_port() {
        assert!(endpoint(None).is_none());
        for bind in [
            "0.0.0.0:8080",
            "192.0.2.1:8080",
            "127.0.0.1:0",
            "[::1]:8080",
        ] {
            assert!(endpoint(Some(bind.parse().unwrap())).is_none());
        }
        assert_eq!(
            endpoint(Some("127.0.0.1:8080".parse().unwrap())).as_deref(),
            Some("http://127.0.0.1:8080/")
        );
    }
}
