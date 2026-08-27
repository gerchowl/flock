//! The cross-repo destination directory (#371).
//!
//! ## Why enumerate at all
//!
//! The feature is filing into the repo you are NOT in. A picker seeded only
//! from local checkouts solves the case that was never broken — you queue that
//! to the agent already sitting in the pane. So the directory has to reach
//! repositories this machine has never cloned.
//!
//! ## Why one query and not `viewer.organizations`
//!
//! `affiliations` already spans OWNER, COLLABORATOR and ORGANIZATION_MEMBER,
//! which covers team-granted access too. Walking `viewer.organizations {
//! repositories }` separately returns the same repositories a second time and
//! doubles the point cost for nothing.
//!
//! Measured against a real token: 77 repositories, `rateLimit.cost` **1**, one
//! page. The ceiling that matters is pages, not points — so this pages to
//! exhaustion rather than truncating at the first 100 and quietly presenting a
//! partial directory as complete.

use std::time::{Duration, Instant};

use super::graphql::{execute, GraphQlErrorKind};

/// Stop paging after this many rounds.
///
/// A guard against a `hasNextPage`/`endCursor` pair that never advances —
/// without it a malformed response spins forever inside a UI action. At
/// a page size of 100 this still admits 2 000 repositories, well past any real
/// account.
const MAX_PAGES: usize = 20;

/// How long a fetched directory stays usable.
///
/// Repository sets churn on days-to-weeks, so this trades staleness nobody
/// notices for not re-querying on every open. A repo created since the last
/// fetch is reachable immediately by typing its `owner/name` — the directory is
/// a shortcut, never the only way through, which is what makes a long TTL safe.
pub(crate) const DIRECTORY_TTL: Duration = Duration::from_secs(15 * 60);

/// One filable destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoEntry {
    pub name_with_owner: String,
    /// Descending recency of the last push, as GitHub ordered it. Kept as the
    /// server's ordinal rather than a timestamp: the only use is a stable
    /// tiebreak, and a timestamp would invite arithmetic on a clock this
    /// machine does not own.
    pub recency_rank: usize,
}

/// Where a destination came from, which is also its sort order.
///
/// Ranking is not decoration. An operator with 77 repositories reachable and
/// three they actually work in should not scroll; but the ones they have never
/// checked out must still be *present*, because those are the whole point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Provenance {
    /// A worktree open in this session.
    LocalCheckout,
    /// Named by a peer summary — someone in the fleet is working on it.
    SeenInFleet,
    /// Reachable by the token, unseen locally.
    Reachable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RankedRepo {
    pub name_with_owner: String,
    pub provenance: Provenance,
}

/// The directory plus the moment it was fetched.
#[derive(Debug, Clone)]
pub(crate) struct RepoDirectory {
    entries: Vec<RepoEntry>,
    fetched_at: Instant,
}

impl RepoDirectory {
    pub(crate) fn new(entries: Vec<RepoEntry>, fetched_at: Instant) -> Self {
        Self {
            entries,
            fetched_at,
        }
    }

    pub(crate) fn entries(&self) -> &[RepoEntry] {
        &self.entries
    }

    /// Whether `now` is past the TTL. Uses `checked_duration_since` so a `now`
    /// earlier than `fetched_at` reads as fresh rather than panicking — the
    /// monotonic clock makes that unreachable in production, but a test that
    /// constructs both is not a reason to abort the process.
    pub(crate) fn is_stale(&self, now: Instant) -> bool {
        now.checked_duration_since(self.fetched_at)
            .is_some_and(|age| age >= DIRECTORY_TTL)
    }
}

/// The paged enumeration document.
///
/// `viewerCanCreateIssues` and `hasIssuesEnabled` are requested so the filter
/// runs on facts rather than on a guess; `isArchived` because an archived repo
/// accepts no issues and offering it is a dead end the operator only discovers
/// after composing.
pub(crate) fn enumerate_query() -> &'static str {
    r#"
query($cursor: String) {
  viewer {
    repositories(first: 100, after: $cursor,
      affiliations: [OWNER, COLLABORATOR, ORGANIZATION_MEMBER],
      orderBy: {field: PUSHED_AT, direction: DESC}) {
      pageInfo { hasNextPage endCursor }
      nodes { nameWithOwner viewerCanCreateIssues hasIssuesEnabled isArchived }
    }
  }
}"#
}

/// One page of parsed entries plus the cursor to continue from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Page {
    pub entries: Vec<RepoEntry>,
    pub next_cursor: Option<String>,
}

/// Parse one enumeration page, dropping anything unfilable.
///
/// `start_rank` continues the recency ordinal across pages so page two does not
/// restart at zero and outrank page one.
pub(crate) fn parse_page(body: &str, start_rank: usize) -> Page {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Page {
            entries: Vec::new(),
            next_cursor: None,
        };
    };
    let repos = value
        .pointer("/data/viewer/repositories")
        .unwrap_or(&serde_json::Value::Null);

    let next_cursor = repos
        .pointer("/pageInfo/hasNextPage")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        .then(|| {
            repos
                .pointer("/pageInfo/endCursor")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .flatten();

    let mut entries = Vec::new();
    if let Some(nodes) = repos.get("nodes").and_then(serde_json::Value::as_array) {
        for node in nodes {
            let flag = |key: &str| {
                node.get(key)
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
            };
            // Absent flags read as false, so a response shape that changes
            // under us hides a repo rather than offering one that will reject
            // the mutation after the operator has written the body.
            if !flag("viewerCanCreateIssues") || !flag("hasIssuesEnabled") || flag("isArchived") {
                continue;
            }
            let Some(name) = node
                .get("nameWithOwner")
                .and_then(serde_json::Value::as_str)
                .filter(|name| name.contains('/'))
            else {
                continue;
            };
            entries.push(RepoEntry {
                name_with_owner: name.to_string(),
                recency_rank: start_rank + entries.len(),
            });
        }
    }
    Page {
        entries,
        next_cursor,
    }
}

/// Enumerate every filable repository the token can reach.
pub(crate) fn fetch_directory() -> Result<Vec<RepoEntry>, GraphQlErrorKind> {
    let mut all: Vec<RepoEntry> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let variables = serde_json::json!({ "cursor": cursor });
        let body = execute(enumerate_query(), variables, "issue_drop_repos")?;
        let page = parse_page(&body, all.len());
        let advanced = !page.entries.is_empty();
        all.extend(page.entries);
        match page.next_cursor {
            // A cursor that returns nothing new would otherwise page forever.
            Some(next) if advanced => cursor = Some(next),
            _ => break,
        }
    }
    Ok(all)
}

/// Order the directory for presentation.
///
/// `local` and `fleet` are matched case-insensitively because git remotes and
/// peer summaries do not agree on the case of an owner, and presenting
/// `Gerchowl/flock` as unknown next to `gerchowl/flock` is a bug the operator
/// would have to debug rather than a cosmetic slip.
///
/// A repo named locally or in the fleet that the token cannot reach is still
/// listed: the operator knows it exists, and a directory that silently omits it
/// looks broken in exactly the way that sends someone back to the browser.
pub(crate) fn rank(entries: &[RepoEntry], local: &[String], fleet: &[String]) -> Vec<RankedRepo> {
    let fold = |values: &[String]| -> Vec<String> {
        values.iter().map(|v| v.to_ascii_lowercase()).collect()
    };
    let local_lc = fold(local);
    let fleet_lc = fold(fleet);

    let provenance_of = |name: &str| {
        let lc = name.to_ascii_lowercase();
        if local_lc.contains(&lc) {
            Provenance::LocalCheckout
        } else if fleet_lc.contains(&lc) {
            Provenance::SeenInFleet
        } else {
            Provenance::Reachable
        }
    };

    let mut ranked: Vec<(Provenance, usize, String)> = entries
        .iter()
        .map(|entry| {
            (
                provenance_of(&entry.name_with_owner),
                entry.recency_rank,
                entry.name_with_owner.clone(),
            )
        })
        .collect();

    // Known-but-unreachable destinations, appended so they cannot be lost.
    let present: Vec<String> = entries
        .iter()
        .map(|e| e.name_with_owner.to_ascii_lowercase())
        .collect();
    for (source, provenance) in [
        (local, Provenance::LocalCheckout),
        (fleet, Provenance::SeenInFleet),
    ] {
        for name in source {
            let lc = name.to_ascii_lowercase();
            if !present.contains(&lc) && name.contains('/') {
                ranked.push((provenance, usize::MAX, name.clone()));
            }
        }
    }

    ranked.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    // The same repo can arrive from both the directory and a peer summary.
    ranked.dedup_by(|a, b| a.2.eq_ignore_ascii_case(&b.2));
    ranked
        .into_iter()
        .map(|(provenance, _, name_with_owner)| RankedRepo {
            name_with_owner,
            provenance,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_body(names: &[(&str, bool, bool, bool)], has_next: bool, cursor: &str) -> String {
        let nodes: Vec<serde_json::Value> = names
            .iter()
            .map(|(name, can, enabled, archived)| {
                serde_json::json!({
                    "nameWithOwner": name,
                    "viewerCanCreateIssues": can,
                    "hasIssuesEnabled": enabled,
                    "isArchived": archived,
                })
            })
            .collect();
        serde_json::json!({
            "data": { "viewer": { "repositories": {
                "pageInfo": { "hasNextPage": has_next, "endCursor": cursor },
                "nodes": nodes,
            }}}
        })
        .to_string()
    }

    #[test]
    fn unfilable_repositories_are_dropped() {
        let body = page_body(
            &[
                ("o/writable", true, true, false),
                ("o/read-only", false, true, false),
                ("o/issues-off", true, false, false),
                ("o/archived", true, true, true),
            ],
            false,
            "",
        );
        let page = parse_page(&body, 0);
        let names: Vec<&str> = page
            .entries
            .iter()
            .map(|e| e.name_with_owner.as_str())
            .collect();
        assert_eq!(names, ["o/writable"]);
    }

    #[test]
    fn recency_rank_continues_across_pages() {
        // Page two must not restart at zero, or its first entry ties with page
        // one's and the sort silently interleaves them.
        let page_two = parse_page(&page_body(&[("o/later", true, true, false)], false, ""), 5);
        assert_eq!(page_two.entries[0].recency_rank, 5);
    }

    #[test]
    fn cursor_is_only_returned_when_another_page_exists() {
        let more = parse_page(&page_body(&[("o/a", true, true, false)], true, "CUR"), 0);
        assert_eq!(more.next_cursor.as_deref(), Some("CUR"));
        let last = parse_page(&page_body(&[("o/a", true, true, false)], false, "CUR"), 0);
        assert_eq!(last.next_cursor, None);
    }

    #[test]
    fn a_malformed_response_yields_nothing_rather_than_panicking() {
        assert_eq!(parse_page("not json", 0).entries.len(), 0);
        assert_eq!(parse_page(r#"{"data":null}"#, 0).entries.len(), 0);
        assert_eq!(parse_page(r#"{"data":{"viewer":{}}}"#, 0).next_cursor, None);
    }

    fn entry(name: &str, rank: usize) -> RepoEntry {
        RepoEntry {
            name_with_owner: name.to_string(),
            recency_rank: rank,
        }
    }

    #[test]
    fn ranking_puts_local_first_then_fleet_then_everything_else() {
        let entries = vec![
            entry("o/unseen", 0),
            entry("o/fleet", 1),
            entry("o/local", 2),
        ];
        let ranked = rank(&entries, &["o/local".to_string()], &["o/fleet".to_string()]);
        let names: Vec<&str> = ranked.iter().map(|r| r.name_with_owner.as_str()).collect();
        assert_eq!(names, ["o/local", "o/fleet", "o/unseen"]);
        assert_eq!(ranked[0].provenance, Provenance::LocalCheckout);
        assert_eq!(ranked[2].provenance, Provenance::Reachable);
    }

    #[test]
    fn recency_breaks_ties_within_a_tier() {
        let entries = vec![entry("o/older", 9), entry("o/newer", 1)];
        let ranked = rank(&entries, &[], &[]);
        let names: Vec<&str> = ranked.iter().map(|r| r.name_with_owner.as_str()).collect();
        assert_eq!(names, ["o/newer", "o/older"]);
    }

    #[test]
    fn owner_case_does_not_split_a_repo_into_two_tiers() {
        // git remotes and peer summaries disagree on owner case; without the
        // fold the local checkout lands in the Reachable tier and the operator
        // scrolls past 70 repos to find the one they are standing in.
        let entries = vec![entry("Gerchowl/flock", 0)];
        let ranked = rank(&entries, &["gerchowl/flock".to_string()], &[]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].provenance, Provenance::LocalCheckout);
    }

    #[test]
    fn a_known_repo_the_token_cannot_reach_is_still_offered() {
        // Dropping it would present a directory that looks broken: the
        // operator can see the worktree open in front of them.
        let ranked = rank(&[], &["o/private".to_string()], &[]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].name_with_owner, "o/private");
        assert_eq!(ranked[0].provenance, Provenance::LocalCheckout);
    }

    #[test]
    fn a_repo_in_both_local_and_fleet_appears_once_at_the_stronger_tier() {
        let entries = vec![entry("o/both", 0)];
        let ranked = rank(&entries, &["o/both".to_string()], &["o/both".to_string()]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].provenance, Provenance::LocalCheckout);
    }

    #[test]
    fn staleness_is_measured_against_the_ttl() {
        let now = Instant::now();
        let dir = RepoDirectory::new(vec![entry("o/a", 0)], now);
        assert!(!dir.is_stale(now));
        assert!(!dir.is_stale(now + DIRECTORY_TTL - Duration::from_secs(1)));
        assert!(dir.is_stale(now + DIRECTORY_TTL));
        // A clock that appears to run backwards must not abort the process.
        assert!(!dir.is_stale(now - Duration::from_secs(60)));
    }
}
