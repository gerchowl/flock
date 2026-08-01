//! Per-host mechanical checks (#175 phase 4).
//!
//! The check runner OWNS the debounce state machine and emits `FireDecision`s;
//! it never fires notifications itself (P1 — mechanical gates). Actions are
//! executed by the App tick which folds `FireDecision`s into the existing
//! notification / event paths.

pub(crate) mod blocked_alert;
pub(crate) mod config;
pub(crate) mod hibernation;
pub(crate) mod runner;
pub(crate) mod script;

pub(crate) use blocked_alert::{
    format_blocked_duration, BlockedAlertFire, BlockedAlertFold, BlockedPaneSnapshot,
    BLOCKED_ALERT_CHECK_NAME,
};
pub(crate) use config::{ActionSpec, ChecksConfig};
pub(crate) use hibernation::{HibernationFold, HibernationPaneSnapshot, HIBERNATION_CHECK_NAME};
pub(crate) use runner::CheckRunner;
pub(crate) use script::{run_script, Outcome};
