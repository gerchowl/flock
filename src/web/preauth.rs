//! UID-scoped HTTP/hello leases and fixed one-second failed-hello windows.
//! Unknown attribution shares one bucket and never receives the operator bucket.
use super::wire::timing::Clock;
use crate::api::error_code::ErrorCode;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// A single peer cannot exhaust the global backstop; authenticated sessions cap at 16.
const PER_UID: usize = 16;
const GLOBAL: usize = 128;
const PEERS: usize = 64;
const WINDOW: Duration = Duration::from_secs(1);
struct Entry {
    active: usize,
    refusing: bool,
    failures: usize,
    start: Instant,
}
/// Bounded pending-admission and fixed-window accounting for a kernel UID.
/// The UID attribution design is held for an owner decision on #2500.
pub(super) struct Budget {
    entries: Mutex<HashMap<Option<u32>, Entry>>,
    clock: Arc<dyn Clock>,
}
/// Shared across the HTTP task and its upgraded socket; dropped after verified hello.
pub(super) struct Ticket {
    budget: Arc<Budget>,
    uid: Option<u32>,
    refusal: bool,
}
impl Budget {
    /// Use the transport's monotonic clock for exact fixed-window boundaries.
    pub(super) fn new(clock: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(HashMap::new()),
            clock,
        })
    }
    /// Own one pending connection, refusing that UID's exhausted budget.
    pub(super) fn acquire(self: &Arc<Self>, uid: Option<u32>) -> Result<Arc<Ticket>, ErrorCode> {
        let now = self.clock.now();
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| ErrorCode::PreauthCapacity)?;
        entries.retain(|_, entry| entry.active != 0 || now < entry.start + WINDOW);
        if entries.values().map(|e| e.active).sum::<usize>() >= GLOBAL
            || (!entries.contains_key(&uid) && entries.len() >= PEERS)
        {
            return Err(ErrorCode::PreauthCapacity);
        }
        let entry = entries.entry(uid).or_insert(Entry {
            active: 0,
            refusing: false,
            failures: 0,
            start: now,
        });
        if now >= entry.start + WINDOW {
            entry.start = now;
            entry.failures = 0;
        }
        if entry.failures >= 10 {
            return Err(ErrorCode::AuthRateLimited);
        }
        if entry.active >= PER_UID {
            return Err(ErrorCode::PreauthCapacity);
        }
        entry.active += 1;
        Ok(Arc::new(Ticket {
            budget: self.clone(),
            uid,
            refusal: false,
        }))
    }
    /// One bounded HTTP refusal per peer; repeated rejected sockets cannot fill
    /// the global HTTP task cap while another peer needs admission.
    pub(super) fn reject(self: &Arc<Self>, uid: Option<u32>) -> Option<Arc<Ticket>> {
        let mut entries = self.entries.lock().ok()?;
        let entry = entries.get_mut(&uid)?;
        if entry.refusing {
            return None;
        }
        entry.refusing = true;
        entry.active += 1;
        Some(Arc::new(Ticket {
            budget: self.clone(),
            uid,
            refusal: true,
        }))
    }
}
impl Ticket {
    /// Count a refused hello in this peer's fixed failure window.
    pub(super) fn failed(&self) {
        if let Ok(mut entries) = self.budget.entries.lock() {
            if let Some(entry) = entries.get_mut(&self.uid) {
                let now = self.budget.clock.now();
                if now >= entry.start + WINDOW {
                    entry.start = now;
                    entry.failures = 0;
                }
                entry.failures = entry.failures.saturating_add(1);
            }
        }
    }
}
impl Drop for Ticket {
    fn drop(&mut self) {
        if let Ok(mut entries) = self.budget.entries.lock() {
            if let Some(entry) = entries.get_mut(&self.uid) {
                entry.active -= 1;
                if self.refusal {
                    entry.refusing = false;
                }
            }
        }
    }
}
/// Upgrade refusals use the registry's status and retry class, never bare 503s.
pub(super) fn refusal(code: ErrorCode) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        axum::http::StatusCode::from_u16(code.http_status())
            .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR),
        [
            (axum::http::header::CACHE_CONTROL, "no-store"),
            (axum::http::header::CONNECTION, "close"),
        ],
        axum::Json(
            serde_json::json!({"error":{"code":code.as_str(),"retry":code.retry().as_str()}}),
        ),
    )
        .into_response()
}
#[cfg(test)]
mod tests;
