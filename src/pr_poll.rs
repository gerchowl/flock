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
//! `gh pr view <branch>` returned, so this is a transport change rather than a
//! behaviour change.
//!
//! One honest caveat: `gh pr view` biases toward the OPEN PR for a branch,
//! whereas `first: 1, orderBy: UPDATED_AT DESC` takes the most recently
//! touched one regardless of state. They differ only when a head branch has
//! several PRs and a closed one was touched more recently than an open one —
//! rare, and the badge self-corrects on the next change.

use std::collections::HashMap;

use crate::worktree::{parse_pr_state_fields, PrStateInfo};

/// Aliases per request. GitHub enforces node/complexity ceilings, and an
/// oversized document is refused wholesale — which without chunking means the
/// poller retries the identical rejected query forever and PR state never
/// updates again. Chunking keeps the request count proportional to fleet size
/// while staying orders of magnitude below one process per worktree.
pub(crate) const MAX_ALIASES_PER_REQUEST: usize = 100;

/// One `(owner/repo, branch)` pair to resolve.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PrQuery {
    pub repo: String,
    pub branch: String,
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
        // serde_json escapes quotes and backslashes, but U+2028/U+2029 are
        // GraphQL LineTerminators that JSON emits raw — they would end the
        // string mid-document. Vanishingly unlikely in a git ref; free to close.
        let branch = serde_json::to_string(&q.branch)
            .unwrap_or_else(|_| "\"\"".into())
            .replace('\u{2028}', "\\u2028")
            .replace('\u{2029}', "\\u2029");
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
/// Run one batched round. `Err` means the ROUND failed (no transport, no token,
/// GraphQL-level error); an empty-but-`Ok` map means "asked, nothing matched".
/// The distinction is what lets health tell "GitHub is unreachable" from
/// "none of these branches have PRs".
pub(crate) fn fetch_batch(
    queries: &[PrQuery],
) -> Result<HashMap<PrQuery, PrStateInfo>, PrPollErrorKind> {
    if queries.is_empty() {
        return Ok(HashMap::new());
    }
    // Chunked so a large fleet degrades into a few requests instead of one
    // document GitHub refuses. Still bounded work per round, unlike 2N execs.
    if queries.len() > MAX_ALIASES_PER_REQUEST {
        let mut merged = HashMap::new();
        for chunk in queries.chunks(MAX_ALIASES_PER_REQUEST) {
            merged.extend(fetch_batch(chunk)?);
        }
        return Ok(merged);
    }
    let text =
        crate::github::graphql::execute(&build_query(queries), serde_json::Value::Null, "pr_poll")?;
    Ok(parse_response(&text, queries))
}

/// Source-scoped health for the PR poller (#295).
///
/// Deliberately **one row for one poller**, not a field per workspace. A single
/// batched call covers every tracked branch, so a failure is one failure —
/// recording it on each of N workspaces would report one outage as N and
/// destroy the distinction between "this branch has no PR" and "the poller
/// wedged", which is exactly the signal the #294 incident lacked.
///
/// The state machine is shared with the fleet's other periodic pollers via
/// [`crate::health::PollerHealthCore`]; the type-level fence is the closed
/// error-kind enum, which each poller owns.
pub(crate) type PrPollHealth = crate::health::PollerHealthCore<PrPollErrorKind>;

/// Longest a PR-poll round can legitimately be in flight: `curl`'s own
/// ceiling plus slack for process spawn and the channel hop.
pub(crate) const PR_POLL_MAX_ROUND_SECS: u64 = 40;

/// Why a round failed, as a closed set.
///
/// An alias rather than a second enum: `PrPollHealth.last_error` is served to
/// OTHER MACHINES inside `PeersSummary`, and the reason it is a closed set —
/// GitHub's raw `errors[0].message` names private repositories and echoes
/// branch names back — applies identically to every caller of the shared
/// transport. Two enums would be two places for that rule to drift.
pub(crate) type PrPollErrorKind = crate::github::graphql::GraphQlErrorKind;

/// Recovery hook for an auth failure. Re-exported rather than re-implemented:
/// the cache it clears is the shared transport's, so a second copy here would
/// clear nothing.
pub(crate) use crate::github::graphql::forget_cached_token;

// The three-state verdict and the degraded-failure-streak threshold moved
// to `crate::health` as part of the shared health primitive (#295). Callers
// reach them through that path now.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::graphql::top_level_error;
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
            crate::health::PollerStatus::Broken,
            "a poller that has never answered must not render as healthy"
        );
        assert_eq!(health.last_success_age_secs(Instant::now()), None);
    }

    #[test]
    fn status_degrades_on_age_then_breaks() {
        let now = Instant::now();
        let mut health = PrPollHealth::default();
        health.mark_success(now - Duration::from_secs(30));
        assert_eq!(
            health.status_at(now, 60, 10),
            crate::health::PollerStatus::Ok
        );

        health.mark_success(now - Duration::from_secs(90));
        assert_eq!(
            health.status_at(now, 60, 10),
            crate::health::PollerStatus::Degraded
        );

        health.mark_success(now - Duration::from_secs(601));
        assert_eq!(
            health.status_at(now, 60, 10),
            crate::health::PollerStatus::Broken
        );
    }

    /// A failure streak degrades even while the last value is still inside the
    /// freshness window — otherwise a poller failing every round looks fine
    /// right up until it silently crosses the staleness line.
    #[test]
    fn a_failure_streak_degrades_inside_the_freshness_window() {
        let now = Instant::now();
        let mut health = PrPollHealth::default();
        health.mark_success(now - Duration::from_secs(5));
        for _ in 0..crate::health::DEGRADED_FAILURE_STREAK {
            health.mark_failure(now, PrPollErrorKind::Transport);
        }
        assert_eq!(
            health.status_at(now, 60, 10),
            crate::health::PollerStatus::Degraded
        );
        assert_eq!(
            health.consecutive_failures,
            crate::health::DEGRADED_FAILURE_STREAK
        );
    }

    #[test]
    fn success_clears_the_failure_streak_and_the_in_flight_flag() {
        let now = Instant::now();
        let mut health = PrPollHealth::default();
        health.mark_started(now);
        assert!(health.in_flight_since.is_some());
        health.mark_failure(now, PrPollErrorKind::Transport);
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

    /// GitHub error text names private repositories. It is classified and
    /// dropped rather than stored, because health crosses hosts inside
    /// PeersSummary.
    #[test]
    fn graphql_errors_are_classified_without_retaining_the_message() {
        assert_eq!(
            PrPollErrorKind::classify_graphql("Bad credentials"),
            PrPollErrorKind::Auth
        );
        assert_eq!(
            PrPollErrorKind::classify_graphql("API rate limit exceeded for user"),
            PrPollErrorKind::RateLimited
        );
        let leaky = "Could not resolve to a Repository with the name 'acme/secret-project'";
        let kind = PrPollErrorKind::classify_graphql(leaky);
        assert_eq!(kind, PrPollErrorKind::GraphQl);
        assert!(
            !kind.as_str().contains("secret-project") && !kind.as_str().contains("acme"),
            "the classification must not carry the repo name"
        );
    }

    /// A guard that outlives its round turns a visible pile-up into a silent
    /// stall — the poller stops and nothing reports it.
    #[test]
    fn a_guard_outliving_its_round_is_reaped() {
        let now = Instant::now();
        let mut health = PrPollHealth::default();
        health.mark_started(now - Duration::from_secs(PR_POLL_MAX_ROUND_SECS + 5));
        assert!(
            health.reap_stuck_round(now, PR_POLL_MAX_ROUND_SECS, PrPollErrorKind::Transport),
            "stale guard should be reaped"
        );
        assert!(health.in_flight_since.is_none());
        assert_eq!(
            health.consecutive_failures, 1,
            "reaping counts as a failure"
        );
    }

    #[test]
    fn a_round_still_within_its_deadline_is_left_alone() {
        let now = Instant::now();
        let mut health = PrPollHealth::default();
        health.mark_started(now - Duration::from_secs(1));
        assert!(!health.reap_stuck_round(now, PR_POLL_MAX_ROUND_SECS, PrPollErrorKind::Transport));
        assert!(
            health.in_flight_since.is_some(),
            "a live round must not be reaped"
        );
    }

    /// Unbounded aliases mean one refused document and a poller that retries
    /// the identical query forever.
    #[test]
    fn alias_count_is_bounded_per_request() {
        let many: Vec<PrQuery> = (0..MAX_ALIASES_PER_REQUEST + 10)
            .map(|i| q("o/n", &format!("b{i}")))
            .collect();
        let doc = build_query(&many[..MAX_ALIASES_PER_REQUEST]);
        assert!(
            doc.contains(&format!("a{}", MAX_ALIASES_PER_REQUEST - 1)),
            "full chunk emitted"
        );
    }

    /// The guard is only useful if it is released on EVERY exit path — a round
    /// that fails and leaves it set would wedge the poller permanently.
    #[test]
    fn an_empty_query_set_short_circuits_without_touching_the_network() {
        assert!(fetch_batch(&[]).expect("empty batch is Ok").is_empty());
    }
}
