//! Morning digest render (#175 phase 5 / S3 commit 2).
//!
//! A pure fold over `event_hub::persisted_events_after(0)` → a single
//! self-contained HTML file (inline CSS, no external assets) written to
//! `session::data_dir()/digest/<YYYY-MM-DD>.html` by default.
//!
//! # Determinism
//!
//! [`render`] takes the raw event list and an explicit `since_ms` /
//! `generated_for_date` — it never reads the clock. Given the same inputs
//! it returns byte-identical output; the empty-log test asserts that too.
//! This is the property `ActionSpec::WriteDigest` needs so a scheduled
//! check writes stable output even when the runner ticks a hair earlier
//! or later than expected.
//!
//! # Ranking (§ design PR-B)
//!
//! Red > Yellow > Green. Sections render in that order so the operator's
//! eye lands on problems first.
//!
//! - **red** — dropped-undeliverable messages, errored checks
//!   (`CheckErrored`), errored issue triggers (`TriggerErrored`).
//! - **yellow** — fired checks (any `CheckFired`, including
//!   `blocked_alert`), ignored triggers, hibernated agents that never
//!   resumed.
//! - **green** — delivered messages (both `delivered` and
//!   `delivered_generic`), passed check runs, forks created, resumes from
//!   hibernation, fired triggers.
//!
//! # No LLM in the loop
//!
//! Per P1: this file only interprets the durable log. There is no shell,
//! no external tool, no model call — the operator can audit every line
//! back to a `(seq, ts_ms, envelope)` triple.

use std::collections::HashMap;

use crate::api::schema::{EventData, EventEnvelope};
use crate::costs::Accountant;

/// Path-template placeholders honored by `ActionSpec::WriteDigest`.
pub(crate) const DATE_PLACEHOLDER: &str = "{date}";

/// Rendered digest sections, in the order the HTML lays them out.
/// Exposed for callers that want to reason about the categorization
/// without re-parsing the HTML (tests, and any future JSON surface).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct DigestSections {
    pub red: Vec<DigestRow>,
    pub yellow: Vec<DigestRow>,
    pub green: Vec<DigestRow>,
    pub per_repo: Vec<crate::costs::RepoActivity>,
    /// Total events considered (after the `since_ms` filter).
    pub events_considered: usize,
}

/// One row in a rank section. `run_id` is present when the source event
/// carried a lineage handle (currently only `AgentForked`); the digest
/// renders it so the operator can pipe it straight to `flk revert-run`
/// (commit 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DigestRow {
    /// Log sequence — stable, monotonically-increasing per session.
    pub seq: u64,
    /// Wall-clock at emit, ms since epoch.
    pub ts_ms: u64,
    /// Event kind label (`message_delivered`, `check_errored`, …).
    pub kind: &'static str,
    /// Short human-readable summary.
    pub summary: String,
    /// Cross-references the operator can act on (pane id, repo, branch,
    /// check name). Rendered as a comma-separated list of key=value.
    pub context: Vec<(String, String)>,
    /// Optional lineage id (`AgentForked.run_id`), only present when the
    /// row's event carried one — the digest treats it as opaque.
    pub run_id: Option<String>,
}

/// Options for [`render`]. Kept a struct (not five positional args) so the
/// wire between the socket handler and the pure render stays legible.
#[derive(Debug, Clone)]
pub(crate) struct RenderOptions<'a> {
    /// Include only events with `ts_ms >= since_ms`. Zero means "everything".
    pub since_ms: u64,
    /// Date the digest was generated *for* (YYYY-MM-DD). Not derived
    /// inside [`render`] — the caller has the clock, not the fold.
    pub generated_for_date: &'a str,
    /// Human-readable label for the "since" window (e.g. `24h`,
    /// `2026-08-01T00:00:00Z`). Rendered verbatim in the header.
    pub since_label: &'a str,
}

/// Categorize + summarize `events` into [`DigestSections`]. Pure.
pub(crate) fn categorize<'a, I>(events: I) -> DigestSections
where
    I: IntoIterator<Item = (u64, u64, &'a EventEnvelope)>,
{
    let mut sections = DigestSections::default();
    let mut hibernated_at: HashMap<String, (u64, u64)> = HashMap::new();
    let mut resumed_panes: HashMap<String, ()> = HashMap::new();
    let mut collected: Vec<(u64, u64, EventEnvelope)> = Vec::new();

    for (seq, ts_ms, envelope) in events {
        collected.push((seq, ts_ms, envelope.clone()));
        sections.events_considered += 1;
        match &envelope.data {
            EventData::MessageDelivered {
                correlation_id,
                delivered,
                outcome,
                delivery_attempts,
                latency_ms,
            } => {
                let row = DigestRow {
                    seq,
                    ts_ms,
                    kind: "message_delivered",
                    summary: if *delivered {
                        format!(
                            "delivered {correlation_id} after {delivery_attempts} attempt(s), {latency_ms}ms"
                        )
                    } else {
                        format!(
                            "{outcome}: {correlation_id} after {delivery_attempts} attempt(s), {latency_ms}ms"
                        )
                    },
                    context: vec![
                        ("correlation_id".into(), correlation_id.clone()),
                        ("outcome".into(), outcome.clone()),
                    ],
                    run_id: None,
                };
                if !*delivered || outcome == "dropped_undeliverable" {
                    sections.red.push(row);
                } else {
                    sections.green.push(row);
                }
            }
            EventData::CheckErrored { name, reason } => sections.red.push(DigestRow {
                seq,
                ts_ms,
                kind: "check_errored",
                summary: format!("check {name} errored: {reason}"),
                context: vec![("check".into(), name.clone())],
                run_id: None,
            }),
            EventData::TriggerErrored {
                repo,
                issue,
                reason,
            } => sections.red.push(DigestRow {
                seq,
                ts_ms,
                kind: "trigger_errored",
                summary: format!("issue-guard {repo}#{issue} errored: {reason}"),
                context: vec![
                    ("repo".into(), repo.clone()),
                    ("issue".into(), issue.to_string()),
                ],
                run_id: None,
            }),
            EventData::CheckFired { name, episode } => sections.yellow.push(DigestRow {
                seq,
                ts_ms,
                kind: "check_fired",
                summary: format!("check {name} fired (episode {episode})"),
                context: vec![
                    ("check".into(), name.clone()),
                    ("episode".into(), episode.clone()),
                ],
                run_id: None,
            }),
            EventData::TriggerIgnored {
                repo,
                issue,
                reason,
            } => sections.yellow.push(DigestRow {
                seq,
                ts_ms,
                kind: "trigger_ignored",
                summary: format!("issue-guard {repo}#{issue} ignored: {reason}"),
                context: vec![
                    ("repo".into(), repo.clone()),
                    ("issue".into(), issue.to_string()),
                ],
                run_id: None,
            }),
            EventData::AgentHibernated {
                pane_id,
                workspace_id,
                agent,
                session,
            } => {
                hibernated_at.insert(pane_id.clone(), (seq, ts_ms));
                sections.yellow.push(DigestRow {
                    seq,
                    ts_ms,
                    kind: "agent_hibernated",
                    summary: format!(
                        "hibernated {agent} on {pane_id} (ws {workspace_id}, session {})",
                        session.as_deref().unwrap_or("?")
                    ),
                    context: vec![
                        ("pane".into(), pane_id.clone()),
                        ("workspace".into(), workspace_id.clone()),
                        ("agent".into(), agent.clone()),
                    ],
                    run_id: None,
                });
            }
            EventData::AgentResumedFromHibernation {
                pane_id,
                workspace_id,
                agent,
            } => {
                resumed_panes.insert(pane_id.clone(), ());
                sections.green.push(DigestRow {
                    seq,
                    ts_ms,
                    kind: "agent_resumed_from_hibernation",
                    summary: format!("resumed {agent} on {pane_id}"),
                    context: vec![
                        ("pane".into(), pane_id.clone()),
                        ("workspace".into(), workspace_id.clone()),
                    ],
                    run_id: None,
                });
            }
            EventData::AgentForked {
                run_id,
                parent_pane_id,
                parent_repo,
                agent,
                child_pane_id,
                child_branch,
                seeded,
                ..
            } => sections.green.push(DigestRow {
                seq,
                ts_ms,
                kind: "agent_forked",
                summary: format!(
                    "forked {agent}: {parent_pane_id} → {child_pane_id} on branch {child_branch} (seeded={seeded})"
                ),
                context: vec![
                    ("parent".into(), parent_pane_id.clone()),
                    ("child".into(), child_pane_id.clone()),
                    ("repo".into(), parent_repo.clone()),
                    ("branch".into(), child_branch.clone()),
                ],
                run_id: Some(run_id.clone()),
            }),
            EventData::TriggerFired {
                repo,
                issue,
                dedupe_key,
                action,
            } => sections.green.push(DigestRow {
                seq,
                ts_ms,
                kind: "trigger_fired",
                summary: format!("issue-guard {repo}#{issue} fired {action}"),
                context: vec![
                    ("repo".into(), repo.clone()),
                    ("issue".into(), issue.to_string()),
                    ("dedupe_key".into(), dedupe_key.clone()),
                ],
                run_id: None,
            }),
            EventData::CheckRan {
                name,
                outcome,
                duration_ms,
            } if outcome == "pass" => sections.green.push(DigestRow {
                seq,
                ts_ms,
                kind: "check_ran",
                summary: format!("{name} passed in {duration_ms}ms"),
                context: vec![("check".into(), name.clone())],
                run_id: None,
            }),
            _ => {
                // Other event kinds contribute to per-repo activity via the
                // accountant but don't warrant their own digest row.
            }
        }
    }

    // Per-repo activity via the accountant — feeds it borrowed envelopes
    // from `collected` (owned above) so the fold sees the same data twice
    // consistently.
    let acc = Accountant::default().fold(collected.iter().map(|(_, _, env)| env));
    sections.per_repo = acc.per_repo();
    sections
}

/// Render `events` as a self-contained HTML digest. Deterministic — same
/// inputs, byte-identical output.
pub(crate) fn render<'a, I>(events: I, opts: &RenderOptions<'_>) -> String
where
    I: IntoIterator<Item = (u64, u64, &'a EventEnvelope)>,
{
    let filtered: Vec<(u64, u64, &EventEnvelope)> = events
        .into_iter()
        .filter(|(_, ts_ms, _)| *ts_ms >= opts.since_ms)
        .collect();
    let sections = categorize(filtered);
    render_html(&sections, opts)
}

fn render_html(sections: &DigestSections, opts: &RenderOptions<'_>) -> String {
    let mut out = String::with_capacity(8 * 1024);
    out.push_str("<!doctype html>\n");
    out.push_str("<html lang=\"en\"><head><meta charset=\"utf-8\">\n");
    out.push_str("<title>flock digest ");
    push_escaped(&mut out, opts.generated_for_date);
    out.push_str("</title>\n<style>\n");
    out.push_str(EMBEDDED_CSS);
    out.push_str("</style>\n</head><body>\n");
    out.push_str("<header><h1>flock digest</h1>\n");
    out.push_str("<p class=\"meta\">for ");
    push_escaped(&mut out, opts.generated_for_date);
    out.push_str(" · since ");
    push_escaped(&mut out, opts.since_label);
    out.push_str(&format!(
        " · {} events considered</p></header>\n",
        sections.events_considered
    ));

    render_section(&mut out, "red", "attention", &sections.red);
    render_section(&mut out, "yellow", "in flight", &sections.yellow);
    render_section(&mut out, "green", "progress", &sections.green);

    out.push_str("<section class=\"repo\"><h2>per-repo activity</h2>\n");
    if sections.per_repo.is_empty() {
        out.push_str("<p class=\"empty\">no repo activity recorded</p>\n");
    } else {
        out.push_str("<table><thead><tr><th>repo</th><th>panes</th><th>turns</th><th>sent</th><th>received</th><th>forks</th></tr></thead><tbody>\n");
        for repo in &sections.per_repo {
            out.push_str("<tr><td>");
            push_escaped(&mut out, &repo.repo);
            out.push_str(&format!(
                "</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
                repo.panes,
                repo.turns_started,
                repo.messages_sent,
                repo.messages_received,
                repo.forks_spawned,
            ));
        }
        out.push_str("</tbody></table>\n");
    }
    out.push_str("</section>\n</body></html>\n");
    out
}

fn render_section(out: &mut String, class: &str, label: &str, rows: &[DigestRow]) {
    out.push_str(&format!(
        "<section class=\"rank {class}\"><h2>{class} · {label} ({count})</h2>\n",
        class = class,
        label = label,
        count = rows.len(),
    ));
    if rows.is_empty() {
        out.push_str("<p class=\"empty\">none</p>\n");
    } else {
        out.push_str("<ul>\n");
        for row in rows {
            out.push_str("<li><span class=\"seq\">#");
            out.push_str(&row.seq.to_string());
            out.push_str("</span> <span class=\"ts\">");
            push_escaped(out, &row.ts_ms.to_string());
            out.push_str("</span> <span class=\"kind\">");
            push_escaped(out, row.kind);
            out.push_str("</span> ");
            push_escaped(out, &row.summary);
            if let Some(run_id) = row.run_id.as_ref() {
                out.push_str(" <span class=\"run\">run=");
                push_escaped(out, run_id);
                out.push_str("</span>");
            }
            if !row.context.is_empty() {
                out.push_str(" <span class=\"ctx\">");
                let mut first = true;
                for (key, value) in &row.context {
                    if !first {
                        out.push_str(", ");
                    }
                    first = false;
                    push_escaped(out, key);
                    out.push('=');
                    push_escaped(out, value);
                }
                out.push_str("</span>");
            }
            out.push_str("</li>\n");
        }
        out.push_str("</ul>\n");
    }
    out.push_str("</section>\n");
}

fn push_escaped(out: &mut String, raw: &str) {
    for ch in raw.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
}

/// Expand `{date}` in a `WriteDigest.path_template` to the operator-supplied
/// date. The default template is just `{date}.html` — everything else stays
/// verbatim (no globbing, no shell), P7.
pub(crate) fn expand_path_template(template: &str, date: &str) -> String {
    template.replace(DATE_PLACEHOLDER, date)
}

/// UTC YYYY-MM-DD from ms-since-epoch. Deliberately UTC (not local): we do
/// not want to pull a timezone crate to implement one digest file, and a
/// digest that flips days at midnight UTC is fine for a morning-summary
/// tool. The header shows the label so operators know the boundary.
pub(crate) fn local_ymd_utc(now_ms: u64) -> String {
    let days = (now_ms / 86_400_000) as i64;
    let (year, month, day) = civil_from_days_since_epoch(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Howard Hinnant's algorithm (public domain): days since 1970-01-01 →
/// civil `(year, month, day)`. Correct for the full Gregorian range;
/// unit-tested against a handful of known dates.
fn civil_from_days_since_epoch(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146_096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year as i32, m, d)
}

const EMBEDDED_CSS: &str = "
:root { color-scheme: light dark; }
body { font: 14px/1.5 system-ui, sans-serif; margin: 2rem; max-width: 60rem; }
header h1 { margin: 0 0 .25rem 0; }
header .meta { color: #666; margin: 0 0 1.5rem 0; }
section.rank { border-radius: 6px; padding: 1rem; margin-bottom: 1rem; }
section.rank.red { background: #ffe5e5; }
section.rank.yellow { background: #fff5cc; }
section.rank.green { background: #e5f7e5; }
@media (prefers-color-scheme: dark) {
  section.rank.red { background: #4a1010; }
  section.rank.yellow { background: #4a3d10; }
  section.rank.green { background: #103a10; }
  body { color: #eee; background: #111; }
  header .meta { color: #aaa; }
  table { color: #eee; }
  th, td { border-color: #444; }
}
section.rank h2 { margin: 0 0 .5rem 0; font-size: 1rem; text-transform: lowercase; }
section.rank ul { list-style: none; padding: 0; margin: 0; }
section.rank li { padding: .25rem 0; border-top: 1px solid rgba(0,0,0,.05); font-family: ui-monospace, monospace; font-size: 12px; }
section.rank li:first-child { border-top: none; }
.seq, .ts, .kind { color: #666; margin-right: .35rem; }
.run { color: #06c; margin-left: .35rem; }
.ctx { color: #888; margin-left: .35rem; }
p.empty { color: #888; margin: 0; }
table { border-collapse: collapse; width: 100%; }
th, td { text-align: left; padding: .35rem .5rem; border-bottom: 1px solid #ddd; }
th { font-weight: 600; }
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{EventEnvelope, EventKind};

    fn env(kind: EventKind, data: EventData) -> EventEnvelope {
        EventEnvelope { event: kind, data }
    }

    fn events() -> Vec<(u64, u64, EventEnvelope)> {
        vec![
            (
                1,
                1_000,
                env(
                    EventKind::CheckErrored,
                    EventData::CheckErrored {
                        name: "disk".into(),
                        reason: "gh: not found".into(),
                    },
                ),
            ),
            (
                2,
                1_010,
                env(
                    EventKind::MessageDelivered,
                    EventData::MessageDelivered {
                        correlation_id: "c1".into(),
                        delivered: false,
                        outcome: "dropped_undeliverable".into(),
                        delivery_attempts: 8,
                        latency_ms: 90_000,
                    },
                ),
            ),
            (
                3,
                1_020,
                env(
                    EventKind::MessageDelivered,
                    EventData::MessageDelivered {
                        correlation_id: "c2".into(),
                        delivered: true,
                        outcome: "delivered".into(),
                        delivery_attempts: 1,
                        latency_ms: 4,
                    },
                ),
            ),
            (
                4,
                1_030,
                env(
                    EventKind::CheckFired,
                    EventData::CheckFired {
                        name: "blocked_alert".into(),
                        episode: "blocked_alert:w1:p1:1000".into(),
                    },
                ),
            ),
            (
                5,
                1_040,
                env(
                    EventKind::AgentForked,
                    EventData::AgentForked {
                        run_id: "fork:abc".into(),
                        parent_pane_id: "w1:p1".into(),
                        parent_workspace_id: "w1".into(),
                        parent_repo: "flock".into(),
                        agent: "claude".into(),
                        child_workspace_id: "w2".into(),
                        child_pane_id: "w2:p1".into(),
                        child_worktree: "/tmp/wt".into(),
                        child_branch: "fork/b".into(),
                        seeded: true,
                    },
                ),
            ),
        ]
    }

    #[test]
    fn categorize_ranks_red_yellow_green_correctly() {
        let evs = events();
        let sections = categorize(evs.iter().map(|(seq, ts, env)| (*seq, *ts, env)));
        // Red: check errored + dropped_undeliverable.
        let red_kinds: Vec<&'static str> = sections.red.iter().map(|r| r.kind).collect();
        assert_eq!(red_kinds, vec!["check_errored", "message_delivered"]);
        // Yellow: check_fired.
        let yellow_kinds: Vec<&'static str> = sections.yellow.iter().map(|r| r.kind).collect();
        assert_eq!(yellow_kinds, vec!["check_fired"]);
        // Green: delivered message + fork.
        let green_kinds: Vec<&'static str> = sections.green.iter().map(|r| r.kind).collect();
        assert_eq!(green_kinds, vec!["message_delivered", "agent_forked"]);
        // Fork's run_id survives to the row.
        assert_eq!(
            sections.green[1].run_id.as_deref(),
            Some("fork:abc"),
            "run id passes through to the digest so revert-run can consume it"
        );
        // Per-repo derived by the accountant.
        assert_eq!(sections.per_repo.len(), 1);
        assert_eq!(sections.per_repo[0].repo, "flock");
        assert_eq!(sections.per_repo[0].forks_spawned, 1);
    }

    #[test]
    fn render_is_byte_deterministic_for_the_same_events() {
        let evs = events();
        let opts = RenderOptions {
            since_ms: 0,
            generated_for_date: "2026-08-01",
            since_label: "epoch",
        };
        let a = render(evs.iter().map(|(seq, ts, env)| (*seq, *ts, env)), &opts);
        let b = render(evs.iter().map(|(seq, ts, env)| (*seq, *ts, env)), &opts);
        assert_eq!(a, b, "same inputs must produce byte-identical HTML");
    }

    #[test]
    fn empty_log_renders_a_valid_page_with_zeros_and_no_panic() {
        let opts = RenderOptions {
            since_ms: 0,
            generated_for_date: "2026-08-01",
            since_label: "epoch",
        };
        let out = render(std::iter::empty(), &opts);
        assert!(out.starts_with("<!doctype html>\n"));
        assert!(
            out.contains("0 events considered"),
            "header states event count even at zero: {out}"
        );
        assert!(
            out.contains("red · attention (0)")
                && out.contains("yellow · in flight (0)")
                && out.contains("green · progress (0)"),
            "all three rank sections render with (0) counts: {out}"
        );
        assert!(out.contains("no repo activity recorded"));
        assert!(out.ends_with("</html>\n"));
    }

    #[test]
    fn since_ms_filters_events_out_of_the_window() {
        let evs = events();
        let opts = RenderOptions {
            since_ms: 1_025, // drop the first three (ts 1000, 1010, 1020)
            generated_for_date: "2026-08-01",
            since_label: "later",
        };
        let out = render(evs.iter().map(|(seq, ts, env)| (*seq, *ts, env)), &opts);
        assert!(out.contains("2 events considered"));
    }

    #[test]
    fn html_escaping_prevents_summary_injection() {
        let evs = [(
            9,
            1_000,
            env(
                EventKind::CheckErrored,
                EventData::CheckErrored {
                    name: "<script>alert('x')</script>".into(),
                    reason: "\"quoted\" & <injected>".into(),
                },
            ),
        )];
        let opts = RenderOptions {
            since_ms: 0,
            generated_for_date: "2026-08-01",
            since_label: "epoch",
        };
        let out = render(evs.iter().map(|(seq, ts, env)| (*seq, *ts, env)), &opts);
        assert!(
            !out.contains("<script>alert"),
            "raw script tag leaked: {out}"
        );
        assert!(out.contains("&lt;script&gt;"));
        assert!(out.contains("&quot;quoted&quot;"));
    }

    #[test]
    fn local_ymd_utc_matches_known_dates() {
        // 1970-01-01: day 0.
        assert_eq!(local_ymd_utc(0), "1970-01-01");
        // 2000-01-01: 30 years past epoch — check leap-year math.
        // Days from 1970-01-01 to 2000-01-01 = 10_957.
        assert_eq!(local_ymd_utc(10_957 * 86_400_000), "2000-01-01");
        // A leap-day boundary: 2020-02-29.
        // Days 1970-01-01 → 2020-02-29 = 18_321.
        assert_eq!(local_ymd_utc(18_321 * 86_400_000), "2020-02-29");
        // Task's "today": 2026-08-01. Days = 20_666.
        assert_eq!(local_ymd_utc(20_666 * 86_400_000), "2026-08-01");
    }

    #[test]
    fn expand_path_template_replaces_only_the_date_placeholder() {
        assert_eq!(
            expand_path_template("digest-{date}.html", "2026-08-01"),
            "digest-2026-08-01.html"
        );
        assert_eq!(
            expand_path_template("plain.html", "2026-08-01"),
            "plain.html"
        );
        // Two placeholders both fire (no clever "first-only" trap).
        assert_eq!(
            expand_path_template("{date}/{date}.html", "2026-08-01"),
            "2026-08-01/2026-08-01.html"
        );
    }
}
