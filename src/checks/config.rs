//! `[checks]` configuration section (#175 phase 4).
//!
//! Per-host, per-node section that declares the built-in mechanical checks
//! (blocked-alert, hibernation, issue-guard) and any user-defined
//! `[[checks.script]]` entries. The runner (`super::runner::CheckRunner`) never
//! executes actions itself — it emits `FireDecision`s — so this section models
//! only the DECLARATIVE surface: knobs, script argv, and a CLOSED `ActionSpec`
//! enum (#175 P7, no shell strings ever).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::cron::{CronExpr, CronTz};
use crate::api::schema::NotificationShowSound;

/// Default cap on concurrent script check runs — a small backstop against a
/// user pasting a slow script and starving the loop, not a policy dial.
pub const DEFAULT_MAX_CONCURRENT: usize = 4;
/// Default minimum tick — bounds how often the scheduler wakes for check work
/// even if a script sets a tiny interval.
pub const DEFAULT_MIN_TICK_SECS: u64 = 10;
/// Default heartbeat interval — emits a durable `ChecksHeartbeat` so an
/// external observer can tell the runner is alive.
pub const DEFAULT_HEARTBEAT_SECS: u64 = 60;
/// Blocked-alert defaults (from the phase-4 design).
pub const DEFAULT_BLOCKED_ALERT_THRESHOLD_SECS: u64 = 1800;
/// Reap defaults (S2 scheduled reap builtin). 24h idle threshold; a slow
/// cadence so a manifest-drift trip doesn't spam. Off by default — the
/// integration-manifest gate is precisely the "don't autopilot until the
/// operator opts in" surface (P8 gradual-rollout).
pub const DEFAULT_REAP_IDLE_THRESHOLD_SECS: u64 = 86_400;
pub const DEFAULT_REAP_CADENCE_SECS: u64 = 3_600;
/// Hibernation defaults (from the phase-4 design).
pub const DEFAULT_HIBERNATION_IDLE_THRESHOLD_SECS: u64 = 3600;
pub const DEFAULT_HIBERNATION_DONE_THRESHOLD_SECS: u64 = 21600;
pub const DEFAULT_HIBERNATION_MIN_SECS_SINCE_LAST_STOP: u64 = 60;
/// Issue-guard defaults (from the phase-4 design).
pub const DEFAULT_ISSUE_GUARD_OWNER: &str = "gerchowl";
pub const DEFAULT_ISSUE_GUARD_GH_BIN: &str = "gh";
pub const DEFAULT_ISSUE_GUARD_POLL_SECS: u64 = 60;

/// Per-script defaults.
pub const DEFAULT_SCRIPT_INTERVAL_SECS: u64 = 300;
pub const DEFAULT_SCRIPT_TIMEOUT_SECS: u64 = 20;
pub const DEFAULT_SCRIPT_DEBOUNCE: u32 = 3;

/// Root of the `[checks]` config section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ChecksConfig {
    /// Master switch — off disables ALL check work, built-in and script.
    pub enable: bool,
    /// Cap on concurrent in-flight script runs.
    pub max_concurrent: usize,
    /// Minimum tick period. Any `interval_secs` below this is refused via
    /// `diagnostics()` (clamped in the runner) so a typo can't spin the loop.
    pub min_tick_secs: u64,
    /// How often the runner emits a durable `ChecksHeartbeat` event.
    pub heartbeat_secs: u64,
    /// Blocked-alert built-in check.
    pub blocked_alert: BlockedAlertConfig,
    /// Hibernation built-in check.
    pub hibernation: HibernationConfig,
    /// Issue-guard built-in check.
    pub issue_guard: IssueGuardConfig,
    /// Scheduled reap built-in (S2). Default OFF — enabling requires the
    /// operator has verified their installed integrations first (the
    /// runner refuses to reap while `verify_integration_manifest` trips).
    pub reap: ReapConfig,
    /// User-declared `[[checks.script]]` entries.
    #[serde(default, rename = "script", skip_serializing_if = "Vec::is_empty")]
    pub scripts: Vec<ScriptCheck>,
    /// User-declared `[[checks.cron]]` entries — scheduled predicates (S1).
    /// Each entry fires on its cron schedule with missed-fires-while-asleep
    /// collapsing to ONE fire per wake (`CheckRunner::tick_crons`).
    #[serde(default, rename = "cron", skip_serializing_if = "Vec::is_empty")]
    pub crons: Vec<CronCheck>,
    /// #175 S3 commit 4 (ops): opt-in installation of a per-worktree
    /// `prepare-commit-msg` hook that stamps `Agent-Run: <FLOCK_RUN_ID>`
    /// on every commit made from a scheduler-spawned or forked pane.
    ///
    /// OFF by default. We do NOT silently write hooks into user repos —
    /// operators must opt in. When on, `agent.fork` (and, once the
    /// scheduler-spawn path lands, S1's `SpawnAgent` action) writes the
    /// hook into the freshly-created child worktree and never touches an
    /// existing user-authored `prepare-commit-msg`.
    #[serde(default)]
    pub run_trailers: bool,
}

impl Default for ChecksConfig {
    fn default() -> Self {
        Self {
            enable: true,
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            min_tick_secs: DEFAULT_MIN_TICK_SECS,
            heartbeat_secs: DEFAULT_HEARTBEAT_SECS,
            blocked_alert: BlockedAlertConfig::default(),
            hibernation: HibernationConfig::default(),
            issue_guard: IssueGuardConfig::default(),
            reap: ReapConfig::default(),
            scripts: Vec::new(),
            crons: Vec::new(),
            run_trailers: false,
        }
    }
}

/// Scheduled reap built-in (#175 S2). Off by default; when enabled, every
/// tick first calls `verify_integration_manifest` — a HookHalfState /
/// Outdated verdict emits a throttled `CheckErrored` + toast and yields
/// ZERO candidates. When Ok, worktree workspaces whose panes are all
/// Idle+seen past `idle_threshold_secs` and not operator-focused are
/// classified through the scheduled variant of the kill machinery and
/// dispatched (Quarantine for skip-because-unmerged/dirty; the existing
/// merge-gated delete rules still apply).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ReapConfig {
    pub enable: bool,
    pub cadence_secs: u64,
    pub idle_threshold_secs: u64,
}

impl Default for ReapConfig {
    fn default() -> Self {
        Self {
            enable: false,
            cadence_secs: DEFAULT_REAP_CADENCE_SECS,
            idle_threshold_secs: DEFAULT_REAP_IDLE_THRESHOLD_SECS,
        }
    }
}

/// One `[[checks.cron]]` entry (#175 S1). Parse-time validation runs through
/// `CronExpr::parse` in the field's serde impl, so a malformed expression
/// bubbles up as a deserialize error — loud rejection at config load, no
/// silent fallback. `name` + `expr` are required; `tz` defaults to `local`
/// and `on_fire` to the closed `ActionSpec::default()`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CronCheck {
    pub name: String,
    pub expr: CronExpr,
    #[serde(default)]
    pub tz: CronTz,
    #[serde(default)]
    pub on_fire: ActionSpec,
}

/// Built-in blocked-alert check: fire when an agent pane has been Blocked for
/// longer than `threshold_secs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct BlockedAlertConfig {
    pub enable: bool,
    pub threshold_secs: u64,
}

impl Default for BlockedAlertConfig {
    fn default() -> Self {
        Self {
            enable: true,
            threshold_secs: DEFAULT_BLOCKED_ALERT_THRESHOLD_SECS,
        }
    }
}

/// Built-in hibernation check: fire on a pane that has been Idle (or Done)
/// longer than its threshold, and hasn't been stopped very recently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct HibernationConfig {
    pub enable: bool,
    pub idle_threshold_secs: u64,
    pub done_threshold_secs: u64,
    pub min_secs_since_last_stop: u64,
}

impl Default for HibernationConfig {
    fn default() -> Self {
        Self {
            enable: false,
            idle_threshold_secs: DEFAULT_HIBERNATION_IDLE_THRESHOLD_SECS,
            done_threshold_secs: DEFAULT_HIBERNATION_DONE_THRESHOLD_SECS,
            min_secs_since_last_stop: DEFAULT_HIBERNATION_MIN_SECS_SINCE_LAST_STOP,
        }
    }
}

/// Built-in issue-guard check: poll `gh` for open issues on the configured
/// repos and fire when the guardrail conditions the later phases wire up
/// trip. Phase 4 only ships the config surface; the guard logic itself is a
/// later commit's problem.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct IssueGuardConfig {
    pub enable: bool,
    pub owner: String,
    pub repos: Vec<String>,
    pub gh_bin: String,
    pub poll_secs: u64,
    /// Cap on open issues fetched per poll. Older issues beyond the cap are
    /// never evaluated — raise it for a busy repo.
    #[serde(default = "default_max_issues")]
    pub max_issues: u32,
}

fn default_max_issues() -> u32 {
    50
}

impl Default for IssueGuardConfig {
    fn default() -> Self {
        Self {
            enable: true,
            owner: DEFAULT_ISSUE_GUARD_OWNER.to_string(),
            repos: Vec::new(),
            gh_bin: DEFAULT_ISSUE_GUARD_GH_BIN.to_string(),
            poll_secs: DEFAULT_ISSUE_GUARD_POLL_SECS,
            max_issues: default_max_issues(),
        }
    }
}

/// One `[[checks.script]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ScriptCheck {
    /// Unique name — reads back on every emitted event.
    pub name: String,
    /// Executable to spawn. Must be absolute OR live under
    /// `~/.flock/checks/`; `diagnostics()` refuses anything else.
    pub program: PathBuf,
    /// argv passed after `program` (never shell-parsed — P7).
    pub args: Vec<String>,
    /// Nominal cadence between runs.
    pub interval_secs: u64,
    /// Hard wall-clock cap; the executor SIGTERMs at `timeout_secs` and
    /// SIGKILLs at `+5s`.
    pub timeout_secs: u64,
    /// Consecutive Fire outcomes required before the runner emits a
    /// `FireDecision` for the current episode.
    pub debounce: u32,
    /// Action the runner returns when the check fires.
    pub on_fire: ActionSpec,
    /// Explicit env allowlist passed to the child (in addition to
    /// `FLOCK_CHECK_NAME`). RUST_LOG is always cleared by the executor.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
}

impl Default for ScriptCheck {
    fn default() -> Self {
        Self {
            name: String::new(),
            program: PathBuf::new(),
            args: Vec::new(),
            interval_secs: DEFAULT_SCRIPT_INTERVAL_SECS,
            timeout_secs: DEFAULT_SCRIPT_TIMEOUT_SECS,
            debounce: DEFAULT_SCRIPT_DEBOUNCE,
            on_fire: ActionSpec::default(),
            env: HashMap::new(),
        }
    }
}

/// Closed action enum (#175 P7). NEVER a shell string; new action shapes must
/// land as a new variant here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionSpec {
    /// Route through the existing notification path
    /// (`ui.toast` delivery policy applies).
    Notify {
        title: String,
        #[serde(default, skip_serializing_if = "NotificationShowSound::is_none")]
        sound: NotificationShowSound,
    },
    /// Emit a durable `CheckFired { episode: <label> }` event and nothing
    /// else — a hook point for external listeners.
    Event { label: String },
    /// #175 phase 5 / S3 commit 2: fold the durable event log into a
    /// self-contained HTML digest and write it under `session::data_dir()`.
    /// `path_template` accepts the `{date}` placeholder (YYYY-MM-DD).
    /// P1: no LLM in the loop — the fold is pure.
    WriteDigest {
        /// Path relative to `session::data_dir()/digest/`, or absolute.
        /// `{date}` expands to the local YYYY-MM-DD at run time.
        #[serde(default = "default_digest_path_template")]
        path_template: String,
    },
}

fn default_digest_path_template() -> String {
    "{date}.html".to_string()
}

impl Default for ActionSpec {
    fn default() -> Self {
        // Match the design's example schema: a Notify with an empty title
        // and the default (silent) sound. A `[[checks.script]]` that omits
        // `on_fire` is unusual but the runner treats it as a no-op title.
        Self::Notify {
            title: String::new(),
            sound: NotificationShowSound::default(),
        }
    }
}

/// Path forms accepted for `[[checks.script]] program`. Absolute paths pass
/// through untouched; a `~/.flock/checks/<...>` path is expanded so downstream
/// (the executor) can canonicalize. Anything else (bare command name, `../`
/// escape, `/etc/passwd`) is refused via `diagnostics()`.
///
/// Public visibility inside the crate: the executor and tests both consult
/// this helper so the diagnostic and the run-time acceptance stay in lockstep.
pub(crate) fn accepted_program_path(program: &std::path::Path) -> Option<PathBuf> {
    if program.as_os_str().is_empty() {
        return None;
    }
    if program.is_absolute() {
        return Some(program.to_path_buf());
    }
    // The tilde form is the only relative shape we accept: `~/.flock/checks/…`.
    let raw = program.to_string_lossy();
    let expanded = expand_flock_checks_tilde(raw.as_ref())?;
    Some(expanded)
}

fn expand_flock_checks_tilde(raw: &str) -> Option<PathBuf> {
    let rest = raw.strip_prefix("~/.flock/checks/")?;
    if rest.is_empty() {
        return None;
    }
    // Reject the classic `..` escape at the source — a check that tries to
    // hop out of `~/.flock/checks/` is refused, and canonicalization on the
    // executor side is a belt-and-braces second line.
    if rest.split('/').any(|segment| segment == "..") {
        return None;
    }
    let home = std::env::var_os("HOME")?;
    let mut path = PathBuf::from(home);
    path.push(".flock/checks");
    path.push(rest);
    Some(path)
}

impl ChecksConfig {
    /// Bounds + relative-sanity diagnostics, mirroring `GossipConfig::diagnostics()`.
    ///
    /// The runner clamps the diagnosed values so the fleet still ticks — the
    /// diagnostic is the surface, not the enforcement.
    pub fn diagnostics(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.min_tick_secs < 1 {
            out.push(format!(
                "[checks] min_tick_secs ({}) is less than 1; using 1s so the runner keeps ticking",
                self.min_tick_secs
            ));
        }
        if self.heartbeat_secs < 1 {
            out.push(format!(
                "[checks] heartbeat_secs ({}) is less than 1; using 1s so heartbeats keep firing",
                self.heartbeat_secs
            ));
        }
        if self.max_concurrent == 0 {
            out.push(
                "[checks] max_concurrent is 0; no script check would ever run — using 1"
                    .to_string(),
            );
        }

        let mut seen = std::collections::HashSet::new();
        for script in &self.scripts {
            let name = script.name.trim();
            if name.is_empty() {
                out.push("[[checks.script]] entry is missing name; entry ignored".to_string());
                continue;
            }
            if !seen.insert(name.to_string()) {
                out.push(format!(
                    "[[checks.script]] duplicate name \"{name}\"; later entry ignored"
                ));
                continue;
            }
            if script.interval_secs < self.min_tick_secs.max(1) {
                out.push(format!(
                    "[[checks.script]] \"{name}\" interval_secs ({}) is less than [checks] min_tick_secs ({}); clamped",
                    script.interval_secs,
                    self.min_tick_secs.max(1)
                ));
            }
            if script.timeout_secs < 1 {
                out.push(format!(
                    "[[checks.script]] \"{name}\" timeout_secs ({}) is less than 1; using 1s",
                    script.timeout_secs
                ));
            }
            if script.debounce == 0 {
                out.push(format!(
                    "[[checks.script]] \"{name}\" debounce is 0; using 1 so a single fire still emits"
                ));
            }
            if accepted_program_path(&script.program).is_none() {
                out.push(format!(
                    "[[checks.script]] \"{name}\" program {:?} is not absolute and not under ~/.flock/checks/; entry ignored",
                    script.program
                ));
            }
        }

        // Cron entries share the script-name namespace: two different
        // predicate kinds with the same name would confuse ack/list/run.
        for cron in &self.crons {
            let name = cron.name.trim();
            if name.is_empty() {
                out.push("[[checks.cron]] entry is missing name; entry ignored".to_string());
                continue;
            }
            if !seen.insert(name.to_string()) {
                out.push(format!(
                    "[[checks.cron]] duplicate name \"{name}\"; later entry ignored"
                ));
                continue;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_shipped_shape() {
        let checks = ChecksConfig::default();
        assert!(checks.enable);
        assert_eq!(checks.max_concurrent, DEFAULT_MAX_CONCURRENT);
        assert_eq!(checks.min_tick_secs, DEFAULT_MIN_TICK_SECS);
        assert_eq!(checks.heartbeat_secs, DEFAULT_HEARTBEAT_SECS);
        assert!(checks.blocked_alert.enable);
        assert_eq!(
            checks.blocked_alert.threshold_secs,
            DEFAULT_BLOCKED_ALERT_THRESHOLD_SECS
        );
        assert!(!checks.hibernation.enable);
        assert_eq!(
            checks.hibernation.idle_threshold_secs,
            DEFAULT_HIBERNATION_IDLE_THRESHOLD_SECS
        );
        assert!(checks.issue_guard.enable);
        assert_eq!(checks.issue_guard.owner, DEFAULT_ISSUE_GUARD_OWNER);
        assert!(checks.scripts.is_empty());
        // Sane defaults produce no diagnostics.
        assert!(
            checks.diagnostics().is_empty(),
            "{:?}",
            checks.diagnostics()
        );
    }

    #[test]
    fn parses_full_section_and_script_entry() {
        // Rooted at the ChecksConfig level (no `[checks]` prefix) because
        // that's the shape `serde(rename = "script")` on `scripts` expects the
        // outer table to be. The section-under-Config drift test lives in
        // `config::io::tests::checks_section_survives_live_reload`.
        let toml = r#"
enable = true
max_concurrent = 2
min_tick_secs = 15
heartbeat_secs = 30

[blocked_alert]
enable = true
threshold_secs = 900

[hibernation]
enable = true
idle_threshold_secs = 1200
done_threshold_secs = 3600
min_secs_since_last_stop = 30

[issue_guard]
enable = false
owner = "gerchowl"
repos = ["flock"]
gh_bin = "/opt/homebrew/bin/gh"
poll_secs = 45

[[script]]
name = "disk-space"
program = "/usr/local/bin/free-disk-check.sh"
args = ["--min", "10G"]
interval_secs = 300
timeout_secs = 10
debounce = 2

[script.on_fire.notify]
title = "disk low"
sound = "request"

[script.env]
LC_ALL = "C"
"#;
        let checks: ChecksConfig = toml::from_str(toml).unwrap();
        assert_eq!(checks.max_concurrent, 2);
        assert_eq!(checks.min_tick_secs, 15);
        assert_eq!(checks.heartbeat_secs, 30);
        assert_eq!(checks.blocked_alert.threshold_secs, 900);
        assert!(checks.hibernation.enable);
        assert_eq!(checks.hibernation.idle_threshold_secs, 1200);
        assert!(!checks.issue_guard.enable);
        assert_eq!(checks.issue_guard.repos, vec!["flock".to_string()]);
        assert_eq!(checks.issue_guard.gh_bin, "/opt/homebrew/bin/gh");
        assert_eq!(checks.scripts.len(), 1);
        let script = &checks.scripts[0];
        assert_eq!(script.name, "disk-space");
        assert_eq!(script.args, vec!["--min".to_string(), "10G".to_string()]);
        assert_eq!(script.interval_secs, 300);
        assert_eq!(script.timeout_secs, 10);
        assert_eq!(script.debounce, 2);
        match &script.on_fire {
            ActionSpec::Notify { title, sound } => {
                assert_eq!(title, "disk low");
                assert_eq!(*sound, NotificationShowSound::Request);
            }
            other => panic!("unexpected on_fire: {other:?}"),
        }
        assert_eq!(script.env.get("LC_ALL").map(String::as_str), Some("C"));
        assert!(
            checks.diagnostics().is_empty(),
            "{:?}",
            checks.diagnostics()
        );
    }

    #[test]
    fn action_spec_event_variant_parses() {
        let toml = r#"
name = "custom"
program = "/usr/bin/true"

[on_fire.event]
label = "custom-fired"
"#;
        let script: ScriptCheck = toml::from_str(toml).unwrap();
        match script.on_fire {
            ActionSpec::Event { label } => assert_eq!(label, "custom-fired"),
            other => panic!("expected Event variant, got {other:?}"),
        }
    }

    #[test]
    fn round_trip_serde_defaults() {
        let checks = ChecksConfig::default();
        let toml_text = toml::to_string(&checks).unwrap();
        let parsed: ChecksConfig = toml::from_str(&toml_text).unwrap();
        assert_eq!(parsed, checks);
    }

    #[test]
    fn diagnostics_refuse_below_min_tick_and_invalid_program() {
        let checks = ChecksConfig {
            min_tick_secs: 0,
            heartbeat_secs: 0,
            max_concurrent: 0,
            scripts: vec![
                ScriptCheck {
                    name: String::new(),
                    program: PathBuf::from("/usr/bin/true"),
                    ..ScriptCheck::default()
                },
                ScriptCheck {
                    name: "bare".into(),
                    program: PathBuf::from("bare-cmd"),
                    ..ScriptCheck::default()
                },
                ScriptCheck {
                    name: "dup".into(),
                    program: PathBuf::from("/usr/bin/true"),
                    ..ScriptCheck::default()
                },
                ScriptCheck {
                    name: "dup".into(),
                    program: PathBuf::from("/usr/bin/true"),
                    ..ScriptCheck::default()
                },
                ScriptCheck {
                    name: "escape".into(),
                    program: PathBuf::from("~/.flock/checks/../etc/passwd"),
                    ..ScriptCheck::default()
                },
                ScriptCheck {
                    name: "fast".into(),
                    program: PathBuf::from("/usr/bin/true"),
                    interval_secs: 0,
                    timeout_secs: 0,
                    debounce: 0,
                    ..ScriptCheck::default()
                },
            ],
            ..ChecksConfig::default()
        };
        let diags = checks.diagnostics();
        assert!(
            diags.iter().any(|d| d.contains("min_tick_secs")),
            "{diags:?}"
        );
        assert!(
            diags.iter().any(|d| d.contains("heartbeat_secs")),
            "{diags:?}"
        );
        assert!(
            diags.iter().any(|d| d.contains("max_concurrent")),
            "{diags:?}"
        );
        assert!(
            diags.iter().any(|d| d.contains("missing name")),
            "{diags:?}"
        );
        assert!(
            diags
                .iter()
                .any(|d| d.contains("\"bare\"") && d.contains("not absolute")),
            "{diags:?}"
        );
        assert!(
            diags.iter().any(|d| d.contains("duplicate name")),
            "{diags:?}"
        );
        assert!(
            diags
                .iter()
                .any(|d| d.contains("\"escape\"") && d.contains("not absolute")),
            "{diags:?}"
        );
        assert!(
            diags
                .iter()
                .any(|d| d.contains("\"fast\"") && d.contains("interval_secs")),
            "{diags:?}"
        );
        assert!(
            diags
                .iter()
                .any(|d| d.contains("\"fast\"") && d.contains("timeout_secs")),
            "{diags:?}"
        );
        assert!(
            diags
                .iter()
                .any(|d| d.contains("\"fast\"") && d.contains("debounce")),
            "{diags:?}"
        );
    }

    #[test]
    fn parses_cron_section_and_rejects_bad_expression() {
        let toml = r#"
[[cron]]
name = "nightly-digest"
expr = "0 3 * * *"
tz = "utc"

[cron.on_fire.event]
label = "digest"
"#;
        let checks: ChecksConfig = toml::from_str(toml).unwrap();
        assert_eq!(checks.crons.len(), 1);
        assert_eq!(checks.crons[0].name, "nightly-digest");
        assert_eq!(checks.crons[0].tz, super::CronTz::Utc);
        assert_eq!(format!("{}", checks.crons[0].expr), "0 3 * * *");

        // A malformed expression is a hard load-time error — not a
        // diagnostic. That is the loud-rejection posture the design calls
        // for.
        let bad = r#"
[[cron]]
name = "nope"
expr = "@daily"
"#;
        let err = toml::from_str::<ChecksConfig>(bad).unwrap_err().to_string();
        assert!(err.contains("cron"), "{err}");
    }

    #[test]
    fn cron_diagnostics_flag_missing_name_and_duplicate() {
        let expr = super::CronExpr::parse("* * * * *").unwrap();
        let checks = ChecksConfig {
            crons: vec![
                super::CronCheck {
                    name: String::new(),
                    expr: expr.clone(),
                    tz: super::CronTz::Local,
                    on_fire: ActionSpec::default(),
                },
                super::CronCheck {
                    name: "dup".into(),
                    expr: expr.clone(),
                    tz: super::CronTz::Local,
                    on_fire: ActionSpec::default(),
                },
                super::CronCheck {
                    name: "dup".into(),
                    expr,
                    tz: super::CronTz::Local,
                    on_fire: ActionSpec::default(),
                },
            ],
            ..ChecksConfig::default()
        };
        let diags = checks.diagnostics();
        assert!(
            diags
                .iter()
                .any(|d| d.contains("[[checks.cron]] entry is missing name")),
            "{diags:?}"
        );
        assert!(
            diags
                .iter()
                .any(|d| d.contains("[[checks.cron]] duplicate name")),
            "{diags:?}"
        );
    }

    #[test]
    fn accepted_program_path_handles_absolute_and_tilde() {
        assert_eq!(
            accepted_program_path(std::path::Path::new("/usr/bin/true")),
            Some(PathBuf::from("/usr/bin/true"))
        );
        // Set HOME to a deterministic value for the tilde expansion.
        std::env::set_var("HOME", "/tmp/test-home");
        assert_eq!(
            accepted_program_path(std::path::Path::new("~/.flock/checks/foo.sh")),
            Some(PathBuf::from("/tmp/test-home/.flock/checks/foo.sh"))
        );
        assert_eq!(
            accepted_program_path(std::path::Path::new("~/.flock/checks/../evil")),
            None
        );
        assert_eq!(
            accepted_program_path(std::path::Path::new("relative/path")),
            None
        );
        assert_eq!(accepted_program_path(std::path::Path::new("")), None);
    }
}
