// The digest render (S3 commit 2) folds an entire durable event log through
// this accountant; the fleet-pause banner and revert-run tool don't touch
// it. Between landing this file and landing the digest the accountant is
// not called from non-test code, so pre-mark it — matches the pattern used
// elsewhere in the tree (see `runner::RunnableCheck.check`).
#![allow(dead_code, reason = "consumed by the digest render in S3 commit 2")]

//! Per-agent spend accountant (#175 phase 5 / S3 commit 1).
//!
//! # Why this is a turn/message counter, not a dollar meter
//!
//! The accepted phases-5-6 design proposes `[costs.<agent>]` with
//! `prompt_usd_per_mtok` / `completion_usd_per_mtok` so a caller can multiply
//! tokens by USD-per-Mtok and get a real spend figure. On this branch **the
//! durable event log does not carry token counts on any event kind** — the
//! `PaneReport*` methods (see `src/api/schema.rs` §660–693) are RPC methods,
//! not persisted events, and they don't carry `usage` either. Inventing a
//! dollar figure from turn or message counts alone would be a lie the
//! operator can't audit.
//!
//! So this accountant restricts itself to what the log truthfully proves:
//! - `turns_started` — one per `PaneAgentStatusChanged { agent_status:
//!   Working }` (the pane went from something else back into Working).
//! - `messages_sent`  — `MessageQueued.from_pane == this pane`.
//! - `messages_received` — `MessageQueued.to_pane == this pane`.
//! - `forks_spawned` — `AgentForked.parent_pane_id == this pane`.
//!
//! `[costs.<agent>]` is deliberately absent from `Config` until a token
//! event lands; adding it now would leak an unused surface into the schema
//! and give operators the false impression that dollar figures already work.
//! The [`Accountant`] exposes only the truthful counts, and the digest
//! (commit 2) and any future sidebar widget render them as such.
//!
//! # Fold contract
//!
//! [`Accountant::fold`] is a pure fold over an event slice — no clock, no
//! I/O, no state carried across calls. Given the same event list it returns
//! the same activity tables; the tests below rely on that.

use std::collections::HashMap;

use crate::api::schema::{AgentStatus, EventData, EventEnvelope};

/// Truthful per-pane activity extracted from the durable event log.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PaneActivity {
    /// Public pane id (`ws:pN`), stable across restarts.
    pub pane_id: String,
    /// The pane's workspace, when a status event or lineage event ever
    /// stamped one on the log.
    pub workspace_id: Option<String>,
    /// Latest agent label the log observed for this pane (`claude`,
    /// `codex`, …). None when the pane never emitted a labelled status.
    pub agent: Option<String>,
    /// Repo the pane is associated with — derived from `MessageQueued`
    /// (from_repo / to_repo) or `AgentForked.parent_repo` for the parent.
    /// None when no persisted event tied this pane to a repo.
    pub repo: Option<String>,
    /// `PaneAgentStatusChanged` transitions INTO `Working` (Idle/Done/…→Working).
    /// A truthful proxy for "how many turns did this agent work". Repeat
    /// Working events in a row (very rare, from resend of the same status)
    /// count once — we only bump on the *transition*.
    pub turns_started: u32,
    /// `MessageQueued.from_pane == pane_id`.
    pub messages_sent: u32,
    /// `MessageQueued.to_pane == pane_id`.
    pub messages_received: u32,
    /// `AgentForked.parent_pane_id == pane_id`.
    pub forks_spawned: u32,
}

/// Truthful per-repo activity, aggregated across every pane whose event
/// stream mentioned a repo (`MessageQueued.from_repo|to_repo`,
/// `AgentForked.parent_repo`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct RepoActivity {
    pub repo: String,
    /// Panes with at least one recorded event tied to this repo.
    pub panes: u32,
    pub turns_started: u32,
    pub messages_sent: u32,
    pub messages_received: u32,
    pub forks_spawned: u32,
}

/// Pure fold — nothing else. Kept a struct (not just a free function) so
/// callers hold an opaque view and future extensions (per-agent buckets,
/// once tokens land) don't reshape signatures.
#[derive(Debug, Clone, Default)]
pub(crate) struct Accountant {
    per_pane: HashMap<String, PaneActivity>,
    /// Last non-Working status seen for each pane, so a repeated Working
    /// event doesn't inflate `turns_started`.
    last_status: HashMap<String, AgentStatus>,
}

impl Accountant {
    /// Fold `events` into per-pane activity. Idempotent w.r.t. a fresh
    /// accountant: `Accountant::default().fold(events).per_pane()` is a
    /// pure function of the event slice.
    pub(crate) fn fold<'a>(mut self, events: impl IntoIterator<Item = &'a EventEnvelope>) -> Self {
        for envelope in events {
            match &envelope.data {
                EventData::PaneAgentStatusChanged {
                    pane_id,
                    workspace_id,
                    agent_status,
                    agent,
                    ..
                } => {
                    let entry =
                        self.per_pane
                            .entry(pane_id.clone())
                            .or_insert_with(|| PaneActivity {
                                pane_id: pane_id.clone(),
                                ..PaneActivity::default()
                            });
                    if entry.workspace_id.is_none() {
                        entry.workspace_id = Some(workspace_id.clone());
                    }
                    if let Some(name) = agent.as_ref() {
                        entry.agent = Some(name.clone());
                    }
                    // Transitions INTO Working count as a turn start. The
                    // very first observed status also counts if it's Working
                    // (the pane came online mid-task).
                    let previous = self.last_status.get(pane_id).copied();
                    if matches!(agent_status, AgentStatus::Working)
                        && previous != Some(AgentStatus::Working)
                    {
                        entry.turns_started = entry.turns_started.saturating_add(1);
                    }
                    self.last_status.insert(pane_id.clone(), *agent_status);
                }
                EventData::MessageQueued {
                    from_agent: None,
                    from_host: None,
                    from_pane,
                    from_repo,
                    to_pane,
                    to_repo,
                    ..
                } => {
                    if let Some(from) = from_pane.as_ref() {
                        let entry =
                            self.per_pane
                                .entry(from.clone())
                                .or_insert_with(|| PaneActivity {
                                    pane_id: from.clone(),
                                    ..PaneActivity::default()
                                });
                        entry.messages_sent = entry.messages_sent.saturating_add(1);
                        if entry.repo.is_none() {
                            entry.repo = from_repo.clone();
                        }
                    }
                    let entry =
                        self.per_pane
                            .entry(to_pane.clone())
                            .or_insert_with(|| PaneActivity {
                                pane_id: to_pane.clone(),
                                ..PaneActivity::default()
                            });
                    entry.messages_received = entry.messages_received.saturating_add(1);
                    if entry.repo.is_none() {
                        entry.repo = to_repo.clone();
                    }
                }
                EventData::AgentForked {
                    parent_pane_id,
                    parent_workspace_id,
                    parent_repo,
                    agent,
                    ..
                } => {
                    let entry = self
                        .per_pane
                        .entry(parent_pane_id.clone())
                        .or_insert_with(|| PaneActivity {
                            pane_id: parent_pane_id.clone(),
                            ..PaneActivity::default()
                        });
                    entry.forks_spawned = entry.forks_spawned.saturating_add(1);
                    if entry.workspace_id.is_none() {
                        entry.workspace_id = Some(parent_workspace_id.clone());
                    }
                    if entry.agent.is_none() {
                        entry.agent = Some(agent.clone());
                    }
                    if entry.repo.is_none() {
                        entry.repo = Some(parent_repo.clone());
                    }
                }
                _ => {}
            }
        }
        self
    }

    /// Panes with any recorded activity, sorted by pane id for deterministic
    /// downstream rendering (digest, tests).
    pub(crate) fn per_pane(&self) -> Vec<PaneActivity> {
        let mut out: Vec<PaneActivity> = self.per_pane.values().cloned().collect();
        out.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
        out
    }

    /// Aggregate the per-pane view by repo. Panes with no repo association
    /// are omitted from this view (there is nothing to aggregate them
    /// under). Sorted by repo name.
    pub(crate) fn per_repo(&self) -> Vec<RepoActivity> {
        let mut per_repo: HashMap<String, RepoActivity> = HashMap::new();
        for pane in self.per_pane.values() {
            let Some(repo) = pane.repo.as_ref() else {
                continue;
            };
            let entry = per_repo
                .entry(repo.clone())
                .or_insert_with(|| RepoActivity {
                    repo: repo.clone(),
                    ..RepoActivity::default()
                });
            entry.panes = entry.panes.saturating_add(1);
            entry.turns_started = entry.turns_started.saturating_add(pane.turns_started);
            entry.messages_sent = entry.messages_sent.saturating_add(pane.messages_sent);
            entry.messages_received = entry
                .messages_received
                .saturating_add(pane.messages_received);
            entry.forks_spawned = entry.forks_spawned.saturating_add(pane.forks_spawned);
        }
        let mut out: Vec<RepoActivity> = per_repo.into_values().collect();
        out.sort_by(|a, b| a.repo.cmp(&b.repo));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{EventEnvelope, EventKind};
    use std::collections::HashMap;

    fn status(pane: &str, ws: &str, agent: &str, status: AgentStatus) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::PaneAgentStatusChanged,
            data: EventData::PaneAgentStatusChanged {
                pane_id: pane.into(),
                workspace_id: ws.into(),
                agent_status: status,
                agent: Some(agent.into()),
                title: None,
                display_agent: None,
                custom_status: None,
                state_labels: HashMap::new(),
            },
        }
    }

    fn queued(
        cid: &str,
        from_pane: Option<&str>,
        from_repo: Option<&str>,
        to_pane: &str,
        to_repo: Option<&str>,
    ) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::MessageQueued,
            data: EventData::MessageQueued {
                from_agent: None,
                from_host: None,
                correlation_id: cid.into(),
                from_pane: from_pane.map(str::to_string),
                from_repo: from_repo.map(str::to_string),
                to_pane: to_pane.into(),
                to_repo: to_repo.map(str::to_string),
                cross_repo: from_repo != to_repo,
                in_reply_to: None,
                enqueued_at_ms: 0,
                intent: crate::api::schema::MsgIntent::Fyi,
                body: "hi".into(),
            },
        }
    }

    fn forked(parent_pane: &str, parent_ws: &str, parent_repo: &str, agent: &str) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::AgentForked,
            data: EventData::AgentForked {
                run_id: format!("fork:{parent_pane}-x"),
                parent_pane_id: parent_pane.into(),
                parent_workspace_id: parent_ws.into(),
                parent_repo: parent_repo.into(),
                agent: agent.into(),
                child_workspace_id: format!("{parent_ws}-child"),
                child_pane_id: format!("{parent_pane}-child"),
                child_worktree: "/tmp/wt".into(),
                child_branch: "b".into(),
                seeded: true,
            },
        }
    }

    #[test]
    fn empty_fold_yields_no_activity() {
        let acc = Accountant::default().fold(&[]);
        assert!(acc.per_pane().is_empty());
        assert!(acc.per_repo().is_empty());
    }

    #[test]
    fn working_transitions_count_but_repeat_working_does_not() {
        // Idle → Working → Working → Idle → Working
        // Two DISTINCT transitions INTO Working: 2 turns, not 3.
        let events = [
            status("w1:p1", "w1", "claude", AgentStatus::Idle),
            status("w1:p1", "w1", "claude", AgentStatus::Working),
            status("w1:p1", "w1", "claude", AgentStatus::Working),
            status("w1:p1", "w1", "claude", AgentStatus::Idle),
            status("w1:p1", "w1", "claude", AgentStatus::Working),
        ];
        let acc = Accountant::default().fold(&events);
        let per_pane = acc.per_pane();
        assert_eq!(per_pane.len(), 1);
        assert_eq!(per_pane[0].turns_started, 2);
        assert_eq!(per_pane[0].agent.as_deref(), Some("claude"));
        assert_eq!(per_pane[0].workspace_id.as_deref(), Some("w1"));
    }

    #[test]
    fn first_status_working_still_counts_as_one_turn() {
        let acc =
            Accountant::default().fold(&[status("w2:p1", "w2", "claude", AgentStatus::Working)]);
        let per_pane = acc.per_pane();
        assert_eq!(per_pane[0].turns_started, 1);
    }

    #[test]
    fn messages_count_from_and_to_and_populate_repo() {
        let events = [
            queued(
                "c1",
                Some("w1:p1"),
                Some("flock"),
                "w2:p1",
                Some("dotfiles"),
            ),
            queued(
                "c2",
                Some("w2:p1"),
                Some("dotfiles"),
                "w1:p1",
                Some("flock"),
            ),
            queued("c3", None, None, "w1:p1", None), // no sender
        ];
        let acc = Accountant::default().fold(&events);
        let panes = acc.per_pane();
        let by_id: HashMap<_, _> = panes.iter().map(|p| (p.pane_id.clone(), p)).collect();
        let a = by_id.get("w1:p1").expect("w1:p1 tracked");
        assert_eq!(a.messages_sent, 1);
        assert_eq!(a.messages_received, 2);
        assert_eq!(a.repo.as_deref(), Some("flock"));
        let b = by_id.get("w2:p1").expect("w2:p1 tracked");
        assert_eq!(b.messages_sent, 1);
        assert_eq!(b.messages_received, 1);
        assert_eq!(b.repo.as_deref(), Some("dotfiles"));
    }

    #[test]
    fn forks_count_on_parent_and_stamp_repo_agent() {
        let events = [
            forked("w1:p1", "w1", "flock", "claude"),
            forked("w1:p1", "w1", "flock", "claude"),
        ];
        let acc = Accountant::default().fold(&events);
        let panes = acc.per_pane();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].forks_spawned, 2);
        assert_eq!(panes[0].agent.as_deref(), Some("claude"));
        assert_eq!(panes[0].repo.as_deref(), Some("flock"));
    }

    #[test]
    fn per_repo_aggregates_across_panes() {
        let events = [
            status("w1:p1", "w1", "claude", AgentStatus::Idle),
            status("w1:p1", "w1", "claude", AgentStatus::Working),
            queued("c1", Some("w1:p1"), Some("flock"), "w2:p1", Some("flock")),
            queued("c2", Some("w1:p1"), Some("flock"), "w3:p1", Some("flock")),
            forked("w1:p1", "w1", "flock", "claude"),
            queued(
                "c3",
                Some("w9:p1"),
                Some("dotfiles"),
                "w9:p2",
                Some("dotfiles"),
            ),
        ];
        let acc = Accountant::default().fold(&events);
        let per_repo = acc.per_repo();
        assert_eq!(per_repo.len(), 2, "flock + dotfiles");
        let flock = per_repo.iter().find(|r| r.repo == "flock").unwrap();
        // w1:p1 sends 2, w2:p1 receives 1, w3:p1 receives 1 → 3 panes on
        // flock; 2 sent, 2 received, 1 turn, 1 fork.
        assert_eq!(flock.panes, 3);
        assert_eq!(flock.messages_sent, 2);
        assert_eq!(flock.messages_received, 2);
        assert_eq!(flock.turns_started, 1);
        assert_eq!(flock.forks_spawned, 1);
        let dotfiles = per_repo.iter().find(|r| r.repo == "dotfiles").unwrap();
        assert_eq!(dotfiles.panes, 2);
        assert_eq!(dotfiles.messages_sent, 1);
        assert_eq!(dotfiles.messages_received, 1);
    }

    #[test]
    fn fold_is_deterministic_and_pure() {
        // Same event list, two folds → byte-identical outputs. This is the
        // property the digest test in commit 2 depends on.
        let events = [
            status("w1:p1", "w1", "claude", AgentStatus::Working),
            queued("c1", Some("w1:p1"), Some("flock"), "w2:p1", Some("flock")),
            forked("w1:p1", "w1", "flock", "claude"),
        ];
        let a = Accountant::default().fold(&events).per_pane();
        let b = Accountant::default().fold(&events).per_pane();
        assert_eq!(a, b);
    }

    #[test]
    fn non_activity_events_are_ignored() {
        // WorkspaceCreated, PaneClosed etc. must never inflate any counter.
        let events = [
            EventEnvelope {
                event: EventKind::PaneClosed,
                data: EventData::PaneClosed {
                    pane_id: "w1:p1".into(),
                    workspace_id: "w1".into(),
                },
            },
            EventEnvelope {
                event: EventKind::CheckRan,
                data: EventData::CheckRan {
                    name: "x".into(),
                    outcome: "pass".into(),
                    duration_ms: 5,
                },
            },
        ];
        let acc = Accountant::default().fold(&events);
        assert!(acc.per_pane().is_empty(), "no activity to count");
    }
}
