//! `checks.*` API handlers (#175 phase 4, CLI surface).
//!
//! - `checks.list` — enumerate the runner's script checks and each
//!   built-in's on/off state.
//! - `checks.ack` — silence a named check for its debounce window (script)
//!   or its current in-flight episodes (built-in blocked-alert).
//! - `checks.run` — force a script check due right now, out of cadence.

use std::time::Instant;

use crate::api::schema::{ChecksListEntry, ChecksNamedTarget, ResponseResult};
use crate::app::App;

use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_checks_list(&mut self, id: String) -> String {
        let mut checks: Vec<ChecksListEntry> = self
            .checks_runner
            .list_entries()
            .into_iter()
            .map(|entry| ChecksListEntry {
                name: entry.name,
                kind: "script".to_string(),
                state: if entry.acked {
                    "acked".into()
                } else if matches!(entry.last_outcome, Some("error")) {
                    "errored".into()
                } else {
                    "enabled".into()
                },
                consecutive_fails: Some(entry.consecutive_fails),
                last_outcome: entry.last_outcome.map(str::to_string),
            })
            .collect();
        // Built-ins: one row each so `flk checks list` shows the full
        // running set at a glance.
        let cfg = &self.state.config.checks;
        checks.push(ChecksListEntry {
            name: crate::checks::BLOCKED_ALERT_CHECK_NAME.to_string(),
            kind: "blocked_alert".into(),
            state: state_label(cfg.blocked_alert.enable),
            consecutive_fails: None,
            last_outcome: None,
        });
        checks.push(ChecksListEntry {
            name: crate::checks::HIBERNATION_CHECK_NAME.to_string(),
            kind: "hibernation".into(),
            state: state_label(cfg.hibernation.enable),
            consecutive_fails: None,
            last_outcome: None,
        });
        checks.push(ChecksListEntry {
            name: crate::checks::ISSUE_GUARD_CHECK_NAME.to_string(),
            kind: "issue_guard".into(),
            state: state_label(cfg.issue_guard.enable && !cfg.issue_guard.repos.is_empty()),
            consecutive_fails: None,
            last_outcome: None,
        });
        encode_success(id, ResponseResult::ChecksList { checks })
    }

    pub(super) fn handle_checks_ack(&mut self, id: String, target: ChecksNamedTarget) -> String {
        let now = Instant::now();
        if target.name == crate::checks::BLOCKED_ALERT_CHECK_NAME {
            let snapshots = self.collect_blocked_alert_snapshots_public();
            self.blocked_alert.ack_current_episodes(&snapshots);
            return encode_success(id, ResponseResult::Ok {});
        }
        if self.checks_runner.ack(&target.name, now) {
            return encode_success(id, ResponseResult::Ok {});
        }
        encode_error(
            id,
            "check_not_found",
            format!("no check named {}", target.name),
        )
    }

    pub(super) fn handle_checks_run(&mut self, id: String, target: ChecksNamedTarget) -> String {
        let now = Instant::now();
        if !self.checks_runner.force_run_now(&target.name, now) {
            return encode_error(
                id,
                "check_not_found",
                format!("no runnable script check named {}", target.name),
            );
        }
        self.checks_next_deadline = Some(now);
        encode_success(id, ResponseResult::Ok {})
    }
}

fn state_label(enabled: bool) -> String {
    if enabled {
        "enabled".to_string()
    } else {
        "disabled".to_string()
    }
}
