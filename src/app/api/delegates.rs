//! `delegate.list` (#960, ADR-0027 §2): the one read every delegate surface
//! projects. It answers from the edge index (#959) joined with what the
//! server has cached about each delegate: its agent record and the PR
//! poller's last answer for its branch. No git or `gh` call happens here, so
//! a sidebar can render it per frame (#805 §5).

use std::collections::{HashMap, HashSet};

use crate::api::schema::{
    AgentInfo, AgentStatus, DelegateListParams, DelegateRow, PrInfo, ResponseResult,
};
use crate::app::delegation::{DelegationEdge, EdgeState};
use crate::app::{App, SpawnCaller};
use crate::worktree::{PrState, PrStateInfo};

use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_delegate_list(
        &mut self,
        id: String,
        params: DelegateListParams,
    ) -> String {
        let architect = match params.architect.trim() {
            "all" => None,
            "self" | "" => match self.spawn_caller() {
                SpawnCaller::Agent { agent_id, .. }
                | SpawnCaller::Structured {
                    agent_id: Some(agent_id),
                    ..
                } => Some(agent_id),
                SpawnCaller::Operator { .. } | SpawnCaller::Structured { agent_id: None, .. } => {
                    return encode_error(
                        id,
                        "no_caller_pane",
                        "`self` is the agent whose pane runs the caller, and this caller \
                         runs in no agent pane, so it has no delegates of its own: use \
                         --all (architect \"all\") or --architect <agent id>",
                    )
                }
            },
            other => Some(other.to_string()),
        };
        let delegates = self.delegate_rows(
            architect.as_deref(),
            params.branch.as_deref(),
            params.repo.as_deref(),
        );
        encode_success(id, ResponseResult::DelegateList { delegates })
    }

    fn delegate_rows(
        &self,
        architect: Option<&str>,
        branch: Option<&str>,
        repo: Option<&str>,
    ) -> Vec<DelegateRow> {
        let local_agents = self.collect_agent_infos();
        let mut known: HashSet<String> = self
            .state
            .terminals
            .values()
            .map(|terminal| terminal.agent_id.to_string())
            .collect();
        // Architect presence includes the same cached gossip as the fleet directory.
        // Reuse the local snapshot; listing must not poll peers or observe it again.
        known.extend(
            self.collect_fleet_agents(&local_agents)
                .into_iter()
                .map(|agent| agent.agent_id),
        );
        let agents: HashMap<String, AgentInfo> = local_agents
            .into_iter()
            .map(|agent| (agent.agent_id.clone(), agent))
            .collect();
        let now = crate::app::notifications::now_ms();
        self.delegations
            .edges()
            .filter(|edge| architect.is_none_or(|architect| edge.architect == architect))
            .filter(|edge| branch.is_none_or(|branch| edge.branch.as_deref() == Some(branch)))
            .filter(|edge| repo.is_none_or(|repo| edge.repo.as_deref() == Some(repo)))
            .map(|edge| {
                let agent = agents.get(&edge.delegate);
                let pr = self.cached_pr(edge);
                let worktree_on_disk = edge
                    .worktree
                    .as_deref()
                    .is_some_and(|path| std::path::Path::new(path).exists());
                DelegateRow {
                    architect: edge.architect.clone(),
                    delegate: edge.delegate.clone(),
                    name: agent.and_then(|agent| agent.name.clone()),
                    via: edge.via,
                    state: edge_state(
                        edge,
                        agent.map(|agent| agent.agent_status),
                        known.contains(&edge.architect),
                        pr.map(|pr| pr.state),
                        worktree_on_disk,
                    )
                    .to_string(),
                    label: edge.label.clone(),
                    harness: agent.and_then(|agent| agent.agent.clone()),
                    agent_status: agent.map(|agent| agent.agent_status),
                    status_age_secs: agent.and_then(|agent| agent.status_age_secs),
                    edge_age_secs: now.saturating_sub(edge.since_ms) / 1000,
                    workspace_id: agent
                        .map_or_else(|| edge.workspace.clone(), |a| a.workspace_id.clone()),
                    pane_id: agent.map(|agent| agent.pane_id.clone()),
                    worktree: edge.worktree.clone(),
                    branch: edge.branch.clone(),
                    repo: edge.repo.clone(),
                    pr: pr.map(PrInfo::from_state),
                    questions: agent.and_then(|agent| agent.questions),
                }
            })
            .collect()
    }

    /// The poller's cached PR for the edge's branch, read off any workspace
    /// on that branch in the same repository.
    fn cached_pr(&self, edge: &DelegationEdge) -> Option<PrStateInfo> {
        let branch = edge.branch.as_deref()?;
        self.state
            .workspaces
            .iter()
            .filter(|ws| ws.branch().as_deref() == Some(branch))
            .filter(|ws| edge.repo.is_none() || ws.repo_group_key() == edge.repo.as_deref())
            .find_map(|ws| ws.pr_state())
    }
}

/// The edge state ADR-0027 §2 lists. A closed-unmerged PR outranks the rest,
/// batch-landed `landed-in-batch` PRs included, because that is what the
/// architect has to act on: such an edge never reaps (§5).
fn edge_state(
    edge: &DelegationEdge,
    delegate_status: Option<AgentStatus>,
    architect_known: bool,
    pr: Option<PrState>,
    worktree_on_disk: bool,
) -> &'static str {
    if pr == Some(PrState::Closed) {
        return "closed, not merged";
    }
    if edge.state == EdgeState::Gone || delegate_status.is_none() {
        return if worktree_on_disk {
            "gone, worktree retained"
        } else {
            "gone"
        };
    }
    if !architect_known {
        return "orphaned";
    }
    if delegate_status == Some(AgentStatus::Hibernated)
        && matches!(pr, Some(PrState::Open | PrState::Draft))
    {
        return "hibernated, awaiting merge";
    }
    "live"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::DelegationVia;

    fn edge(state: EdgeState) -> DelegationEdge {
        DelegationEdge {
            architect: "arch".into(),
            delegate: "d1".into(),
            via: DelegationVia::Start,
            workspace: "w2".into(),
            worktree: Some("/wt/d1".into()),
            branch: Some("feat/x".into()),
            repo: None,
            label: None,
            state,
            since_ms: 0,
        }
    }

    #[test]
    fn every_edge_state_the_adr_lists_is_reachable() {
        let live = edge(EdgeState::Live);
        let gone = edge(EdgeState::Gone);
        let working = Some(AgentStatus::Working);
        let hibernated = Some(AgentStatus::Hibernated);
        let cases = [
            (edge_state(&live, working, true, None, true), "live"),
            (
                edge_state(&live, hibernated, true, Some(PrState::Open), true),
                "hibernated, awaiting merge",
            ),
            (edge_state(&live, hibernated, true, None, true), "live"),
            (edge_state(&gone, None, true, None, false), "gone"),
            (
                edge_state(&gone, None, true, None, true),
                "gone, worktree retained",
            ),
            (edge_state(&live, None, true, None, false), "gone"),
            (edge_state(&live, working, false, None, true), "orphaned"),
            (
                edge_state(&gone, None, true, Some(PrState::Closed), true),
                "closed, not merged",
            ),
            (
                edge_state(&live, working, true, Some(PrState::Merged), true),
                "live",
            ),
        ];
        for (got, want) in cases {
            assert_eq!(got, want);
        }
    }
}

/// ADR-0027 §2's acceptance, driven through the socket verb with callers
/// attested by process ancestry, and with requests built by the CLI parser.
#[cfg(test)]
mod handler_tests {
    use crate::api::schema::{
        DelegateRow, DelegationVia, Method, Request, ResponseResult, SuccessResponse,
    };
    use crate::api::EventHub;
    use crate::app::delegation::handler_tests::{
        app_on, caller_is_an_agent, shutdown, start, temp_log,
    };
    use crate::app::delegation::{DelegationEdge, EdgeState};
    use crate::app::App;
    use crate::worktree::{PrState, PrStateInfo};

    fn list(app: &mut App, words: &[&str]) -> Result<Vec<DelegateRow>, String> {
        let words: Vec<String> = words.iter().map(|word| word.to_string()).collect();
        let (params, _) = crate::cli::delegate_list::parse_list_args(&words).expect("parses");
        let response = app.handle_api_request(Request {
            id: "req".into(),
            method: Method::DelegateList(params),
        });
        let Ok(success) = serde_json::from_str::<SuccessResponse>(&response) else {
            return Err(response);
        };
        let ResponseResult::DelegateList { delegates } = success.result else {
            panic!("expected delegate_list: {response}");
        };
        Ok(delegates)
    }

    fn delegates_of(rows: &[DelegateRow]) -> Vec<(String, String)> {
        let mut pairs: Vec<_> = rows
            .iter()
            .map(|row| (row.architect.clone(), row.delegate.clone()))
            .collect();
        pairs.sort();
        pairs
    }

    /// A second architect, in its own workspace, becomes the caller.
    fn caller_is_architect_b(app: &mut App) -> String {
        app.state
            .workspaces
            .push(crate::workspace::Workspace::test_new("architect-b"));
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[1].focused_pane_id().expect("pane");
        let terminal_id = app.state.workspaces[1]
            .pane_state(pane_id)
            .expect("pane state")
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).expect("terminal");
        terminal.launch_argv = Some(vec!["codex".into()]);
        let agent_id = terminal.agent_id.to_string();
        crate::test_support::attest_pane_caller(app, pane_id);
        agent_id
    }

    #[tokio::test]
    async fn mine_is_exactly_the_callers_edges_and_all_shows_both_architects() {
        let log = temp_log("list-mine");
        let mut app = app_on(EventHub::with_persistence(log.clone()));
        let a = caller_is_an_agent(&mut app);
        let a_delegates: Vec<String> = (0..3).map(|_| start(&mut app, None).0).collect();
        let b = caller_is_architect_b(&mut app);
        let (b_delegate, _) = start(&mut app, Some("#2"));

        let b_rows = list(&mut app, &["--mine"]).expect("b lists");
        assert_eq!(delegates_of(&b_rows), [(b.clone(), b_delegate.clone())]);
        assert_eq!(b_rows[0].state, "live");
        assert_eq!(b_rows[0].label.as_deref(), Some("#2"));
        assert!(b_rows[0].agent_status.is_some() && b_rows[0].pane_id.is_some());

        let a_rows = list(&mut app, &["--architect", &a]).expect("a by id");
        let mut want: Vec<_> = a_delegates.iter().map(|d| (a.clone(), d.clone())).collect();
        want.sort();
        assert_eq!(delegates_of(&a_rows), want);

        // A's own `--mine`, run from A's pane, is exactly A's three.
        assert_eq!(caller_is_an_agent(&mut app), a);
        let a_mine = list(&mut app, &["--mine"]).expect("a lists");
        assert_eq!(delegates_of(&a_mine), want);
        app.current_api_peer_pid = None;

        let all = list(&mut app, &["--all"]).expect("all");
        let mut want_all = want.clone();
        want_all.push((b.clone(), b_delegate));
        want_all.sort();
        assert_eq!(delegates_of(&all), want_all);
        let tree = crate::cli::delegate_list::render(&all);
        assert!(tree.contains(&format!("architect {a}\n")), "{tree}");
        assert!(tree.contains(&format!("architect {b}\n")), "{tree}");
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }

    fn edge(delegate: &str, branch: &str, state: EdgeState) -> DelegationEdge {
        DelegationEdge {
            architect: "agent_lead".into(),
            delegate: delegate.into(),
            via: DelegationVia::Start,
            workspace: "w9".into(),
            worktree: None,
            branch: Some(branch.into()),
            repo: None,
            label: None,
            state,
            since_ms: 0,
        }
    }

    fn workspace_on(app: &mut App, branch: &str, pr: Option<PrStateInfo>) {
        let mut ws = crate::workspace::Workspace::test_new(branch);
        ws.cached_git_branch = Some(branch.into());
        ws.pr_state = pr;
        app.state.workspaces.push(ws);
    }

    #[tokio::test]
    async fn branch_finds_the_owning_edge_and_pr_and_gone_states_come_from_cached_state() {
        let log = temp_log("list-states");
        let mut app = app_on(EventHub::with_persistence(log.clone()));
        workspace_on(
            &mut app,
            "feat/open",
            Some(PrStateInfo {
                state: PrState::Open,
                number: 624,
            }),
        );
        workspace_on(
            &mut app,
            "feat/batched",
            Some(PrStateInfo {
                state: PrState::Closed,
                number: 625,
            }),
        );
        app.delegations
            .insert(edge("agent_open", "feat/open", EdgeState::Live));
        app.delegations
            .insert(edge("agent_batched", "feat/batched", EdgeState::Live));
        app.delegations
            .insert(edge("agent_removed", "feat/removed", EdgeState::Live));
        app.delegations.mark_gone("agent_removed");
        // A workspace known only by its cached git space, with no worktree
        // membership, still carries the PR for an edge that names its repo.
        let mut space_only = crate::workspace::Workspace::test_new("space-only");
        space_only.cached_git_branch = Some("feat/space".into());
        space_only.cached_git_space = Some(crate::workspace::GitSpaceMetadata {
            key: "/repo/.git".into(),
            checkout_key: "/repo".into(),
            label: "repo".into(),
            repo_root: "/repo".into(),
            is_linked_worktree: false,
            project_key: "dir:repo".into(),
        });
        space_only.pr_state = Some(PrStateInfo {
            state: PrState::Merged,
            number: 626,
        });
        app.state.workspaces.push(space_only);
        let mut in_repo = edge("agent_space", "feat/space", EdgeState::Live);
        in_repo.repo = Some("/repo/.git".into());
        app.delegations.insert(in_repo);

        let owner = list(&mut app, &["--all", "--branch", "feat/open"]).expect("by branch");
        assert_eq!(owner.len(), 1);
        assert_eq!(owner[0].delegate, "agent_open");
        let pr = owner[0].pr.as_ref().expect("the poller's PR");
        assert_eq!((pr.number, pr.state.as_str()), (624, "open"));

        let rows = list(&mut app, &["--architect", "agent_lead"]).expect("by id");
        let state = |delegate: &str| {
            rows.iter()
                .find(|row| row.delegate == delegate)
                .map(|row| row.state.clone())
        };
        assert_eq!(
            state("agent_batched").as_deref(),
            Some("closed, not merged")
        );
        assert_eq!(state("agent_removed").as_deref(), Some("gone"));
        let space_pr = rows
            .iter()
            .find(|row| row.delegate == "agent_space")
            .and_then(|row| row.pr.clone())
            .expect("the PR of a workspace matched by its git space");
        assert_eq!((space_pr.number, space_pr.state.as_str()), (626, "merged"));

        // The operator has no pane, so it has no `self`.
        let refused = list(&mut app, &["--mine"]).expect_err("operator self");
        assert!(
            refused.contains("no_caller_pane") && refused.contains("--all"),
            "{refused}"
        );
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }

    const REMOTE_ARCHITECT: &str = "agent_architect.example_1";

    fn remote_architect_app() -> (App, String) {
        let mut app = app_on(EventHub::default());
        caller_is_an_agent(&mut app);
        let (delegate, _) = start(&mut app, None);
        let original = app
            .delegations
            .edges()
            .find(|edge| edge.delegate == delegate)
            .unwrap()
            .clone();
        // Replay the durable edge whose architect is on another node.
        let event = crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::AgentDelegated,
            data: crate::api::schema::EventData::AgentDelegated {
                architect: REMOTE_ARCHITECT.into(),
                delegate: delegate.clone(),
                via: original.via,
                workspace: original.workspace,
                worktree: None,
                branch: None,
                repo: None,
                label: None,
                reissued: true,
            },
        };
        app.delegations.fold([(0, &event)]);
        app.state
            .peer_summaries
            .push(crate::peers::PeerSummaryState::new(
                &crate::config::PeerConfig {
                    name: "hub.example".into(),
                    ..Default::default()
                },
            ));
        app.current_api_peer_pid = None;
        (app, delegate)
    }

    fn publish_remote_architect(app: &mut App, relayed: bool, present: bool) {
        let workspace: crate::api::schema::PeerWorkspaceSummary =
            serde_json::from_value(serde_json::json!({
                "workspace":"fixture", "status":"idle", "agents":[{
                    "agent_id":REMOTE_ARCHITECT, "pane_id":"w1:p1", "agent":"claude", "status":"idle"
                }]
            }))
            .unwrap();
        let workspaces = if present { vec![workspace] } else { Vec::new() };
        let relay: crate::api::schema::RelayedFleetPeer = serde_json::from_value(serde_json::json!({
            "name":"architect.example", "ssh_target":"architect.example", "host":"architect.example",
            "origin":"hub.example", "workspaces":workspaces, "age_secs":0, "origin_last_ok_secs":0
        })).unwrap();
        app.handle_internal_event(crate::events::AppEvent::PeerSummaryFetched(
            crate::peers::PeerSummaryFetch {
                peer: "hub.example".into(),
                stream_error: None,
                result: Ok(crate::peers::PeerSummaryPayload {
                    node_id: None,
                    outbound_pending: false,
                    host: if relayed {
                        "hub.example"
                    } else {
                        "architect.example"
                    }
                    .into(),
                    version: Some("fixture".into()),
                    protocol: Some(crate::protocol::PROTOCOL_VERSION),
                    icon: None,
                    system: None,
                    latency_ms: 0,
                    workspaces: if relayed { Vec::new() } else { workspaces },
                    relayed_fleet: if relayed { vec![relay] } else { Vec::new() },
                }),
            },
        ));
    }

    #[tokio::test]
    async fn direct_architect_presence_survives_unreachability_until_summary_removal() {
        let (mut app, delegate) = remote_architect_app();
        assert_eq!(
            list(&mut app, &["--architect", REMOTE_ARCHITECT]).unwrap()[0].state,
            "orphaned"
        );
        publish_remote_architect(&mut app, false, true);
        let locals = app.collect_agent_infos();
        assert!(!locals
            .iter()
            .any(|agent| agent.agent_id == REMOTE_ARCHITECT));
        assert!(app
            .collect_fleet_agents(&locals)
            .iter()
            .any(|agent| agent.agent_id == REMOTE_ARCHITECT && !agent.local));
        let rows = list(&mut app, &["--architect", REMOTE_ARCHITECT]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].delegate, delegate);
        assert_eq!(rows[0].state, "live", "a gossiped architect is present");
        let rendered = crate::cli::delegate_list::render(&rows);
        assert!(!rendered.contains("orphaned"), "{rendered}");
        app.handle_internal_event(crate::events::AppEvent::PeerSummaryFetched(
            crate::peers::PeerSummaryFetch {
                peer: "hub.example".into(),
                stream_error: None,
                result: Err("fixture unavailable".into()),
            },
        ));
        assert_eq!(
            list(&mut app, &["--all"]).unwrap()[0].state,
            "live",
            "unreachable is not removed"
        );
        // Direct summaries have no age-based eviction, even past the relay TTL.
        let ttl = app.state.config.gossip.stale_after().as_secs()
            * crate::app::state::RELAYED_ENTRY_TTL_STALE_MULTIPLE;
        app.state.peer_summaries[0].last_ok =
            std::time::Instant::now().checked_sub(std::time::Duration::from_secs(ttl + 1));
        assert!(app.state.peer_summaries[0].last_ok.is_some());
        app.expire_relayed_entries();
        assert_eq!(list(&mut app, &["--all"]).unwrap()[0].state, "live");
        publish_remote_architect(&mut app, false, false);
        assert_eq!(list(&mut app, &["--all"]).unwrap()[0].state, "orphaned");
        assert!(
            crate::cli::delegate_list::render(&list(&mut app, &["--all"]).unwrap())
                .contains("orphaned")
        );
        shutdown(app);
    }

    #[tokio::test]
    async fn relayed_architect_presence_expires_without_reviving_gone_delegates() {
        let (mut app, delegate) = remote_architect_app();
        publish_remote_architect(&mut app, true, true);
        assert!(
            !app.state.relayed_fleet_cache.is_empty(),
            "real gossip merge cached the relay"
        );
        assert_eq!(list(&mut app, &["--all"]).unwrap()[0].state, "live");
        let ttl = app.state.config.gossip.stale_after().as_secs()
            * crate::app::state::RELAYED_ENTRY_TTL_STALE_MULTIPLE;
        // Advance local dwell on the real gossiped row, without waiting or clearing it.
        let ingested_at = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(ttl + 1))
            .expect("fixture clock can represent the relay TTL");
        for entry in app.state.relayed_fleet_cache.values_mut() {
            entry.peer.ingested_at = Some(ingested_at);
        }
        app.expire_relayed_entries();
        assert!(
            app.state.relayed_fleet_cache.is_empty(),
            "expiry tick evicts the relay"
        );
        assert_eq!(list(&mut app, &["--all"]).unwrap()[0].state, "orphaned");
        publish_remote_architect(&mut app, true, true);
        app.delegations.mark_gone(&delegate);
        assert_eq!(list(&mut app, &["--all"]).unwrap()[0].state, "gone");
        shutdown(app);
    }
}
