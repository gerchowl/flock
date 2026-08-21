//! `checks.*` API handlers (#175 phase 4, CLI surface).
//!
//! - `checks.list` — enumerate every scheduled check: the runner's script
//!   checks AND its crons (#330), plus one row per built-in. Rows carry
//!   `next_fire_ms` so "when does this run next" is answerable without
//!   reading the durable event log after the fact.
//! - `checks.ack` — silence a named check for its debounce window (script)
//!   or its current in-flight episodes (built-in blocked-alert).
//! - `checks.run` — force a script check due right now, out of cadence.

use std::time::Instant;

use crate::api::schema::{ChecksListEntry, ChecksNamedTarget, ResponseResult};
use crate::app::App;

use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_checks_list(&mut self, id: String) -> String {
        // One wall/monotonic anchor for the whole response: every derived
        // `next_fire_ms` is projected from the SAME pair, so two rows in one
        // listing can be compared against each other.
        let now = Instant::now();
        let now_wall_ms = crate::checks::runner::current_wall_ms();
        let mut checks: Vec<ChecksListEntry> = self
            .checks_runner
            .list_entries(now, now_wall_ms)
            .into_iter()
            .map(|entry| ChecksListEntry {
                name: entry.name,
                kind: entry.kind.to_string(),
                state: if entry.acked {
                    "acked".into()
                } else if matches!(entry.last_outcome, Some("error")) {
                    "errored".into()
                } else {
                    "enabled".into()
                },
                consecutive_fails: Some(entry.consecutive_fails),
                last_outcome: entry.last_outcome.map(str::to_string),
                next_fire_ms: entry.next_fire_wall_ms,
                last_fire_ms: entry.last_fire_wall_ms,
                missed_fires: (entry.missed_fires > 0).then_some(entry.missed_fires),
                cron_expr: entry.cron_expr,
                cron_tz: entry.cron_tz.map(|tz| match tz {
                    crate::checks::CronTz::Local => "local".to_string(),
                    crate::checks::CronTz::Utc => "utc".to_string(),
                }),
                cadence_secs: None,
            })
            .collect();
        // Built-ins: one row each so `flk checks list` shows the full
        // running set at a glance.
        let cfg = &self.state.config.checks;
        checks.push(ChecksListEntry {
            name: crate::checks::BLOCKED_ALERT_CHECK_NAME.to_string(),
            kind: "blocked_alert".into(),
            state: state_label(cfg.blocked_alert.enable),
            ..ChecksListEntry::builtin()
        });
        checks.push(ChecksListEntry {
            name: crate::checks::HIBERNATION_CHECK_NAME.to_string(),
            kind: "hibernation".into(),
            state: state_label(cfg.hibernation.enable),
            ..ChecksListEntry::builtin()
        });
        checks.push(ChecksListEntry {
            name: crate::checks::ISSUE_GUARD_CHECK_NAME.to_string(),
            kind: "issue_guard".into(),
            state: state_label(cfg.issue_guard.enable && !cfg.issue_guard.repos.is_empty()),
            cadence_secs: Some(cfg.issue_guard.poll_secs),
            ..ChecksListEntry::builtin()
        });
        // Reap (#175 S2) mutates worktrees — it quarantines — so leaving it
        // off this surface was the worst of the omissions (#330). `state`
        // folds the manifest gate: an enabled reap whose last verdict was
        // NOT Ok is reporting `gated`, because it yields zero candidates and
        // "enabled" would be a lie.
        let reap_gated = self
            .reap_last_verdict
            .as_deref()
            .is_some_and(|verdict| verdict != "Ok");
        checks.push(ChecksListEntry {
            name: crate::checks::REAP_CHECK_NAME.to_string(),
            kind: "reap".into(),
            state: if !(cfg.enable && cfg.reap.enable) {
                "disabled".into()
            } else if reap_gated {
                "gated".into()
            } else {
                "enabled".into()
            },
            last_outcome: self.reap_last_verdict.clone(),
            cadence_secs: Some(cfg.reap.cadence_secs),
            next_fire_ms: self.reap_next_deadline.map(|deadline| {
                now_wall_ms.saturating_add(
                    deadline
                        .saturating_duration_since(now)
                        .as_millis()
                        .min(u128::from(u64::MAX)) as u64,
                )
            }),
            ..ChecksListEntry::builtin()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::ChecksConfig;

    fn app_with_checks(checks: ChecksConfig) -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let config = crate::config::Config {
            checks,
            ..crate::config::Config::default()
        };
        App::new(&config, true, None, api_rx, crate::api::EventHub::default())
    }

    fn rows(app: &mut App) -> Vec<serde_json::Value> {
        let raw = app.handle_checks_list("t".into());
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid json response");
        parsed["result"]["checks"]
            .as_array()
            .expect("checks array")
            .clone()
    }

    fn row_named<'a>(rows: &'a [serde_json::Value], name: &str) -> &'a serde_json::Value {
        rows.iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| panic!("no row named {name} in {rows:?}"))
    }

    /// #330: reap is the one built-in that MUTATES worktrees (quarantine),
    /// and it was the one built-in missing from the listing entirely.
    #[test]
    fn reap_row_is_listed_and_reports_disabled_by_default() {
        let mut app = app_with_checks(ChecksConfig::default());
        let rows = rows(&mut app);
        let reap = row_named(&rows, crate::checks::REAP_CHECK_NAME);
        assert_eq!(reap["kind"], "reap");
        // Default is off — see `ReapConfig::default`.
        assert_eq!(reap["state"], "disabled");
    }

    /// An enabled reap whose manifest gate is tripping yields ZERO
    /// candidates. Reporting that as "enabled" is the confusing lie this
    /// row exists to prevent.
    #[test]
    fn enabled_reap_behind_a_tripped_manifest_gate_reads_as_gated() {
        let checks = ChecksConfig {
            reap: crate::checks::config::ReapConfig {
                enable: true,
                ..Default::default()
            },
            ..ChecksConfig::default()
        };
        let mut app = app_with_checks(checks);

        // Clean verdict first: enabled means enabled.
        app.reap_last_verdict = Some("Ok".to_string());
        let clean = rows(&mut app);
        assert_eq!(
            row_named(&clean, crate::checks::REAP_CHECK_NAME)["state"],
            "enabled"
        );

        // Drifted verdict: the row must say so.
        app.reap_last_verdict = Some("HookHalfState".to_string());
        let drifted = rows(&mut app);
        let reap = row_named(&drifted, crate::checks::REAP_CHECK_NAME);
        assert_eq!(reap["state"], "gated");
        assert_eq!(reap["last_outcome"], "HookHalfState");
    }

    /// A reap that has never ticked has no verdict. That must not be
    /// reported as a clean gate — absence of evidence is not Ok.
    #[test]
    fn reap_with_no_verdict_yet_is_not_reported_as_gated() {
        let checks = ChecksConfig {
            reap: crate::checks::config::ReapConfig {
                enable: true,
                ..Default::default()
            },
            ..ChecksConfig::default()
        };
        let mut app = app_with_checks(checks);
        let rows = rows(&mut app);
        let reap = row_named(&rows, crate::checks::REAP_CHECK_NAME);
        assert_eq!(reap["state"], "enabled");
        assert!(
            reap.get("last_outcome").is_none(),
            "no verdict yet must serialise as absent, not as a fabricated Ok"
        );
        assert_eq!(reap["cadence_secs"], 3_600);
    }

    /// The end-to-end shape of the #330 fix: a declared cron reaches the
    /// API response with its expression and next fire.
    #[test]
    fn declared_crons_reach_the_api_response() {
        let checks = ChecksConfig {
            crons: vec![crate::checks::config::CronCheck {
                name: "nightly-digest".into(),
                expr: crate::checks::cron::CronExpr::parse("0 3 * * *").unwrap(),
                tz: crate::checks::CronTz::Utc,
                on_fire: crate::checks::ActionSpec::Event {
                    label: "nightly".into(),
                },
            }],
            ..ChecksConfig::default()
        };
        let mut app = app_with_checks(checks);
        let rows = rows(&mut app);
        let cron = row_named(&rows, "nightly-digest");
        assert_eq!(cron["kind"], "cron");
        assert_eq!(cron["cron_expr"], "0 3 * * *");
        assert_eq!(cron["cron_tz"], "utc");
        assert!(
            cron["next_fire_ms"].as_u64().is_some_and(|ms| ms > 0),
            "a listed cron must say when it next fires"
        );
    }
}
