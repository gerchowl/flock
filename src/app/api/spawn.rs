//! `agent.spawn` — agent-initiated spawn of a fresh agent (#329, ADR-0014).
//!
//! The narrowed sibling of `agent.start`. Where that verb takes raw argv and
//! any cwd, this one takes a closed [`AgentKind`], a prompt, and a location
//! naming something flock already tracks. The difference is not politeness:
//! `agent.start`'s shape means a constraint applied by one caller leaves the
//! escape hatch open in the TYPE for every future caller.

use crate::api::schema::{AgentSpawnParams, ResponseResult, SpawnLocation};
use crate::app::App;
use crate::spawn::{admit, AgentKind, SpawnCensus, SpawnRefusal};

use super::responses::{encode_error, encode_error_with_data, encode_success};

/// Cap on the opening turn. Bounded so a caller cannot push an unbounded
/// body through the socket; generous enough for a real dispatch brief.
const MAX_PROMPT_BYTES: usize = 16 * 1024;

impl App {
    pub(super) fn handle_agent_spawn(&mut self, id: String, params: AgentSpawnParams) -> String {
        // Node-level gates first: whether this node offers the verb at all,
        // and whether the fleet is paused, do not depend on WHO is asking.
        // Resolving the caller first made a disabled node report
        // `spawn_caller_unknown`, which sends an operator debugging identity
        // when the real answer is "you never turned this on".
        let fleet = self.state.config.fleet;
        if !fleet.agent_spawn_enabled {
            let refusal = SpawnRefusal::NotEnabled;
            return encode_error_with_data(
                id,
                refusal.code(),
                refusal.message(),
                refusal_data(&refusal),
            );
        }
        if self.fleet_pause.paused {
            let refusal = SpawnRefusal::FleetPaused;
            return encode_error_with_data(
                id,
                refusal.code(),
                refusal.message(),
                refusal_data(&refusal),
            );
        }

        let Some(kind) = AgentKind::parse(params.agent.trim()) else {
            return encode_error_with_data(
                id,
                "unknown_agent_kind",
                format!("unknown agent kind {:?}", params.agent),
                serde_json::json!({
                    "refusal": "unknown_agent_kind",
                    "retryable": false,
                    "requested": params.agent,
                    "supported": AgentKind::supported(),
                }),
            );
        };

        let prompt = params.prompt.trim();
        if prompt.is_empty() {
            return encode_error(id, "invalid_request", "prompt is required");
        }
        if prompt.len() > MAX_PROMPT_BYTES {
            return encode_error(
                id,
                "invalid_request",
                format!("prompt exceeds {MAX_PROMPT_BYTES} bytes"),
            );
        }

        // Who is asking. Process ancestry of the API peer is the only
        // evidence — a caller-supplied identity would be a claim, and depth
        // is exactly the thing a runaway caller would want to lie about.
        //
        // Fail CLOSED when ancestry attests nothing: without a caller we
        // cannot bound depth or fanout, and an unbounded spawn verb is the
        // thing this whole path exists to prevent.
        let caller = self.parse_pane_id_or_peer("", self.current_api_peer_pid);
        let Some((caller_ws, caller_pane)) = caller else {
            return encode_error_with_data(
                id,
                "spawn_caller_unknown",
                "agent.spawn could not attest which agent is calling; refusing to spawn unbounded",
                serde_json::json!({ "refusal": "spawn_caller_unknown", "retryable": false }),
            );
        };

        // The ceiling. Depth and fanout come from state stamped on terminals
        // at spawn, so this never walks the durable event log.
        let census = self.spawn_census(caller_ws, caller_pane);
        if let Err(refusal) = admit(&fleet, census, self.fleet_pause.paused) {
            return encode_error_with_data(
                id,
                refusal.code(),
                refusal.message(),
                refusal_data(&refusal),
            );
        }

        let Some(target_ws) = self.resolve_spawn_location(&params.location) else {
            return encode_error_with_data(
                id,
                "spawn_location_unresolved",
                "the requested location is not a checkout this server has open",
                serde_json::json!({
                    "refusal": "spawn_location_unresolved",
                    "retryable": false,
                }),
            );
        };
        let Some(cwd) = self
            .state
            .workspaces
            .get(target_ws)
            .map(|ws| ws.identity_cwd.clone())
        else {
            return encode_error(id, "internal_error", "resolved workspace has no cwd");
        };

        let caller_agent_id = self
            .agent_id_for_pane(caller_ws, caller_pane)
            .unwrap_or_default();
        let run_id = crate::app::worktrees::generated_fork_run_id();
        self.install_run_trailer_if_enabled(&cwd);
        // Guard, not a bare set: a spawn failure below must disarm the id, or
        // the next unrelated pane inherits it and `flk revert-run` reverts
        // that pane's work instead.
        let _run_id_guard = crate::integration::set_pending_run_id(run_id.clone());
        // #329 / ADR-0014 §3: the child is asked for by an AGENT, so it does
        // not inherit the operator's ambient credentials. For an
        // operator-started pane inheriting them is correct — it is their
        // shell — which is exactly why this is armed here and not globally.
        crate::integration::set_pending_credential_scrub();

        let (rows, cols) = self.state.estimate_pane_size();
        let argv = kind.argv(prompt);
        let spawned = self.spawn_agent_workspace(cwd, rows, cols, &argv, params.focus);
        let (ws_idx, pane_id) = match spawned {
            Ok((ws_idx, _, pane_id)) => (ws_idx, pane_id),
            Err(err) => {
                let body = self.agent_start_error_body(err);
                return encode_error_with_data(
                    id,
                    "spawn_failed",
                    body.message,
                    serde_json::json!({ "refusal": "spawn_failed", "retryable": false }),
                );
            }
        };

        // Stamp the child's lineage BEFORE anything can read it. The run id
        // is what its commits will carry; depth and parent are what the
        // ceiling reads on the child's own next spawn attempt.
        self.record_spawn_run_id(ws_idx, pane_id, &run_id);
        self.record_spawn_lineage(ws_idx, pane_id, census.child_depth, &caller_agent_id);

        if let Some(name) = params
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            self.set_spawned_agent_name(ws_idx, pane_id, name);
        }

        let Some(agent) = self.agent_info(ws_idx, pane_id) else {
            return encode_error(id, "internal_error", "spawned pane is not an agent");
        };
        encode_success(id, ResponseResult::AgentSpawned { run_id, agent })
    }

    /// Live counts the ceiling needs, read from terminal state.
    fn spawn_census(&self, caller_ws: usize, caller_pane: crate::layout::PaneId) -> SpawnCensus {
        let caller_terminal = self
            .state
            .workspaces
            .get(caller_ws)
            .and_then(|ws| ws.pane_state(caller_pane))
            .and_then(|pane| self.state.terminals.get(&pane.attached_terminal_id));
        let caller_depth = caller_terminal.map_or(0, |t| t.spawn_depth);
        let caller_agent_id = caller_terminal.map(|t| t.agent_id.to_string());

        SpawnCensus {
            // Agent-STARTED agents only. An operator's own panes are not
            // counted: the cap bounds what agents do, and counting the
            // operator's work against it would make a busy human shrink the
            // fleet's headroom for no safety gain.
            live_agent_started: self
                .state
                .terminals
                .values()
                .filter(|t| t.spawned_by.is_some())
                .count(),
            parent_live_children: caller_agent_id
                .as_deref()
                .map(|parent| {
                    self.state
                        .terminals
                        .values()
                        .filter(|t| t.spawned_by.as_deref() == Some(parent))
                        .count()
                })
                .unwrap_or(0),
            child_depth: caller_depth.saturating_add(1),
        }
    }

    fn record_spawn_lineage(
        &mut self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        depth: u32,
        parent_agent_id: &str,
    ) {
        let terminal_id = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.pane_state(pane_id))
            .map(|pane| pane.attached_terminal_id.clone());
        if let Some(terminal) = terminal_id.and_then(|tid| self.state.terminals.get_mut(&tid)) {
            terminal.spawn_depth = depth;
            terminal.spawned_by = Some(parent_agent_id.to_string());
        }
    }

    fn agent_id_for_pane(&self, ws_idx: usize, pane_id: crate::layout::PaneId) -> Option<String> {
        self.state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.pane_state(pane_id))
            .and_then(|pane| self.state.terminals.get(&pane.attached_terminal_id))
            .map(|t| t.agent_id.to_string())
    }

    fn set_spawned_agent_name(
        &mut self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        name: &str,
    ) {
        let terminal_id = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.pane_state(pane_id))
            .map(|pane| pane.attached_terminal_id.clone());
        if let Some(terminal) = terminal_id.and_then(|tid| self.state.terminals.get_mut(&tid)) {
            terminal.set_agent_name(name.to_string());
        }
    }

    /// Resolve a [`SpawnLocation`] to an OPEN workspace index.
    ///
    /// Every variant names something this server already tracks — that is the
    /// whole reason the location is a union rather than a path string.
    fn resolve_spawn_location(&self, location: &SpawnLocation) -> Option<usize> {
        match location {
            SpawnLocation::WorktreePath { path } => {
                let canonical = crate::worktree::canonical_or_original(std::path::Path::new(path));
                self.open_workspace_idx_for_checkout(&canonical)
            }
            SpawnLocation::WorkspaceId { workspace_id } => self.parse_workspace_id(workspace_id),
            // Creating a worktree from inside the spawn path means owning the
            // whole `git worktree add` + membership-stamping flow that
            // `agent.fork` runs on a thread. Refused explicitly rather than
            // half-done: a caller can create the checkout first and spawn
            // into it by path.
            SpawnLocation::NewBranch { .. } => None,
        }
    }
}

fn refusal_data(refusal: &SpawnRefusal) -> serde_json::Value {
    let mut data = serde_json::json!({
        "refusal": refusal.code(),
        "retryable": refusal.retryable(),
    });
    let map = data.as_object_mut().expect("json object");
    match refusal {
        SpawnRefusal::AtCapacity { current, limit } | SpawnRefusal::AtFanout { current, limit } => {
            map.insert("current".into(), (*current).into());
            map.insert("limit".into(), (*limit).into());
        }
        SpawnRefusal::AtDepth { depth, limit } => {
            map.insert("depth".into(), (*depth).into());
            map.insert("limit".into(), (*limit).into());
        }
        SpawnRefusal::FleetPaused | SpawnRefusal::NotEnabled => {}
    }
    data
}

#[cfg(test)]
mod tests {
    use crate::api::schema::{AgentSpawnParams, Method, Request, SpawnLocation};

    fn spawn_request(agent: &str) -> Request {
        Request {
            id: "req".into(),
            method: Method::AgentSpawn(AgentSpawnParams {
                agent: agent.into(),
                prompt: "review #42 against the ADRs".into(),
                location: SpawnLocation::WorktreePath {
                    path: "/nonexistent/checkout".into(),
                },
                name: None,
                focus: false,
            }),
        }
    }

    fn error_of(raw: &str) -> serde_json::Value {
        serde_json::from_str::<serde_json::Value>(raw).expect("json")["error"].clone()
    }

    fn test_app() -> crate::app::App {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        crate::app::App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        )
    }

    /// Off by default. Exposing a spawn verb to agents is the same
    /// "don't autopilot until the operator opts in" surface `[checks.reap]`
    /// models, and it ships the same way.
    #[tokio::test]
    async fn agent_spawn_is_refused_until_an_operator_enables_it() {
        let mut app = test_app();
        let error = error_of(&app.handle_api_request(spawn_request("claude")));
        assert_eq!(error["code"], "agent_spawn_disabled");
        assert_eq!(error["data"]["refusal"], "agent_spawn_disabled");
        assert_eq!(
            error["data"]["retryable"], false,
            "a disabled node will not become enabled by polling"
        );
    }

    /// An unknown kind must not fall through to anything, and the refusal
    /// carries the supported set so a caller learns it without a docs
    /// round-trip.
    #[tokio::test]
    async fn an_unknown_agent_kind_is_refused_with_the_supported_set() {
        let mut app = test_app();
        app.state.config.fleet.agent_spawn_enabled = true;
        let error = error_of(&app.handle_api_request(spawn_request("bash")));
        assert_eq!(error["code"], "unknown_agent_kind");
        assert_eq!(error["data"]["retryable"], false);
        assert_eq!(
            error["data"]["supported"],
            serde_json::json!(["claude"]),
            "the refusal must name what IS accepted"
        );
    }

    /// Without an attested caller the ceiling cannot bound depth or fanout,
    /// and an unbounded spawn verb is the thing this path exists to prevent.
    /// So it fails CLOSED rather than assuming depth 0.
    #[tokio::test]
    async fn a_caller_it_cannot_attest_is_refused_rather_than_assumed_to_be_root() {
        let mut app = test_app();
        app.state.config.fleet.agent_spawn_enabled = true;
        // No API peer pid is set, so ancestry attests nothing.
        let error = error_of(&app.handle_api_request(spawn_request("claude")));
        assert_eq!(error["code"], "spawn_caller_unknown");
        assert_eq!(error["data"]["retryable"], false);
    }

    /// Pause deliberately exempts human keystrokes. An agent is not a human,
    /// and a paused fleet that still spawns agents is not paused. Pause is
    /// checked BEFORE the caller is resolved, so it holds even for a caller
    /// flock cannot attest.
    #[tokio::test]
    async fn a_paused_fleet_refuses_before_it_does_any_spawn_work() {
        let mut app = test_app();
        app.state.config.fleet.agent_spawn_enabled = true;
        app.fleet_pause.paused = true;
        let before = app.state.workspaces.len();
        let error = error_of(&app.handle_api_request(spawn_request("claude")));
        assert_eq!(error["code"], "fleet_paused");
        assert_eq!(
            error["data"]["retryable"], true,
            "an operator can resume; backing off is the right response"
        );
        assert_eq!(
            app.state.workspaces.len(),
            before,
            "a refused spawn must create nothing"
        );
    }
}
