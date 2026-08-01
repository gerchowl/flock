//! Built-in owner-guarded issue triggers (#175 phase 4, C4).
//!
//! Polls GitHub issues on the configured repos via the `gh` CLI. Parses a
//! `flk-trigger` fenced block from ISSUE BODIES only (never comments —
//! kills the whole class of pull_request_target-style privilege
//! escalation) and, when the body is authored by the configured owner,
//! dispatches the trigger's action.
//!
//! ## Trigger block shape
//!
//! ````
//! ```flk-trigger
//! on = "issue_updated"
//! run_id = "optional-stable-id"
//!
//! [action.notify]
//! title = "flock reload"
//! sound = "request"
//! ```
//! ````
//!
//! The block is TOML rather than YAML (design accepted YAML; using TOML
//! avoids a new heavy dependency and matches every other flock config
//! surface — documented as a phase-4 deviation).
//!
//! ## Guarantees
//!
//! - **Owner-only:** author != config owner → `TriggerIgnored`, nothing
//!   dispatched.
//! - **One fence per body:** two or more `flk-trigger` fences → error,
//!   post a comment back on the issue, emit `TriggerErrored`.
//! - **Dedupe:** `run_id` when set, else hash(repo, issue, body). Persisted
//!   across restarts by seeding the dedupe set from the durable event log
//!   (mirrors the mailbox pattern from src/app/mailboxes.rs).
//! - **Never a shell string:** the action is a closed `ActionSpec` — no
//!   `Command` / `Script` variant escapes to a shell.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::config::{ActionSpec, IssueGuardConfig};

/// Name of the built-in check — surfaces in `flk checks list` and every
/// durable event emit.
pub(crate) const ISSUE_GUARD_CHECK_NAME: &str = "issue_guard";

/// One issue as reported by `gh issue view --json body,author,number,updatedAt`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GhIssue {
    pub number: u64,
    pub body: String,
    pub author: GhAuthor,
    #[serde(rename = "updatedAt")]
    #[allow(dead_code, reason = "surface reserved for future dedupe-by-time")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GhAuthor {
    pub login: String,
}

/// The parsed body-side trigger block.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct TriggerBlock {
    /// `on: issue_updated` is the only accepted event today. Any other
    /// value is a hard error.
    pub on: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub action: ActionSpec,
}

/// Outcome of running the guard once against one issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IssueGuardOutcome {
    /// The body's author isn't the owner — audit the ignore, don't fire.
    Ignored { reason: String },
    /// The body is fine, and the trigger will fire.
    Fire {
        dedupe_key: String,
        block: Box<TriggerBlock>,
    },
    /// The body already fired for this dedupe_key — silent pass.
    Deduped { dedupe_key: String },
    /// The body carried a broken block — error to audit AND to comment
    /// back on the issue.
    Errored { reason: String },
    /// The body carried no `flk-trigger` fence at all — no-op.
    NoTrigger,
}

/// State the fold keeps across ticks.
#[derive(Debug, Default, Clone)]
pub(crate) struct IssueGuardFold {
    /// Dedupe keys of triggers we already fired. Bounded — an operator
    /// with 10k triggers over the process lifetime is still fine.
    fired: HashSet<String>,
    /// Next allowed poll deadline; bounds the cadence.
    next_poll: Option<Instant>,
}

impl IssueGuardFold {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Seed the dedupe set from the durable event log at boot — the same
    /// pattern the mailbox registry uses (`src/app/mailboxes.rs`).
    pub(crate) fn seed_from_fired_keys<I: IntoIterator<Item = String>>(&mut self, keys: I) {
        self.fired.extend(keys);
    }

    /// Whether the poll cadence has elapsed.
    pub(crate) fn poll_due(&self, now: Instant) -> bool {
        self.next_poll.is_none_or(|deadline| now >= deadline)
    }

    /// Next-poll deadline, if armed. Consumed by the App to fold into
    /// `next_loop_deadline`.
    pub(crate) fn next_poll_deadline(&self) -> Option<Instant> {
        self.next_poll
    }

    /// Arm the next poll after `now` based on config cadence.
    pub(crate) fn arm_next_poll(&mut self, now: Instant, config: &IssueGuardConfig) {
        let poll_secs = config.poll_secs.max(1);
        self.next_poll = Some(now + Duration::from_secs(poll_secs));
    }

    /// An error verdict, at most once per (repo, issue, body). Without this
    /// a single malformed owner-authored trigger would post a `gh issue
    /// comment` on every poll — a self-inflicted comment flood until the
    /// body is edited.
    fn error_once(&mut self, repo: &str, issue: &GhIssue, reason: String) -> IssueGuardOutcome {
        let key = format!("err:{}", body_dedupe_key(repo, issue.number, &issue.body));
        if self.fired.contains(&key) {
            return IssueGuardOutcome::Deduped { dedupe_key: key };
        }
        self.fired.insert(key);
        IssueGuardOutcome::Errored { reason }
    }

    /// Fold one issue against the guard: parse the body, apply the owner
    /// gate and dedupe, return the outcome. Mutates the dedupe set when
    /// returning `Fire` or a first-time `Errored`.
    pub(crate) fn evaluate(
        &mut self,
        repo: &str,
        issue: &GhIssue,
        config: &IssueGuardConfig,
    ) -> IssueGuardOutcome {
        let extracted = extract_trigger_blocks(&issue.body);
        if extracted.is_empty() {
            return IssueGuardOutcome::NoTrigger;
        }
        // Owner gate BEFORE every other verdict (P7, §8.7). The repo is
        // public: a non-owner issue must produce NOTHING but an audit line.
        // Erroring first would let any GitHub user drive flock into posting
        // `gh issue comment` on their own issue — comment-spam amplification
        // with flock's identity.
        if !issue.author.login.eq_ignore_ascii_case(&config.owner) {
            return IssueGuardOutcome::Ignored {
                reason: format!(
                    "author {} is not the configured owner {}",
                    issue.author.login, config.owner
                ),
            };
        }
        if extracted.len() > 1 {
            return self.error_once(
                repo,
                issue,
                format!(
                    "found {n} `flk-trigger` fenced blocks; issue bodies may declare at most one",
                    n = extracted.len()
                ),
            );
        }
        let raw = &extracted[0];
        let block = match toml::from_str::<TriggerBlock>(raw) {
            Ok(block) => block,
            Err(err) => {
                return self.error_once(
                    repo,
                    issue,
                    format!("could not parse flk-trigger block: {err}"),
                );
            }
        };
        if block.on != "issue_updated" {
            return self.error_once(
                repo,
                issue,
                format!(
                    "flk-trigger `on` must be `issue_updated`; got {other:?}",
                    other = block.on
                ),
            );
        }
        // Actions are limited to Notify in phase 4 — the SpawnAgent variant
        // is explicitly deferred to S1.
        if !matches!(block.action, ActionSpec::Notify { .. }) {
            return self.error_once(
                repo,
                issue,
                "flk-trigger action must be `notify` in phase 4; \
                 spawn actions are deferred to S1"
                    .to_string(),
            );
        }

        let dedupe_key = block
            .run_id
            .clone()
            .unwrap_or_else(|| body_dedupe_key(repo, issue.number, &issue.body));
        if self.fired.contains(&dedupe_key) {
            return IssueGuardOutcome::Deduped { dedupe_key };
        }
        self.fired.insert(dedupe_key.clone());
        IssueGuardOutcome::Fire {
            dedupe_key,
            block: Box::new(block),
        }
    }
}

/// Stable, restart-tolerant dedupe key from the body content.
pub(crate) fn body_dedupe_key(repo: &str, issue: u64, body: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    repo.hash(&mut hasher);
    issue.hash(&mut hasher);
    body.hash(&mut hasher);
    format!("{repo}#{issue}:{:016x}", hasher.finish())
}

/// Extract every ` ```flk-trigger` ... ` ``` ` fenced block from an issue
/// body. Preserves order so the caller can enforce "at most one".
pub(crate) fn extract_trigger_blocks(body: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut lines = body.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if trimmed != "```flk-trigger" && !trimmed.starts_with("```flk-trigger ") {
            continue;
        }
        let mut collected = String::new();
        for follow in lines.by_ref() {
            if follow.trim_start().starts_with("```") {
                blocks.push(collected);
                collected = String::new();
                break;
            }
            collected.push_str(follow);
            collected.push('\n');
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::NotificationShowSound;

    fn config(owner: &str) -> IssueGuardConfig {
        IssueGuardConfig {
            enable: true,
            owner: owner.into(),
            repos: vec!["gerchowl/flock".into()],
            gh_bin: "gh".into(),
            poll_secs: 60,
            max_issues: 50,
        }
    }

    fn author(login: &str) -> GhAuthor {
        GhAuthor {
            login: login.into(),
        }
    }

    fn issue(body: &str, author: GhAuthor) -> GhIssue {
        GhIssue {
            number: 42,
            body: body.into(),
            author,
            updated_at: None,
        }
    }

    #[test]
    fn non_owner_never_reaches_an_error_verdict() {
        // §8.7 / P7 (the repo is PUBLIC): a stranger's issue must produce
        // nothing but an audit line. If a malformed-trigger verdict could
        // outrank the owner gate, any GitHub user could drive flock into
        // posting `gh issue comment` on their own issue — comment-spam
        // amplification signed with the operator's identity.
        let mut guard = IssueGuardFold::default();
        let config = config("gerchowl");
        let hostile_bodies = [
            // two fences (the old first-checked branch)
            "```flk-trigger\na = 1\n```\n```flk-trigger\nb = 2\n```\n",
            // unparseable TOML
            "```flk-trigger\n!!! not toml\n```\n",
            // wrong `on`
            "```flk-trigger\non = \"push\"\n```\n",
            // disallowed action
            "```flk-trigger\non = \"issue_updated\"\n[action.hibernate]\npane = \"w1:p1\"\n```\n",
        ];
        for body in hostile_bodies {
            let outcome = guard.evaluate(
                "gerchowl/flock",
                &issue(body, author("randostranger")),
                &config,
            );
            assert!(
                matches!(outcome, IssueGuardOutcome::Ignored { .. }),
                "non-owner body must be ignored, got {outcome:?}"
            );
        }
    }

    #[test]
    fn owner_login_compares_case_insensitively() {
        // GitHub logins are case-insensitive; a config typo'd as "Gerchowl"
        // must not silently ignore every legitimate trigger.
        let mut guard = IssueGuardFold::default();
        let config = config("Gerchowl");
        let body = "```flk-trigger\non = \"issue_updated\"\n[action.notify]\ntitle = \"hi\"\n```\n";
        let outcome = guard.evaluate("gerchowl/flock", &issue(body, author("gerchowl")), &config);
        assert!(
            matches!(outcome, IssueGuardOutcome::Fire { .. }),
            "case-different owner must still fire, got {outcome:?}"
        );
    }

    #[test]
    fn owner_error_verdicts_are_deduped_per_body() {
        // A malformed owner-authored trigger posts ONE comment, not one per
        // poll — otherwise a typo floods the issue every cadence tick.
        let mut guard = IssueGuardFold::default();
        let config = config("gerchowl");
        let body = "```flk-trigger\n!!! not toml\n```\n";
        let first = guard.evaluate("gerchowl/flock", &issue(body, author("gerchowl")), &config);
        assert!(matches!(first, IssueGuardOutcome::Errored { .. }));
        let second = guard.evaluate("gerchowl/flock", &issue(body, author("gerchowl")), &config);
        assert!(
            matches!(second, IssueGuardOutcome::Deduped { .. }),
            "repeat poll of the same bad body must dedupe, got {second:?}"
        );
        // Editing the body is a new error episode — the owner sees the new
        // failure once.
        let edited = "```flk-trigger\nstill !!! broken\n```\n";
        let third = guard.evaluate(
            "gerchowl/flock",
            &issue(edited, author("gerchowl")),
            &config,
        );
        assert!(matches!(third, IssueGuardOutcome::Errored { .. }));
    }

    #[test]
    fn extracts_zero_one_or_many_blocks() {
        assert!(extract_trigger_blocks("nothing here").is_empty());
        let one = "```flk-trigger\non = \"issue_updated\"\n```\n";
        assert_eq!(extract_trigger_blocks(one).len(), 1);
        let two = "```flk-trigger\na = 1\n```\ntext\n```flk-trigger\nb = 2\n```\n";
        assert_eq!(extract_trigger_blocks(two).len(), 2);
    }

    #[test]
    fn owner_gated_ignore_for_non_owner_posts() {
        let mut fold = IssueGuardFold::new();
        let body = "```flk-trigger\non = \"issue_updated\"\n[action.notify]\ntitle = \"go\"\n```\n";
        let out = fold.evaluate(
            "gerchowl/flock",
            &issue(body, author("intruder")),
            &config("gerchowl"),
        );
        match out {
            IssueGuardOutcome::Ignored { reason } => assert!(reason.contains("intruder")),
            other => panic!("expected Ignored, got {other:?}"),
        }
    }

    #[test]
    fn owner_triggers_fire_once_then_dedupe() {
        let mut fold = IssueGuardFold::new();
        let body = "```flk-trigger\non = \"issue_updated\"\nrun_id = \"r1\"\n[action.notify]\ntitle = \"go\"\n```\n";
        let first = fold.evaluate(
            "gerchowl/flock",
            &issue(body, author("gerchowl")),
            &config("gerchowl"),
        );
        assert!(matches!(first, IssueGuardOutcome::Fire { .. }));
        let second = fold.evaluate(
            "gerchowl/flock",
            &issue(body, author("gerchowl")),
            &config("gerchowl"),
        );
        assert!(matches!(second, IssueGuardOutcome::Deduped { .. }));
    }

    #[test]
    fn multiple_fences_error() {
        let mut fold = IssueGuardFold::new();
        let body =
            "```flk-trigger\non = \"issue_updated\"\n```\ntext\n```flk-trigger\nother\n```\n";
        let out = fold.evaluate(
            "gerchowl/flock",
            &issue(body, author("gerchowl")),
            &config("gerchowl"),
        );
        assert!(matches!(out, IssueGuardOutcome::Errored { .. }));
    }

    #[test]
    fn bad_yaml_errors() {
        let mut fold = IssueGuardFold::new();
        let body = "```flk-trigger\n:not toml at all\n```\n";
        let out = fold.evaluate(
            "gerchowl/flock",
            &issue(body, author("gerchowl")),
            &config("gerchowl"),
        );
        assert!(matches!(out, IssueGuardOutcome::Errored { .. }));
    }

    #[test]
    fn non_notify_action_errors() {
        let mut fold = IssueGuardFold::new();
        let body =
            "```flk-trigger\non = \"issue_updated\"\n[action.event]\nlabel = \"custom\"\n```\n";
        let out = fold.evaluate(
            "gerchowl/flock",
            &issue(body, author("gerchowl")),
            &config("gerchowl"),
        );
        assert!(matches!(out, IssueGuardOutcome::Errored { .. }));
    }

    #[test]
    fn no_trigger_body_is_noop() {
        let mut fold = IssueGuardFold::new();
        let out = fold.evaluate(
            "gerchowl/flock",
            &issue("no fences here", author("gerchowl")),
            &config("gerchowl"),
        );
        assert!(matches!(out, IssueGuardOutcome::NoTrigger));
    }

    #[test]
    fn hash_dedupe_covers_bodies_without_run_id() {
        let mut fold = IssueGuardFold::new();
        let body = "```flk-trigger\non = \"issue_updated\"\n[action.notify]\ntitle = \"t\"\n```\n";
        let first = fold.evaluate(
            "gerchowl/flock",
            &issue(body, author("gerchowl")),
            &config("gerchowl"),
        );
        let IssueGuardOutcome::Fire { dedupe_key, .. } = first else {
            panic!("expected Fire");
        };
        let second = fold.evaluate(
            "gerchowl/flock",
            &issue(body, author("gerchowl")),
            &config("gerchowl"),
        );
        let IssueGuardOutcome::Deduped {
            dedupe_key: second_key,
        } = second
        else {
            panic!("expected Deduped");
        };
        assert_eq!(dedupe_key, second_key);
    }

    #[test]
    fn seed_from_fired_keys_preserves_dedupe_across_restart() {
        let body = "```flk-trigger\non = \"issue_updated\"\nrun_id = \"r7\"\n[action.notify]\ntitle = \"t\"\n```\n";
        let mut boot = IssueGuardFold::new();
        boot.seed_from_fired_keys(["r7".to_string()]);
        let out = boot.evaluate(
            "gerchowl/flock",
            &issue(body, author("gerchowl")),
            &config("gerchowl"),
        );
        assert!(
            matches!(out, IssueGuardOutcome::Deduped { .. }),
            "seed must suppress a re-fire on the same run_id"
        );
    }

    #[test]
    fn parsed_notify_action_has_configured_shape() {
        let mut fold = IssueGuardFold::new();
        let body = "```flk-trigger\non = \"issue_updated\"\n[action.notify]\ntitle = \"reload\"\nsound = \"request\"\n```\n";
        let out = fold.evaluate(
            "gerchowl/flock",
            &issue(body, author("gerchowl")),
            &config("gerchowl"),
        );
        match out {
            IssueGuardOutcome::Fire { block, .. } => match block.action {
                ActionSpec::Notify { title, sound } => {
                    assert_eq!(title, "reload");
                    assert_eq!(sound, NotificationShowSound::Request);
                }
                other => panic!("expected Notify action, got {other:?}"),
            },
            other => panic!("expected Fire, got {other:?}"),
        }
    }
}
