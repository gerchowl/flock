//! Fleet pause / resume (#175 phase 5 / S3 commit 3 · US-9).
//!
//! One global switch that halts the SCHEDULER and MESSAGE DELIVERY without
//! silencing human agency. Persisted to `session::data_dir()/pause.json`
//! so a paused fleet survives a `flk server stop && flk server start`.
//!
//! # What pause gates (audit-greppable — grep `US-9` in the tree)
//!
//! 1. Check runner tick — [`App::dispatch_due_script_checks`] early-returns
//!    at the top when paused, so no built-in fold runs, no script check
//!    dispatches, and no heartbeat lands.
//! 2. The message WAKE — [`App::handle_msg_wake`] reports zero when paused,
//!    so no agent is interrupted about its inbox. ADR-0008 (#216) deleted the
//!    scheduled drain this used to name (`deliver_due_messages`) along with
//!    the keystrokes it required; the stop-hook wake that replaced it went
//!    ungated until #316. The TTL sweep
//!    ([`App::expire_undeliverable_messages`]) is halted too, so a paused
//!    fleet does not quietly age messages out either.
//! 3. Cron fires ([`App::dispatch_cron_fires`], S1) and the scheduled
//!    reap / quarantine tick ([`App::evaluate_reap_check`], S2) — both
//!    gated transitively: they are dispatched from inside
//!    [`App::dispatch_due_script_checks`], below its pause early-return,
//!    so a paused fleet fires no scheduled run and quarantines nothing.
//!
//! # What pause does NOT gate
//!
//! - Human keystrokes into the terminal panes: pause is not a keyboard
//!   lock. If an operator wants to talk to a paused agent, they still can.
//! - `agent.send` and other human-invoked verbs (`flk agent fork`,
//!   `flk msg send`): pause halts the scheduler and delivery, not human
//!   agency. The CLI help text says so out loud.
//!
//! # Resume semantics
//!
//! Resume does not replay missed work. The scheduler starts ticking again;
//! script checks re-arm from their next `next_due`; mailbox drain runs on
//! the next boundary. Missed cron fires are collapsed to at most one fire
//! at wake time by the existing "asleep-across-intervals" rule
//! (`runner::CheckRunner::complete`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The persisted pause record. Kept a struct (not a bool) so a resume from
/// a stopped-then-restarted server can render "paused since <ts> because
/// <reason>" without another RPC round-trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(crate) struct FleetPauseState {
    /// True → all three gates fire early. Persisted so restarts stay paused.
    pub paused: bool,
    /// ms-since-epoch the current pause started; zero when not paused. Kept
    /// even when `paused` flips back to false so a subsequent audit can see
    /// the last window (overwritten on the next pause).
    pub since_ms: u64,
    /// Operator-supplied reason (`--reason "kernel bump"`). Empty is fine;
    /// UI renders "no reason given" then.
    pub reason: String,
}

impl FleetPauseState {
    pub(crate) fn load_from(path: &Path) -> Self {
        let Ok(contents) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str::<Self>(&contents).unwrap_or_default()
    }

    pub(crate) fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_vec_pretty(self)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        // Atomic-ish write: temp file + rename. Good enough for a one-line
        // config that a wedged rename would just fall back to the old value.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, path)
    }
}

/// Standard location for the persisted pause record.
pub(crate) fn default_pause_path() -> PathBuf {
    crate::session::data_dir().join("pause.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_loads_as_unpaused_default() {
        let tmp = std::env::temp_dir().join(format!(
            "flock-pause-missing-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        assert!(!tmp.exists());
        let state = FleetPauseState::load_from(&tmp);
        assert_eq!(state, FleetPauseState::default());
        assert!(!state.paused);
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!(
            "flock-pause-rt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pause.json");
        let state = FleetPauseState {
            paused: true,
            since_ms: 1_723_000_000_000,
            reason: "kernel bump".into(),
        };
        state.save_to(&path).expect("write pause.json");
        let restored = FleetPauseState::load_from(&path);
        assert_eq!(restored, state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pause_survives_a_synthetic_restart_via_disk_round_trip() {
        // Simulates the "write pause.json → stop server → start server →
        // load pause.json" cycle the App boot performs. If pause.json says
        // paused, a fresh construction must observe it as paused.
        let dir = std::env::temp_dir().join(format!(
            "flock-pause-restart-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pause.json");
        let before = FleetPauseState {
            paused: true,
            since_ms: 42,
            reason: "restart-test".into(),
        };
        before.save_to(&path).unwrap();
        drop(before);
        let after = FleetPauseState::load_from(&path);
        assert!(after.paused, "restart-loaded state must stay paused");
        assert_eq!(after.since_ms, 42);
        assert_eq!(after.reason, "restart-test");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_loads_as_default_never_panics() {
        let dir = std::env::temp_dir().join(format!(
            "flock-pause-corrupt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pause.json");
        std::fs::write(&path, "not json {").unwrap();
        let restored = FleetPauseState::load_from(&path);
        assert_eq!(restored, FleetPauseState::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
