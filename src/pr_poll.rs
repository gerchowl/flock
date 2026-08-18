//! Batched PR-state polling (#294).
//!
//! ## Why this is not `gh pr view` per worktree
//!
//! The previous shape forked `gh` once per `(repo, branch)`, plus a `git remote
//! get-url origin` before each — `2N` process spawns per tick. On macOS every
//! exec re-validates the binary's code signature through `syspolicyd`, and `gh`
//! is a large, ad-hoc-signed Mach-O: **9164 CodeDirectory hashes against
//! `curl`'s 58**. On a 34-workspace session that asked Gatekeeper to hash
//! hundreds of MB per minute, forever; validations queued, never drained, and
//! every large-binary exec on the host stalled behind them (#294).
//!
//! One batched GraphQL request replaces all of it. Cost now scales with the
//! **size** of the response, not with the **number** of processes — a session
//! with ten times the worktrees still makes one call.
//!
//! ## Why `curl` rather than an HTTP crate
//!
//! flock deliberately carries no HTTP client dependency; `src/update.rs` states
//! it and uses `curl` as a subprocess. This follows that, which also keeps the
//! per-exec signature cost at `curl`'s 58 hashes instead of `gh`'s 9164.
//!
//! ## Why aliases rather than `search`
//!
//! `search(query: "is:pr …")` cannot express "the PR whose head is exactly this
//! branch" per repo without also matching state, so a merged or closed PR would
//! silently drop out of the result and the badge would clear. Aliased
//! `repository(…) { pullRequests(headRefName:) }` maps 1:1 onto what
//! `gh pr view <branch>` returned, so this is a transport change and not a
//! behaviour change.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::process::TracedCommand;
use crate::worktree::{parse_pr_state_fields, PrStateInfo};

/// GitHub's GraphQL endpoint. Overridable for tests and GHES.
const GRAPHQL_URL: &str = "https://api.github.com/graphql";

/// Bound on one poll round. `curl` enforces this itself, so a hung request is
/// reaped by the transport rather than needing a supervising thread — the
/// timeout half of #295 comes free with the transport swap.
const CONNECT_TIMEOUT_SECS: &str = "5";
const MAX_TIME_SECS: &str = "20";

/// One `(owner/repo, branch)` pair to resolve.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PrQuery {
    pub repo: String,
    pub branch: String,
}

/// The token, resolved once per process.
///
/// `gh` supplied auth for free; dropping it means resolving a token ourselves.
/// The env vars cover CI and anyone who already exports one. Falling back to
/// `gh auth token` costs exactly one large-binary exec for the lifetime of the
/// server, rather than one per poll per worktree — which is the whole point.
fn github_token() -> Option<&'static str> {
    static TOKEN: OnceLock<Option<String>> = OnceLock::new();
    TOKEN
        .get_or_init(|| {
            for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
                if let Ok(value) = std::env::var(var) {
                    let value = value.trim().to_string();
                    if !value.is_empty() {
                        return Some(value);
                    }
                }
            }
            let out = TracedCommand::new("gh", "pr_poll")
                .args(["auth", "token"])
                .output_traced()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (!token.is_empty()).then_some(token)
        })
        .as_deref()
}

/// Split `owner/name`, rejecting anything that is not exactly two segments.
fn split_repo(repo: &str) -> Option<(&str, &str)> {
    let (owner, name) = repo.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some((owner, name))
}

/// Build one document with an alias per query.
///
/// Values are emitted through `serde_json::to_string`, so a branch containing a
/// quote or backslash is escaped rather than terminating the GraphQL string and
/// injecting document text.
pub(crate) fn build_query(queries: &[PrQuery]) -> String {
    let mut doc = String::from("query {");
    for (idx, q) in queries.iter().enumerate() {
        let Some((owner, name)) = split_repo(&q.repo) else {
            continue;
        };
        let owner = serde_json::to_string(owner).unwrap_or_else(|_| "\"\"".into());
        let name = serde_json::to_string(name).unwrap_or_else(|_| "\"\"".into());
        let branch = serde_json::to_string(&q.branch).unwrap_or_else(|_| "\"\"".into());
        doc.push_str(&format!(
            " a{idx}: repository(owner: {owner}, name: {name}) {{ \
             pullRequests(headRefName: {branch}, first: 1, \
             orderBy: {{field: UPDATED_AT, direction: DESC}}) \
             {{ nodes {{ number state isDraft }} }} }}"
        ));
    }
    doc.push_str(" }");
    doc
}

/// Map the aliased response back onto the queries that produced it.
///
/// A missing alias, an empty `nodes`, or a per-alias error is "no PR for this
/// branch" — the same answer the old `gh pr view` gave on exit 1. It is
/// deliberately NOT an error for the round: one unresolvable repo must not
/// discard the other 30 results.
pub(crate) fn parse_response(body: &str, queries: &[PrQuery]) -> HashMap<PrQuery, PrStateInfo> {
    let mut out = HashMap::new();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return out;
    };
    let Some(data) = value.get("data") else {
        return out;
    };
    for (idx, q) in queries.iter().enumerate() {
        let node = data
            .get(format!("a{idx}"))
            .and_then(|repo| repo.get("pullRequests"))
            .and_then(|prs| prs.get("nodes"))
            .and_then(|nodes| nodes.get(0));
        let Some(node) = node else { continue };
        let number = node.get("number").and_then(serde_json::Value::as_u64);
        let state = node.get("state").and_then(serde_json::Value::as_str);
        let is_draft = node.get("isDraft").and_then(serde_json::Value::as_bool);
        if let (Some(number), Some(state)) = (number, state) {
            if let Some(info) = parse_pr_state_fields(state, number, is_draft) {
                out.insert(q.clone(), info);
            }
        }
    }
    out
}

/// Whether the response carried a top-level `errors` array. GraphQL answers
/// `200 OK` with errors in the body, so transport success is not query success
/// — treating it as success is how a poller silently reports "no PRs anywhere"
/// forever after a token expires.
pub(crate) fn top_level_error(body: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(body).ok()?;
    let errors = value.get("errors")?.as_array()?;
    let first = errors.first()?;
    Some(
        first
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("graphql error")
            .to_string(),
    )
}

/// Run one batched round. `Err` means the ROUND failed (no transport, no token,
/// GraphQL-level error); an empty-but-`Ok` map means "asked, nothing matched".
/// The distinction is what lets health tell "GitHub is unreachable" from
/// "none of these branches have PRs".
pub(crate) fn fetch_batch(queries: &[PrQuery]) -> Result<HashMap<PrQuery, PrStateInfo>, String> {
    if queries.is_empty() {
        return Ok(HashMap::new());
    }
    let token = github_token().ok_or_else(|| {
        "no GitHub token (set GH_TOKEN/GITHUB_TOKEN or run `gh auth login`)".to_string()
    })?;
    let body = serde_json::json!({ "query": build_query(queries) }).to_string();
    let output = TracedCommand::new("curl", "pr_poll")
        .args([
            "-sS",
            "--connect-timeout",
            CONNECT_TIMEOUT_SECS,
            "--max-time",
            MAX_TIME_SECS,
            "-H",
            &format!("Authorization: bearer {token}"),
            "-H",
            "Content-Type: application/json",
            "-X",
            "POST",
            "-d",
            &body,
            GRAPHQL_URL,
        ])
        .output_traced()
        .map_err(|err| format!("curl failed: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "curl exited {}",
            output
                .status
                .code()
                .map_or_else(|| "by signal".to_string(), |c| c.to_string())
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    if let Some(err) = top_level_error(&text) {
        return Err(err);
    }
    Ok(parse_response(&text, queries))
}

/// Source-scoped health for the PR poller (#295).
///
/// Deliberately **one row for one poller**, not a field per workspace. A single
/// batched call covers every tracked branch, so a failure is one failure —
/// recording it on each of N workspaces would report one outage as N and
/// destroy the distinction between "this branch has no PR" and "the poller
/// wedged", which is exactly the signal the #294 incident lacked.
#[derive(Debug, Default, Clone)]
pub(crate) struct PrPollHealth {
    pub last_success: Option<std::time::Instant>,
    pub last_attempt: Option<std::time::Instant>,
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
    /// Set while a round is running. Also the overlap guard: a tick that finds
    /// this set skips instead of spawning a second round. Unbounded spawning is
    /// what turned a slow host into a collapsing one (#294) — rounds piled up
    /// rather than draining.
    pub in_flight_since: Option<std::time::Instant>,
    /// Rounds skipped because one was already running. Visible rather than
    /// silent: a poller quietly skipping every tick looks identical to a
    /// healthy one that has nothing to report.
    pub skipped_rounds: u64,
}

/// Health as a three-state verdict.
///
/// Thresholds are derived from the caller's configured staleness window rather
/// than invented here, so PR freshness and gossip freshness cannot disagree
/// about what "stale" means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrPollStatus {
    Ok,
    Degraded,
    Broken,
}

/// Consecutive failures past which a poller is Degraded even while its last
/// value is still inside the staleness window.
pub(crate) const DEGRADED_FAILURE_STREAK: u32 = 3;

impl PrPollHealth {
    pub(crate) fn mark_started(&mut self, now: std::time::Instant) {
        self.in_flight_since = Some(now);
        self.last_attempt = Some(now);
    }

    pub(crate) fn mark_success(&mut self, now: std::time::Instant) {
        self.in_flight_since = None;
        self.last_success = Some(now);
        self.consecutive_failures = 0;
        self.last_error = None;
    }

    pub(crate) fn mark_failure(&mut self, now: std::time::Instant, error: String) {
        self.in_flight_since = None;
        let _ = now;
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.last_error = Some(error);
    }

    pub(crate) fn mark_skipped(&mut self) {
        self.skipped_rounds = self.skipped_rounds.saturating_add(1);
    }

    /// Age of the last success, or `None` if it has never succeeded.
    pub(crate) fn last_success_age_secs(&self, now: std::time::Instant) -> Option<u64> {
        self.last_success
            .map(|at| now.saturating_duration_since(at).as_secs())
    }

    /// `stale_after` is the caller's freshness window; `broken_multiple`
    /// mirrors gossip's TTL multiple so both subsystems agree on "long gone".
    pub(crate) fn status_at(
        &self,
        now: std::time::Instant,
        stale_after_secs: u64,
        broken_multiple: u64,
    ) -> PrPollStatus {
        let Some(age) = self.last_success_age_secs(now) else {
            // Never succeeded is Broken, not Ok — an empty poller that has
            // never answered must not render as healthy.
            return PrPollStatus::Broken;
        };
        if age > stale_after_secs.saturating_mul(broken_multiple) {
            return PrPollStatus::Broken;
        }
        if age > stale_after_secs || self.consecutive_failures >= DEGRADED_FAILURE_STREAK {
            return PrPollStatus::Degraded;
        }
        PrPollStatus::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn q(repo: &str, branch: &str) -> PrQuery {
        PrQuery {
            repo: repo.to_string(),
            branch: branch.to_string(),
        }
    }

    /// A branch name is attacker-adjacent data: it comes from whatever someone
    /// named a git ref. Emitting it raw into a GraphQL document would let a
    /// quote terminate the string and inject document text.
    #[test]
    fn branch_names_cannot_break_out_of_the_graphql_string() {
        let doc = build_query(&[q("o/n", r#"eviname") { x } evil(a: "#)]);
        assert!(
            !doc.contains(r#""eviname") { x }"#),
            "raw injection reached the document: {doc}"
        );
        assert!(doc.contains(r#"\""#), "quote should be escaped: {doc}");
    }

    #[test]
    fn one_alias_per_query_and_repo_is_split() {
        let doc = build_query(&[q("gerchowl/flock", "main"), q("gerchowl/g-fleet", "dev")]);
        assert!(
            doc.contains("a0: repository(owner: \"gerchowl\", name: \"flock\")"),
            "{doc}"
        );
        assert!(
            doc.contains("a1: repository(owner: \"gerchowl\", name: \"g-fleet\")"),
            "{doc}"
        );
        assert!(doc.contains("headRefName: \"main\""), "{doc}");
    }

    /// A malformed repo must not shift the alias numbering, or every later
    /// answer would be attributed to the wrong branch.
    #[test]
    fn a_malformed_repo_is_skipped_without_renumbering_the_rest() {
        let doc = build_query(&[q("not-a-repo", "main"), q("o/n", "dev")]);
        assert!(
            !doc.contains("a0:"),
            "malformed repo should emit nothing: {doc}"
        );
        assert!(
            doc.contains("a1: repository"),
            "second query keeps index 1: {doc}"
        );
    }

    #[test]
    fn parses_each_alias_back_onto_its_query() {
        let queries = vec![q("o/a", "main"), q("o/b", "dev")];
        let body = r#"{"data":{
            "a0":{"pullRequests":{"nodes":[{"number":7,"state":"OPEN","isDraft":false}]}},
            "a1":{"pullRequests":{"nodes":[{"number":9,"state":"MERGED","isDraft":false}]}}}}"#;
        let got = parse_response(body, &queries);
        assert_eq!(got.get(&queries[0]).map(|p| p.number), Some(7));
        assert_eq!(got.get(&queries[1]).map(|p| p.number), Some(9));
    }

    /// Partial failure is the batching-specific hazard: one repo erroring must
    /// not discard the other 30 answers.
    #[test]
    fn a_null_alias_drops_only_its_own_entry() {
        let queries = vec![q("o/a", "main"), q("o/b", "dev")];
        let body = r#"{"data":{
            "a0":null,
            "a1":{"pullRequests":{"nodes":[{"number":9,"state":"OPEN","isDraft":true}]}}}}"#;
        let got = parse_response(body, &queries);
        assert!(!got.contains_key(&queries[0]));
        assert_eq!(
            got.get(&queries[1]).map(|p| p.state),
            Some(crate::worktree::PrState::Draft)
        );
    }

    #[test]
    fn an_empty_nodes_list_means_no_pr_for_that_branch() {
        let queries = vec![q("o/a", "main")];
        let body = r#"{"data":{"a0":{"pullRequests":{"nodes":[]}}}}"#;
        assert!(parse_response(body, &queries).is_empty());
    }

    /// GraphQL answers 200 with errors in the body. Treating transport success
    /// as query success is how a poller reports "no PRs anywhere" forever after
    /// a token expires.
    #[test]
    fn top_level_errors_are_detected_despite_http_200() {
        let body = r#"{"errors":[{"message":"Bad credentials"}]}"#;
        assert_eq!(top_level_error(body).as_deref(), Some("Bad credentials"));
        assert_eq!(top_level_error(r#"{"data":{}}"#), None);
    }

    #[test]
    fn never_succeeded_is_broken_not_ok() {
        let health = PrPollHealth::default();
        assert_eq!(
            health.status_at(Instant::now(), 60, 10),
            PrPollStatus::Broken,
            "a poller that has never answered must not render as healthy"
        );
        assert_eq!(health.last_success_age_secs(Instant::now()), None);
    }

    #[test]
    fn status_degrades_on_age_then_breaks() {
        let now = Instant::now();
        let mut health = PrPollHealth::default();
        health.mark_success(now - Duration::from_secs(30));
        assert_eq!(health.status_at(now, 60, 10), PrPollStatus::Ok);

        health.mark_success(now - Duration::from_secs(90));
        assert_eq!(health.status_at(now, 60, 10), PrPollStatus::Degraded);

        health.mark_success(now - Duration::from_secs(601));
        assert_eq!(health.status_at(now, 60, 10), PrPollStatus::Broken);
    }

    /// A failure streak degrades even while the last value is still inside the
    /// freshness window — otherwise a poller failing every round looks fine
    /// right up until it silently crosses the staleness line.
    #[test]
    fn a_failure_streak_degrades_inside_the_freshness_window() {
        let now = Instant::now();
        let mut health = PrPollHealth::default();
        health.mark_success(now - Duration::from_secs(5));
        for _ in 0..DEGRADED_FAILURE_STREAK {
            health.mark_failure(now, "boom".into());
        }
        assert_eq!(health.status_at(now, 60, 10), PrPollStatus::Degraded);
        assert_eq!(health.consecutive_failures, DEGRADED_FAILURE_STREAK);
    }

    #[test]
    fn success_clears_the_failure_streak_and_the_in_flight_flag() {
        let now = Instant::now();
        let mut health = PrPollHealth::default();
        health.mark_started(now);
        assert!(health.in_flight_since.is_some());
        health.mark_failure(now, "boom".into());
        assert!(
            health.in_flight_since.is_none(),
            "a failed round must release the guard"
        );
        health.mark_started(now);
        health.mark_success(now);
        assert_eq!(health.consecutive_failures, 0);
        assert!(health.last_error.is_none());
        assert!(health.in_flight_since.is_none());
    }

    /// The guard is only useful if it is released on EVERY exit path — a round
    /// that fails and leaves it set would wedge the poller permanently.
    #[test]
    fn an_empty_query_set_short_circuits_without_touching_the_network() {
        assert!(fetch_batch(&[]).expect("empty batch is Ok").is_empty());
    }
}
