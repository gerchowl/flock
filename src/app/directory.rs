//! The fleet directory: resolve an [`AgentId`](crate::terminal::AgentId) to
//! where that agent currently lives.
//!
//! ADR-0008 separates a name from a location. `AgentId` is the name — stable
//! across restarts, pane moves and workspace renames. This module answers the
//! other half: given a name, which host and pane is it on *right now*.
//!
//! ONE resolver, deliberately. The recurring defect in this codebase is a
//! decision implemented twice and drifting (#124, #197, #199–#210); addressing
//! is exactly the kind of thing that grows a second implementation the moment
//! a second caller needs it. Message routing, targeting and lineage all come
//! here.

use crate::app::App;

/// Where an agent is, at the moment of asking.
///
/// Every field except the id is expected to change. Callers that persist any
/// of it are storing a routing snapshot, not an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentLocation {
    pub(crate) agent_id: String,
    /// Short host name of the server the agent lives on.
    pub(crate) host: String,
    /// Public pane id **on that host**. Meaningless anywhere else.
    pub(crate) pane_id: String,
    /// `true` when the agent is on this server — i.e. reachable without the
    /// peer channel.
    pub(crate) local: bool,
    /// How to REACH that host: the `[[peers]]` config name of the entry that
    /// told us about this agent. `None` when local.
    ///
    /// Separate from `host` on purpose. `host` is what the machine calls
    /// itself (`vm-dev`); the route is what THIS server knows it as
    /// (`anvil`). They routinely differ — a live cross-host send failed with
    /// "not in this server's [[peers]]" because the relay looked up a peer
    /// named after the reported hostname. Resolution knows which peer the
    /// answer came from, so it should hand that back rather than make the
    /// caller re-derive it.
    pub(crate) route: Option<String>,
}

impl App {
    /// Resolve an agent id to its current location, local first.
    ///
    /// Local wins because it is authoritative and live: a peer summary is a
    /// poll snapshot that can be seconds stale, and an agent on this server is
    /// never better described by someone else's cache of it.
    pub(crate) fn locate_agent(&self, agent_id: &str) -> Option<AgentLocation> {
        self.locate_agent_locally(agent_id)
            .or_else(|| self.locate_agent_in_fleet(agent_id))
    }

    fn locate_agent_locally(&self, agent_id: &str) -> Option<AgentLocation> {
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            for tab in &ws.tabs {
                for pane_id in tab.panes.keys() {
                    let terminal = ws
                        .pane_state(*pane_id)
                        .and_then(|pane| self.state.terminals.get(&pane.attached_terminal_id))?;
                    if terminal.agent_id.to_string() != agent_id {
                        continue;
                    }
                    return Some(AgentLocation {
                        agent_id: agent_id.to_string(),
                        host: crate::app::short_host_name(),
                        pane_id: self.state.public_pane_id(ws_idx, *pane_id)?,
                        local: true,
                        route: None,
                    });
                }
            }
        }
        None
    }

    /// Search the gossiped peer summaries. Covers both directly-polled peers
    /// and one-hop relayed entries, so an agent two machines away is findable
    /// as long as its host reported to someone we talk to.
    fn locate_agent_in_fleet(&self, agent_id: &str) -> Option<AgentLocation> {
        // Directly-polled peers first, then one-hop relayed entries, so an
        // agent two machines away is findable as long as its host reported to
        // someone we talk to. Plain loops rather than chained iterators: the
        // set is small and the borrow shape stays obvious.
        for peer in &self.state.peer_summaries {
            let host = peer.host.clone().unwrap_or_else(|| peer.peer.clone());
            let route = Some(peer.peer.clone());
            for ws in &peer.workspaces {
                for agent in &ws.agents {
                    if agent.agent_id == agent_id {
                        return Some(AgentLocation {
                            agent_id: agent.agent_id.clone(),
                            host,
                            pane_id: agent.pane_id.clone(),
                            local: false,
                            route,
                        });
                    }
                }
            }
        }
        for peer in self.state.relayed_fleet_cache.values() {
            let host = peer.host.clone().unwrap_or_else(|| peer.name.clone());
            // A relayed entry is reachable only via the origin that relayed
            // it; we have no direct edge, so there is no route of our own.
            let route: Option<String> = None;
            for ws in &peer.workspaces {
                for agent in &ws.agents {
                    if agent.agent_id == agent_id {
                        return Some(AgentLocation {
                            agent_id: agent.agent_id.clone(),
                            host,
                            pane_id: agent.pane_id.clone(),
                            local: false,
                            route,
                        });
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use crate::api::schema::{AgentStatus, PeerAgentSummary, PeerWorkspaceSummary};
    use crate::app::App;

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    fn peer_with_agent(peer: &str, host: &str, agent_id: &str) -> crate::peers::PeerSummaryState {
        let mut state = crate::peers::PeerSummaryState::new(&crate::config::PeerConfig {
            name: peer.into(),
            ..Default::default()
        });
        state.host = Some(host.into());
        state.workspaces = vec![PeerWorkspaceSummary {
            id: "w1".into(),
            workspace: "remote".into(),
            project_key: None,
            project_label: None,
            branch: None,
            is_linked_worktree: false,
            agent: Some("cc".into()),
            status: AgentStatus::Idle,
            status_age_secs: None,
            activity: None,
            agents: vec![PeerAgentSummary {
                agent_id: agent_id.into(),
                pane_id: "w1:p1".into(),
                agent: Some("cc".into()),
                status: AgentStatus::Idle,
            }],
        }];
        state
    }

    #[test]
    fn a_local_agent_resolves_to_this_host() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("main")];
        app.state.ensure_test_terminals();

        let pane_id = app.state.workspaces[0].focused_pane_id().expect("pane");
        let terminal_id = app.state.workspaces[0]
            .pane_state(pane_id)
            .expect("pane state")
            .attached_terminal_id
            .clone();
        let agent_id = app.state.terminals[&terminal_id].agent_id.to_string();

        let found = app.locate_agent(&agent_id).expect("local agent resolves");
        assert!(found.local);
        assert_eq!(found.host, crate::app::short_host_name());
        assert_eq!(found.agent_id, agent_id);
    }

    #[test]
    fn a_remote_agent_resolves_through_the_gossiped_summary() {
        // The whole point of the directory: an identity minted on another
        // machine is findable here, with the host needed to reach it.
        let mut app = test_app();
        app.state.peer_summaries = vec![peer_with_agent("anvil", "anvil-dev", "agent_anvil-dev_1")];

        let found = app
            .locate_agent("agent_anvil-dev_1")
            .expect("remote agent resolves");
        assert!(!found.local);
        assert_eq!(found.host, "anvil-dev");
        assert_eq!(found.pane_id, "w1:p1");
    }

    #[test]
    fn a_remote_location_carries_the_route_not_just_the_hostname() {
        // Found live: a peer configured as `anvil` reports its hostname as
        // `vm-dev`, so a relay that looked up `[[peers]]` by the REPORTED host
        // found nothing and refused a message it could actually deliver. The
        // directory knows which peer entry answered — it has to hand that back.
        let mut app = test_app();
        app.state.peer_summaries = vec![peer_with_agent("anvil", "vm-dev", "agent_vm-dev_1")];

        let found = app.locate_agent("agent_vm-dev_1").expect("resolves");
        assert_eq!(found.host, "vm-dev", "where the agent is");
        assert_eq!(
            found.route.as_deref(),
            Some("anvil"),
            "how THIS server reaches it — the [[peers]] name, not the hostname"
        );
    }

    #[test]
    fn a_local_location_needs_no_route() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("main")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].focused_pane_id().expect("pane");
        let terminal_id = app.state.workspaces[0]
            .pane_state(pane_id)
            .expect("pane state")
            .attached_terminal_id
            .clone();
        let agent_id = app.state.terminals[&terminal_id].agent_id.to_string();

        let found = app.locate_agent(&agent_id).expect("resolves");
        assert!(found.local);
        assert!(found.route.is_none(), "no peer hop for a local agent");
    }

    #[test]
    fn an_unknown_identity_resolves_to_nothing() {
        // Must be a clean miss, not a wrong guess: routing a message to the
        // wrong agent is worse than refusing to route it.
        let app = test_app();
        assert!(app.locate_agent("agent_nowhere_9").is_none());
    }

    #[test]
    fn local_wins_over_a_peers_stale_view_of_us() {
        // A peer's summary is a poll snapshot; our own state is live. If both
        // claim the same agent, ours is the one that is true right now.
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("main")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].focused_pane_id().expect("pane");
        let terminal_id = app.state.workspaces[0]
            .pane_state(pane_id)
            .expect("pane state")
            .attached_terminal_id
            .clone();
        let agent_id = app.state.terminals[&terminal_id].agent_id.to_string();

        app.state.peer_summaries = vec![peer_with_agent("anvil", "anvil-dev", &agent_id)];

        let found = app.locate_agent(&agent_id).expect("resolves");
        assert!(
            found.local,
            "our own live state must win over a peer's cache"
        );
        assert_eq!(found.host, crate::app::short_host_name());
    }
}
