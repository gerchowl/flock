//! Retry only a scoped pre-admission refusal, never an ambiguous transport write.
use crate::api::error_code::ErrorCode;
use std::{
    fmt::Debug,
    time::{Duration, Instant},
};

/// Total scoped admission attempts, including the initial request.
const LEASE_ATTEMPTS: usize = 3;
/// Fixed pause between refused admissions. No effect was admitted by lease_busy.
const LEASE_BACKOFF: Duration = Duration::from_millis(25);
/// Supplied wait clock keeps retry sequencing deterministic in tests.
pub(super) trait Clock: Debug + Send + Sync {
    /// Advance past a backoff before issuing another admission attempt.
    fn wait(&self, duration: Duration);
    /// Monotonic instant used to retain the caller's absolute deadline.
    fn now(&self) -> Instant;
}
#[derive(Debug)]
pub(super) struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn wait(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}
pub(super) fn attempts<T, E>(
    scoped: bool,
    clock: &dyn Clock,
    deadline: Option<Instant>,
    mut attempt: impl FnMut() -> Result<T, E>,
    response: impl Fn(&T) -> &serde_json::Value,
) -> Result<T, E> {
    let mut result = attempt()?;
    for _ in 1..LEASE_ATTEMPTS {
        let busy = response(&result)
            .pointer("/error/code")
            .and_then(|v| v.as_str())
            .and_then(ErrorCode::from_code)
            == Some(ErrorCode::LeaseBusy);
        if !scoped || !busy || deadline.is_some_and(|end| clock.now() + LEASE_BACKOFF >= end) {
            break;
        }
        clock.wait(LEASE_BACKOFF);
        if deadline.is_some_and(|end| clock.now() >= end) {
            break;
        }
        result = attempt()?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests;
