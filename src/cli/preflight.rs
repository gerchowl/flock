//! `flk preflight` — is flock actually functional on THIS host right now?
//!
//! A host can finish `just apply` reporting success while flock is quietly
//! half-wired: before #246 the prompt-history panel was fed entirely by
//! lifecycle hooks, so a host whose integration never installed showed an
//! empty panel — indistinguishable from "no history yet" (#263).
//!
//! The exit code is the point. It follows the contract `flk config check`
//! established (#242) so a deploy can gate on it:
//!
//! * `0` — ready
//! * `1` — advisory: works, but something wants attention
//! * `2` — blocking: a configured feature is broken here
//!
//! Deliberately offline. Preflight runs inside an activation, so it must never
//! dial a peer or fetch: it cannot be allowed to hang an apply. Peer
//! reachability belongs to `flk status`, on a different clock.
//!
//! That promise covers network DIALS, not all I/O — the checks stat paths and
//! read the config, so a `worktrees.directory` on a wedged network mount can
//! still block. Anything with an unbounded wait would need its own timeout.

use std::path::Path;

use crate::integration::IntegrationStatusKind;

/// Ready / wants attention / broken. Ordered so `max` picks the worst.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Severity {
    Ok,
    Advisory,
    Blocking,
}

impl Severity {
    fn exit_code(self) -> i32 {
        match self {
            Severity::Ok => 0,
            Severity::Advisory => super::CONFIG_CHECK_ADVISORY,
            Severity::Blocking => super::CONFIG_CHECK_PARSE_FAILURE,
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Severity::Ok => "ok  ",
            Severity::Advisory => "warn",
            Severity::Blocking => "FAIL",
        }
    }
}

/// One reported line. `detail` names the FIX, not the symptom — a preflight
/// that says "integration status mismatch" has told the reader nothing they can
/// act on.
pub(crate) struct Check {
    pub(crate) name: &'static str,
    pub(crate) severity: Severity,
    pub(crate) detail: String,
}

pub(super) fn run_preflight_command(args: &[String]) -> std::io::Result<i32> {
    let mut required: Vec<String> = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--require" => match rest.next() {
                Some(target) => {
                    let target = target.to_ascii_lowercase();
                    // A typo must not pass quietly. `--require clude` would
                    // match no target, silently demoting the blocking check
                    // the deployment asked for back to advisory — defeating
                    // the gate this flag exists to be.
                    if !known_targets().iter().any(|known| known == &target) {
                        eprintln!("flk preflight: unknown --require target {target:?}");
                        eprintln!("known targets: {}", known_targets().join(", "));
                        return Ok(super::CONFIG_CHECK_PARSE_FAILURE);
                    }
                    required.push(target);
                }
                None => {
                    eprintln!("flk preflight: --require needs a target (e.g. claude)");
                    return Ok(super::CONFIG_CHECK_PARSE_FAILURE);
                }
            },
            "-h" | "--help" => {
                print_usage();
                return Ok(0);
            }
            other => {
                eprintln!("flk preflight: unknown argument {other}");
                print_usage();
                return Ok(super::CONFIG_CHECK_PARSE_FAILURE);
            }
        }
    }

    let checks = collect_checks(&required);
    let worst = checks
        .iter()
        .map(|check| check.severity)
        .max()
        .unwrap_or(Severity::Ok);

    for check in &checks {
        println!(
            "{} {:<12} {}",
            check.severity.glyph(),
            check.name,
            check.detail
        );
    }
    match worst {
        Severity::Ok => println!("\nflock is ready on this host."),
        Severity::Advisory => println!("\nflock works here, but something wants attention."),
        Severity::Blocking => {
            println!("\nflock is NOT functional here — a configured feature is broken.")
        }
    }

    Ok(worst.exit_code())
}

fn print_usage() {
    eprintln!("usage: flk preflight [--require TARGET]...");
    eprintln!();
    eprintln!("  --require TARGET  treat TARGET's integration as intended, so a");
    eprintln!("                    missing hook is blocking rather than advisory.");
    eprintln!("                    Repeatable. The deployment knows its own intent;");
    eprintln!("                    flock cannot infer it.");
    eprintln!();
    eprintln!("exit: 0 ready · 1 advisory · 2 blocking");
}

pub(crate) fn collect_checks(required: &[String]) -> Vec<Check> {
    let mut checks = vec![check_config()];
    checks.extend(check_integrations(required));
    checks.push(check_worktree_dir());
    checks.extend(check_external_tools());
    checks
}

/// Reuses `flk config check` wholesale rather than reimplementing its rules —
/// it already separates "loaded with warnings" from "did not load", which is
/// exactly the advisory/blocking split preflight needs.
fn check_config() -> Check {
    // Runs the real command, diagnostics and all: reuse means reuse. Its
    // output lands above preflight's summary lines, which is where a reader
    // wants the detail anyway.
    let code = super::config_check(&[]).unwrap_or(super::CONFIG_CHECK_PARSE_FAILURE);
    let (severity, detail) = match code {
        0 => (Severity::Ok, "parses clean".to_string()),
        super::CONFIG_CHECK_ADVISORY => (
            Severity::Advisory,
            "loaded with warnings — run `flk config check` for detail".to_string(),
        ),
        _ => (
            Severity::Blocking,
            "did NOT load; the session would run on defaults — run `flk config check`".to_string(),
        ),
    };
    Check {
        name: "config",
        severity,
        detail,
    }
}

/// A target that was wired and has gone stale is unambiguously broken. A target
/// that was never wired might be a deliberate opt-out, so flock will not fail an
/// apply over it on its own judgement — `--require` is how a deployment says it
/// meant to have one.
fn check_integrations(required: &[String]) -> Vec<Check> {
    crate::integration::installed_integration_statuses()
        .into_iter()
        .filter_map(|status| {
            let label = crate::integration::integration_target_label(status.target);
            classify_integration(
                label,
                status.state,
                crate::integration::integration_target_available(status.target),
                required.iter().any(|want| want == label),
            )
        })
        .collect()
}

/// Severity for one target. Split out from host probing so the judgement can be
/// tested against every combination — the `available` arm shipped wrong once
/// (every healthy host warned about the nine agents it does not run, so the
/// advisory exit code meant nothing).
///
/// `None` means "say nothing at all", which is different from "say it is fine":
/// a target whose agent is absent is not this host's business.
fn classify_integration(
    label: &str,
    state: IntegrationStatusKind,
    available: bool,
    required: bool,
) -> Option<Check> {
    match state {
        IntegrationStatusKind::Current => Some(Check {
            name: "integration",
            severity: Severity::Ok,
            detail: format!("{label}: hook current"),
        }),
        // Wired once and now stale: broken beyond argument, whatever the
        // deployment intended.
        IntegrationStatusKind::Outdated => Some(Check {
            name: "integration",
            severity: Severity::Blocking,
            detail: format!("{label}: hook is stale — run `flk integration install {label}`"),
        }),
        // The deployment said it meant to have this one. An absent agent is
        // still a failure: passing silently because the agent vanished would
        // hide exactly the breakage this gate exists to catch.
        IntegrationStatusKind::NotInstalled if required => Some(Check {
            name: "integration",
            severity: Severity::Blocking,
            detail: format!(
                "{label}: required by this deployment but not installed — \
                 run `flk integration install {label}`"
            ),
        }),
        IntegrationStatusKind::NotInstalled if available => Some(Check {
            name: "integration",
            severity: Severity::Advisory,
            detail: format!(
                "{label}: agent present but no hook — features fed by it stay empty. \
                 Run `flk integration install {label}`, or ignore if intentional"
            ),
        }),
        IntegrationStatusKind::NotInstalled => None,
    }
}

fn check_worktree_dir() -> Check {
    let config = crate::config::Config::load().config;
    let dir = crate::worktree::expand_tilde_absolute_path(&config.worktrees.directory);
    // Only the deepest EXISTING ancestor can be probed: the leaf is created on
    // demand, so its absence is normal on a fresh host.
    let mut probe = dir.as_path();
    while !probe.exists() {
        match probe.parent() {
            Some(parent) => probe = parent,
            None => {
                return Check {
                    name: "worktrees",
                    severity: Severity::Blocking,
                    detail: format!("{} has no existing ancestor", dir.display()),
                }
            }
        }
    }
    if is_writable(probe) {
        Check {
            name: "worktrees",
            severity: Severity::Ok,
            detail: format!("{} is writable", probe.display()),
        }
    } else {
        Check {
            name: "worktrees",
            severity: Severity::Blocking,
            detail: format!(
                "{} is not writable — worktree creation will fail",
                probe.display()
            ),
        }
    }
}

fn is_writable(path: &Path) -> bool {
    let probe = path.join(".flk-preflight-probe");
    // create_new: never truncate something already there. A leftover probe from
    // a killed run would then read as "not writable" — wrong, but it fails
    // SAFE (an unnecessary warning, never a false all-clear).
    match std::fs::File::create_new(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// `git` and `ssh` are load-bearing for worktrees and federation respectively;
/// `gh` only matters for PR state, so its absence is advisory.
fn check_external_tools() -> Vec<Check> {
    [
        ("git", Severity::Blocking),
        ("ssh", Severity::Blocking),
        ("gh", Severity::Advisory),
    ]
    .into_iter()
    .map(|(tool, missing_severity)| {
        if crate::integration::command_available(tool) {
            Check {
                name: "tools",
                severity: Severity::Ok,
                detail: format!("{tool} found"),
            }
        } else {
            Check {
                name: "tools",
                severity: missing_severity,
                detail: match tool {
                    "gh" => "gh missing — PR state stays unknown".to_string(),
                    other => format!("{other} missing — required"),
                },
            }
        }
    })
    .collect()
}

fn known_targets() -> Vec<String> {
    crate::integration::installed_integration_statuses()
        .into_iter()
        .map(|status| crate::integration::integration_target_label(status.target).to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_worst_check_decides_the_exit_code() {
        // A single blocking check must outrank any number of clean ones — the
        // whole point is that an apply can gate on this.
        assert_eq!(Severity::Ok.exit_code(), 0);
        assert_eq!(Severity::Advisory.exit_code(), 1);
        assert_eq!(Severity::Blocking.exit_code(), 2);
        assert!(Severity::Blocking > Severity::Advisory);
        assert!(Severity::Advisory > Severity::Ok);
    }

    /// The bug this shipped with: flock knows ten targets, so warning about
    /// every uninstalled one made a healthy host exit 1 — an advisory that is
    /// always on is one people learn to scroll past.
    #[test]
    fn an_agent_that_is_not_on_this_host_is_not_mentioned_at_all() {
        assert!(
            classify_integration("codex", IntegrationStatusKind::NotInstalled, false, false)
                .is_none(),
            "a target whose agent is absent is not this host's business"
        );
        assert!(
            classify_integration("codex", IntegrationStatusKind::NotInstalled, true, false)
                .is_some_and(|c| c.severity == Severity::Advisory),
            "the agent being present is what makes a missing hook worth saying"
        );
    }

    /// Every combination, so the judgement cannot drift silently.
    #[test]
    fn integration_severity_covers_the_whole_matrix() {
        use IntegrationStatusKind::{Current, NotInstalled, Outdated};
        let severity = |state, available, required| {
            classify_integration("claude", state, available, required).map(|c| c.severity)
        };

        // Wired and current is fine regardless of what anyone required.
        assert_eq!(severity(Current, true, false), Some(Severity::Ok));
        assert_eq!(severity(Current, true, true), Some(Severity::Ok));

        // Stale is broken whether or not the deployment declared it.
        assert_eq!(severity(Outdated, true, false), Some(Severity::Blocking));
        assert_eq!(severity(Outdated, false, false), Some(Severity::Blocking));

        // Required outranks availability: a declared target whose agent has
        // vanished must fail, not pass quietly.
        assert_eq!(
            severity(NotInstalled, false, true),
            Some(Severity::Blocking)
        );
        assert_eq!(severity(NotInstalled, true, true), Some(Severity::Blocking));

        // Undeclared: advisory only when the agent is actually here.
        assert_eq!(
            severity(NotInstalled, true, false),
            Some(Severity::Advisory)
        );
        assert_eq!(severity(NotInstalled, false, false), None);
    }

    /// A host that deliberately runs without a hook is correctly configured,
    /// not broken: flock must not fail its apply on its own judgement. Asserted
    /// on the decision directly — an earlier version zipped the optional and
    /// required check lists, which misaligns precisely when `--require`
    /// promotes a silent target into a reported one, so it could pass without
    /// ever exercising the promotion it was named for.
    #[test]
    fn a_missing_hook_is_advisory_until_the_deployment_says_it_is_required() {
        let optional =
            classify_integration("claude", IntegrationStatusKind::NotInstalled, true, false)
                .expect("an installed agent with no hook is worth reporting");
        assert_eq!(optional.severity, Severity::Advisory);

        let required =
            classify_integration("claude", IntegrationStatusKind::NotInstalled, true, true)
                .expect("a required target is always reported");
        assert_eq!(required.severity, Severity::Blocking);
    }

    /// Every reported problem has to name the command that fixes it, or the
    /// reader is left holding a symptom. Asserted against the actual install
    /// command rather than "contains some word", which an earlier version did
    /// loosely enough that a garbage detail string would have passed.
    #[test]
    fn a_reported_integration_problem_names_the_install_command() {
        use IntegrationStatusKind::{NotInstalled, Outdated};
        for (state, available, required) in [
            (Outdated, true, false),
            (Outdated, false, false),
            (NotInstalled, true, false),
            (NotInstalled, false, true),
        ] {
            let check = classify_integration("claude", state, available, required)
                .expect("these combinations are all reportable");
            assert!(
                check.detail.contains("flk integration install claude"),
                "{state:?} (available={available}, required={required}) gave no \
                 runnable fix: {}",
                check.detail
            );
        }
    }

    /// `--require` is how a deployment states intent, so a typo must be loud:
    /// silently matching nothing would demote the blocking check it asked for
    /// back to advisory.
    #[test]
    fn known_targets_are_lowercase_labels_so_require_matching_works() {
        let targets = known_targets();
        assert!(
            targets.iter().any(|t| t == "claude"),
            "claude must be a known target, got {targets:?}"
        );
        assert!(
            targets.iter().all(|t| t == &t.to_ascii_lowercase()),
            "labels must be lowercase — --require lowercases its input before comparing"
        );
    }
}
