//! Per-host mechanical checks (#175 phase 4).
//!
//! The check runner OWNS the debounce state machine and emits `FireDecision`s;
//! it never fires notifications itself (P1 — mechanical gates). Actions are
//! executed by the App tick which folds `FireDecision`s into the existing
//! notification / event paths.

pub(crate) mod blocked_alert;
pub(crate) mod config;
pub(crate) mod cron;
pub(crate) mod hibernation;
pub(crate) mod issue_guard;
pub(crate) mod reap;
pub(crate) mod runner;
pub(crate) mod script;

pub(crate) use blocked_alert::{
    format_blocked_duration, BlockedAlertFire, BlockedAlertFold, BlockedPaneSnapshot,
    BLOCKED_ALERT_CHECK_NAME,
};
pub(crate) use config::{ActionSpec, ChecksConfig};
pub(crate) use cron::CronTz;
pub(crate) use hibernation::{HibernationFold, HibernationPaneSnapshot, HIBERNATION_CHECK_NAME};
pub(crate) use issue_guard::{
    GhIssue, IssueGuardFold, IssueGuardOutcome, TriggerBlock, ISSUE_GUARD_CHECK_NAME,
};
pub(crate) use reap::{
    ReapCandidate, ReapFold, ReapPaneSnapshot, ReapWorkspaceSnapshot, REAP_CHECK_NAME,
};
pub(crate) use runner::CheckRunner;
pub(crate) use script::{run_script, Outcome};
