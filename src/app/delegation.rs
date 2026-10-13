//! The architect -> delegate edge (#959, ADR-0027 §1).
//!
//! An agent that starts another agent through `agent.start`, `agent.spawn` or
//! `agent.fork` is recorded as that agent's architect by a durable
//! `AgentDelegated` event. The architect is the caller `spawn_caller()`
//! attested, never a parameter: the edge will carry teardown authority, so a
//! spoofable edge would be a way to get another agent's worktree reaped.
//!
//! The live index is a fold over the durable log, the same way `agent.lineage`
//! folds `AgentForked`. Unlike lineage it does not accept losing an old edge to
//! log rotation: once per server start every OPEN edge is re-emitted with
//! `reissued: true`, so an open edge always has a copy in a recent log file.

use std::collections::{BTreeMap, HashSet};

use crate::api::schema::{DelegationVia, EventData, EventEnvelope, EventKind};

use super::{App, SpawnCaller};

/// Whether the delegate an edge points at is still there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EdgeState {
    Live,
    /// The delegate was removed with no resumable session, or is missing from
    /// the live agent records. The edge stays open while it may own a
    /// worktree, so the worktree is not leaked with nothing pointing at it.
    Gone,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DelegationEdge {
    pub(crate) architect: String,
    pub(crate) delegate: String,
    pub(crate) via: DelegationVia,
    pub(crate) workspace: String,
    pub(crate) worktree: Option<String>,
    pub(crate) branch: Option<String>,
    pub(crate) repo: Option<String>,
    pub(crate) label: Option<String>,
    pub(crate) state: EdgeState,
    /// When the edge was first recorded, in Unix ms. A re-issued copy keeps
    /// the original's time while the original is still in the log.
    pub(crate) since_ms: u64,
}

impl DelegationEdge {
    fn event(&self, reissued: bool) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::AgentDelegated,
            data: EventData::AgentDelegated {
                architect: self.architect.clone(),
                delegate: self.delegate.clone(),
                via: self.via,
                workspace: self.workspace.clone(),
                worktree: self.worktree.clone(),
                branch: self.branch.clone(),
                repo: self.repo.clone(),
                label: self.label.clone(),
                reissued,
            },
        }
    }
}

/// Open edges, keyed by delegate. A delegate has one architect: an `AgentId`
/// is minted once and retired on removal, so no id is ever delegated twice.
#[derive(Debug, Default)]
pub(crate) struct DelegationIndex {
    edges: BTreeMap<String, DelegationEdge>,
}

impl DelegationIndex {
    /// Fold the durable log. A re-issued copy refreshes an edge's fields but
    /// never revives it: gone is sticky, because an `AgentId` never comes
    /// back once it is retired. Each event comes with its log timestamp.
    pub(crate) fn fold<'a>(&mut self, events: impl IntoIterator<Item = (u64, &'a EventEnvelope)>) {
        for (ts_ms, envelope) in events {
            match &envelope.data {
                EventData::AgentDelegated {
                    architect,
                    delegate,
                    via,
                    workspace,
                    worktree,
                    branch,
                    repo,
                    label,
                    reissued: _,
                } => {
                    let (state, since_ms) = self
                        .edges
                        .get(delegate)
                        .map_or((EdgeState::Live, ts_ms), |edge| (edge.state, edge.since_ms));
                    self.edges.insert(
                        delegate.clone(),
                        DelegationEdge {
                            architect: architect.clone(),
                            delegate: delegate.clone(),
                            via: *via,
                            workspace: workspace.clone(),
                            worktree: worktree.clone(),
                            branch: branch.clone(),
                            repo: repo.clone(),
                            label: label.clone(),
                            state,
                            since_ms,
                        },
                    );
                }
                EventData::AgentRemoved { agent_id, .. } => self.mark_gone(agent_id),
                _ => {}
            }
        }
    }

    pub(crate) fn insert(&mut self, edge: DelegationEdge) {
        self.edges.insert(edge.delegate.clone(), edge);
    }

    /// `AgentRemoved` does not close an edge, it moves it to gone.
    pub(crate) fn mark_gone(&mut self, delegate: &str) {
        if let Some(edge) = self.edges.get_mut(delegate) {
            edge.state = EdgeState::Gone;
        }
    }

    /// Decide which edges are still open, against the live agent records
    /// rather than only the logged removals, since a crash can lose the
    /// `AgentRemoved` event. A delegate missing from `live` is gone. A gone
    /// edge stays open while it may own a worktree and closes otherwise.
    ///
    /// "May own" is the conservative reading until worktree provenance lands
    /// (#968): the edge names a worktree and that checkout is still on disk.
    /// Returns the edges left open.
    pub(crate) fn reconcile(
        &mut self,
        live: &HashSet<String>,
        worktree_exists: &dyn Fn(&str) -> bool,
    ) -> Vec<&DelegationEdge> {
        self.edges.retain(|delegate, edge| {
            if !live.contains(delegate) {
                edge.state = EdgeState::Gone;
            }
            edge.state == EdgeState::Live || edge.worktree.as_deref().is_some_and(worktree_exists)
        });
        self.edges.values().collect()
    }

    /// Every open edge, ordered by delegate.
    pub(crate) fn edges(&self) -> impl Iterator<Item = &DelegationEdge> {
        self.edges.values()
    }

    /// Open edges whose architect is `architect`.
    #[cfg(test)]
    pub(crate) fn edges_of(&self, architect: &str) -> Vec<&DelegationEdge> {
        self.edges
            .values()
            .filter(|edge| edge.architect == architect)
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn get(&self, delegate: &str) -> Option<&DelegationEdge> {
        self.edges.get(delegate)
    }
}

/// What `agent.start` reports when ancestry placed the caller in no agent
/// pane. A detached spawner (`setsid`, a reparenting `nohup`, `ssh` from
/// another host) attests as the operator, and must learn that at start time
/// rather than by its delegate never showing up as its own.
pub(crate) const UNATTESTED_EDGE: &str = "none (unattested)";

impl App {
    /// Record the edge for a successful start, when the caller is an agent.
    /// Returns the report `agent.start` answers with.
    pub(super) fn record_delegation(
        &mut self,
        caller: &SpawnCaller,
        via: DelegationVia,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        label: Option<String>,
    ) -> String {
        let SpawnCaller::Agent {
            agent_id: architect,
            ..
        } = caller
        else {
            return UNATTESTED_EDGE.to_string();
        };
        let Some(delegate) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.terminal_id(pane_id))
            .and_then(|id| self.state.terminals.get(id))
            .map(|terminal| terminal.agent_id.to_string())
        else {
            return UNATTESTED_EDGE.to_string();
        };
        let ws = &self.state.workspaces[ws_idx];
        let space = ws.worktree_space_here();
        let edge = DelegationEdge {
            architect: architect.clone(),
            delegate,
            via,
            workspace: self.public_workspace_id(ws_idx),
            worktree: space
                .filter(|space| space.is_linked_worktree)
                .map(|space| space.checkout_path.display().to_string()),
            branch: ws.branch(),
            repo: space.map(|space| space.key.clone()),
            label: label
                .map(|label| label.trim().to_string())
                .filter(|label| !label.is_empty()),
            state: EdgeState::Live,
            since_ms: super::notifications::now_ms(),
        };
        self.emit_event(edge.event(false));
        self.delegations.insert(edge);
        format!("architect {architect}")
    }

    /// Once per server start: read the durable log once and fold the
    /// delegation edges, the goals (ADR-0031 §3) and the owner bindings
    /// (ADR-0035 §1) from it, re-issuing what is open so log rotation cannot
    /// drop it. The same read seeds the derived agent events (ADR-0028 §2,
    /// #1027) and announces `gone` for an agent no restored pane carries.
    ///
    /// Run from the server loop rather than from `App::new`, so a live
    /// handoff folds the log only after the exporting server has stopped
    /// writing to it.
    pub(crate) fn reissue_open_records(&mut self) {
        let restored = self.event_hub.persisted_events_after(0);
        // Read before the re-issues, so a rotation they cause re-issues again.
        self.record_rotations_seen = self.event_hub.rotations();
        self.reissue_open_delegations(&restored);
        self.reissue_open_goals(&restored);
        self.reissue_owner_bindings(&restored);
        self.begin_agent_phases(&restored);
    }

    /// Server loop: when the log rotated since the last look, re-issue the
    /// open goals and the owner bindings from memory (ADR-0031 §4). A
    /// rotation keeps the old active file as `.1`, so each record is on disk
    /// until its copy lands.
    pub(crate) fn reissue_records_after_rotation(&mut self) {
        let rotations = self.event_hub.rotations();
        if rotations == self.record_rotations_seen {
            return;
        }
        self.record_rotations_seen = rotations;
        self.emit_goal_reissues();
        self.emit_owner_reissues();
    }

    /// Fold the edges, decide openness against the live agent records and
    /// the checkouts on disk, and re-emit every open edge.
    fn reissue_open_delegations(&mut self, restored: &[(u64, u64, EventEnvelope)]) {
        let mut index = DelegationIndex::default();
        index.fold(
            restored
                .iter()
                .map(|(_, ts_ms, envelope)| (*ts_ms, envelope)),
        );
        let live: HashSet<String> = self
            .state
            .terminals
            .values()
            .map(|terminal| terminal.agent_id.to_string())
            .collect();
        let reissued: Vec<EventEnvelope> = index
            .reconcile(&live, &|path| std::path::Path::new(path).exists())
            .into_iter()
            .map(|edge| edge.event(true))
            .collect();
        for event in reissued {
            self.emit_event(event);
        }
        self.delegations = index;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(architect: &str, delegate: &str, worktree: Option<&str>) -> EventEnvelope {
        DelegationEdge {
            architect: architect.into(),
            delegate: delegate.into(),
            via: DelegationVia::Start,
            workspace: "w2".into(),
            worktree: worktree.map(str::to_string),
            branch: Some("feat/x".into()),
            repo: Some("/repo/.git".into()),
            label: Some("#614".into()),
            state: EdgeState::Live,
            since_ms: 0,
        }
        .event(false)
    }

    fn removed(agent: &str) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::AgentRemoved,
            data: EventData::AgentRemoved {
                agent_id: agent.into(),
                session: "s".into(),
                reason: "exited".into(),
            },
        }
    }

    fn stamped(events: &[EventEnvelope]) -> impl Iterator<Item = (u64, &EventEnvelope)> {
        events.iter().map(|envelope| (0, envelope))
    }

    fn live(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn the_fold_builds_an_architect_to_edges_index() {
        let mut index = DelegationIndex::default();
        index.fold(stamped(&[
            edge("arch", "d1", None),
            edge("arch", "d2", None),
        ]));
        let delegates: Vec<_> = index
            .edges_of("arch")
            .into_iter()
            .map(|edge| edge.delegate.as_str())
            .collect();
        assert_eq!(delegates, ["d1", "d2"]);
        assert_eq!(
            index.get("d1").map(|e| e.label.as_deref()),
            Some(Some("#614"))
        );
    }

    #[test]
    fn a_removal_moves_the_edge_to_gone_and_a_reissue_does_not_revive_it() {
        let mut index = DelegationIndex::default();
        let mut reissued = edge("arch", "d1", Some("/wt/d1"));
        if let EventData::AgentDelegated { reissued: flag, .. } = &mut reissued.data {
            *flag = true;
        }
        index.fold(stamped(&[
            edge("arch", "d1", Some("/wt/d1")),
            removed("d1"),
            reissued,
        ]));
        assert_eq!(index.get("d1").map(|e| e.state), Some(EdgeState::Gone));
    }

    #[test]
    fn a_gone_edge_stays_open_while_its_worktree_exists_and_closes_when_it_owns_nothing() {
        let mut index = DelegationIndex::default();
        index.fold(stamped(&[
            edge("arch", "kept", Some("/wt/kept")),
            removed("kept"),
            edge("arch", "bare", None),
            removed("bare"),
            edge("arch", "reaped", Some("/wt/reaped")),
            removed("reaped"),
        ]));
        let open: Vec<_> = index
            .reconcile(&live(&[]), &|path| path == "/wt/kept")
            .into_iter()
            .map(|edge| (edge.delegate.clone(), edge.state))
            .collect();
        assert_eq!(open, [("kept".to_string(), EdgeState::Gone)]);
    }

    /// A crash can lose the `AgentRemoved` event, so openness is decided by
    /// the live records: a delegate that is simply missing is gone.
    #[test]
    fn a_delegate_missing_from_the_live_records_is_gone_without_a_logged_removal() {
        let mut index = DelegationIndex::default();
        index.fold(stamped(&[
            edge("arch", "alive", None),
            edge("arch", "lost", None),
            edge("arch", "lost-kept", Some("/wt/lost-kept")),
        ]));
        let open: Vec<_> = index
            .reconcile(&live(&["alive"]), &|path| path == "/wt/lost-kept")
            .into_iter()
            .map(|edge| (edge.delegate.clone(), edge.state))
            .collect();
        assert_eq!(
            open,
            [
                ("alive".to_string(), EdgeState::Live),
                ("lost-kept".to_string(), EdgeState::Gone),
            ]
        );
    }
}

/// From the socket call of a caller attested by its process ancestry to the
/// event on the durable log (AGENTS.md: "Unit tests can be green while the
/// feature has never run"). The ancestry walk itself is `spawn_caller()`'s and
/// is driven for real: this process stands in as the pane's child.
#[cfg(test)]
pub(crate) mod handler_tests {
    use std::path::PathBuf;

    use crate::api::schema::{
        AgentStartParams, DelegationVia, EventData, Method, Request, ResponseResult,
        SuccessResponse,
    };
    use crate::api::EventHub;
    use crate::app::App;

    pub(crate) fn temp_log(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "flock-delegation-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ))
            .join("event-log.jsonl")
    }

    pub(crate) fn app_on(hub: EventHub) -> App {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&crate::config::Config::default(), true, None, rx, hub);
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("architect")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app
    }

    /// Make workspace 0's pane an agent this process is running inside.
    pub(crate) fn caller_is_an_agent(app: &mut App) -> String {
        let pane_id = app.state.workspaces[0]
            .focused_pane_id()
            .expect("workspace has a pane");
        let terminal_id = app.state.workspaces[0]
            .pane_state(pane_id)
            .expect("pane state")
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).expect("terminal");
        terminal.launch_argv = Some(vec!["claude".into()]);
        let agent_id = terminal.agent_id.to_string();
        app.test_pane_child_pids.insert(pane_id, std::process::id());
        app.current_api_peer_pid = Some(std::process::id());
        agent_id
    }

    pub(crate) fn start(app: &mut App, label: Option<&str>) -> (String, Option<String>) {
        let name = format!("worker-{}", app.state.terminals.len());
        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::AgentStart(AgentStartParams {
                dry_run: false,
                name,
                cwd: None,
                workspace_id: Some(app.public_workspace_id(0)),
                tab_id: None,
                split: None,
                active: false,
                here: false,
                focus: false,
                argv: vec![crate::test_support::live_program()],
                label: label.map(str::to_string),
                questions: None,
                unattended: false,
            }),
        });
        let success: SuccessResponse = serde_json::from_str(&response).expect(&response);
        let ResponseResult::AgentStarted { agent, edge, .. } = success.result else {
            panic!("expected agent_started: {response}");
        };
        (agent.agent_id, edge)
    }

    /// (architect, delegate, via, label, reissued) of every persisted edge.
    fn persisted_edges(
        hub: &EventHub,
    ) -> Vec<(String, String, DelegationVia, Option<String>, bool)> {
        hub.persisted_events_after(0)
            .into_iter()
            .filter_map(|(_, _, envelope)| match envelope.data {
                EventData::AgentDelegated {
                    architect,
                    delegate,
                    via,
                    label,
                    reissued,
                    ..
                } => Some((architect, delegate, via, label, reissued)),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn shutdown(mut app: App) {
        let runtimes: Vec<_> = app.terminal_runtimes.drain().collect();
        for (_terminal_id, runtime) in runtimes {
            runtime.shutdown();
        }
    }

    #[tokio::test]
    async fn an_agent_pane_starting_an_agent_records_exactly_one_edge_with_itself_as_architect() {
        let log = temp_log("start");
        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());
        let architect = caller_is_an_agent(&mut app);

        let (delegate, edge) = start(&mut app, Some("#614"));

        assert_eq!(edge, Some(format!("architect {architect}")));
        assert_eq!(
            persisted_edges(&hub),
            [(
                architect.clone(),
                delegate.clone(),
                DelegationVia::Start,
                Some("#614".to_string()),
                false
            )]
        );
        assert_eq!(app.delegations.edges_of(&architect).len(), 1);
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }

    /// A caller ancestry places in no agent pane is the operator, which is
    /// not an architect. It is told so rather than left to find out later.
    #[tokio::test]
    async fn an_operator_start_records_nothing_and_reports_unattested() {
        let log = temp_log("operator");
        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());

        let (_delegate, edge) = start(&mut app, None);

        assert_eq!(edge.as_deref(), Some(super::UNATTESTED_EDGE));
        assert!(persisted_edges(&hub).is_empty());
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }

    /// The fold a restart or a live handoff performs: the edge comes back
    /// from the log, and an open one is re-issued so rotation cannot drop it.
    /// A delegate absent from the live records is gone; with no worktree it
    /// owns nothing, so it closes and is not re-issued.
    #[tokio::test]
    async fn a_server_start_folds_the_log_and_reissues_only_open_edges() {
        let log = temp_log("restart");
        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());
        let architect = caller_is_an_agent(&mut app);
        let (kept, _) = start(&mut app, Some("kept"));
        let (lost, _) = start(&mut app, Some("lost"));
        shutdown(app);

        // The next server: the same log, and a session in which only `kept`
        // came back.
        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());
        let pane_id = app.state.workspaces[0]
            .focused_pane_id()
            .expect("workspace has a pane");
        let terminal_id = app.state.workspaces[0]
            .pane_state(pane_id)
            .expect("pane state")
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("terminal")
            .agent_id = crate::terminal::AgentId::from_persisted(kept.clone());

        app.reissue_open_records();

        let reissued: Vec<_> = persisted_edges(&hub)
            .into_iter()
            .filter(|edge| edge.4)
            .map(|edge| (edge.0, edge.1))
            .collect();
        assert_eq!(reissued, [(architect.clone(), kept.clone())]);
        assert!(app.delegations.get(&kept).is_some());
        assert!(
            app.delegations.get(&lost).is_none(),
            "a delegate missing from the live records, owning no worktree, closes"
        );
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }

    /// A removal moves the edge to gone in the running server's index too,
    /// whether or not the node writes an `AgentRemoved` event.
    #[tokio::test]
    async fn removing_a_delegate_moves_its_edge_to_gone() {
        let log = temp_log("removed");
        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());
        caller_is_an_agent(&mut app);
        let (delegate, _) = start(&mut app, None);

        let (ws_idx, pane_id) = (0..app.state.workspaces.len())
            .find_map(|ws_idx| {
                app.state.workspaces[ws_idx]
                    .tabs
                    .iter()
                    .flat_map(|tab| tab.layout.pane_ids())
                    .find(|pane| {
                        app.state.workspaces[ws_idx]
                            .terminal_id(*pane)
                            .and_then(|id| app.state.terminals.get(id))
                            .is_some_and(|t| t.agent_id.to_string() == delegate)
                    })
                    .map(|pane| (ws_idx, pane))
            })
            .expect("the delegate's pane");
        app.remove_agent_for_pane(
            ws_idx,
            pane_id,
            crate::app::agent_removal::RemovalEvent::Kill,
        );

        assert_eq!(
            app.delegations.get(&delegate).map(|edge| edge.state),
            Some(super::EdgeState::Gone)
        );
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }
}
