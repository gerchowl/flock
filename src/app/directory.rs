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

/// A directory row as the gossip layer holds it: where the agent is, plus the
/// little it reports about itself. Kept next to [`AgentLocation`] rather than
/// folded into it — routing needs the location alone, and widening the type
/// every caller matches on would make every one of them carry display fields
/// it has no use for.
struct FleetAgentEntry {
    location: AgentLocation,
    agent: Option<String>,
    status: crate::api::schema::AgentStatus,
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
        // Resolution is a FILTER over the same enumeration the fleet listing
        // uses, not a second walk of the same gossip state. Two walks is
        // exactly how "where is this agent" grows two answers — the defect
        // this module's header exists to prevent.
        self.remote_agent_entries()
            .into_iter()
            .find(|entry| entry.location.agent_id == agent_id)
            .map(|entry| entry.location)
    }

    /// Every agent the gossip layer knows about on OTHER hosts, in resolution
    /// order: directly-polled peers first, then one-hop relayed entries.
    ///
    /// Reads the snapshot the poller already maintains — it never talks to a
    /// peer. A listing that fanned out would turn a cheap call into fleet-wide
    /// I/O on every invocation, and would answer with data no fresher than the
    /// poll that just ran.
    fn remote_agent_entries(&self) -> Vec<FleetAgentEntry> {
        // Plain loops rather than chained iterators: the set is small and the
        // borrow shape stays obvious.
        let mut entries = Vec::new();
        for peer in &self.state.peer_summaries {
            let host = peer.host.clone().unwrap_or_else(|| peer.peer.clone());
            let route = Some(peer.peer.clone());
            for ws in &peer.workspaces {
                for agent in &ws.agents {
                    entries.push(FleetAgentEntry {
                        location: AgentLocation {
                            agent_id: agent.agent_id.clone(),
                            host: host.clone(),
                            pane_id: agent.pane_id.clone(),
                            local: false,
                            route: route.clone(),
                        },
                        agent: agent.agent.clone(),
                        status: agent.status,
                    });
                }
            }
        }
        for entry in self.state.relayed_fleet_cache.values() {
            let peer = &entry.peer;
            let host = peer.host.clone().unwrap_or_else(|| peer.peer.clone());
            for ws in &peer.workspaces {
                for agent in &ws.agents {
                    entries.push(FleetAgentEntry {
                        location: AgentLocation {
                            agent_id: agent.agent_id.clone(),
                            host: host.clone(),
                            pane_id: agent.pane_id.clone(),
                            local: false,
                            // A relayed entry is reachable only via the origin
                            // that relayed it; we have no direct edge, so there
                            // is no route of our own.
                            route: None,
                        },
                        agent: agent.agent.clone(),
                        status: agent.status,
                    });
                }
            }
        }
        entries
    }

    /// The fleet directory as a wire listing (#320): who can be messaged from
    /// this server, and by what name.
    ///
    /// Local agents come first and win any id collision, matching
    /// [`locate_agent`](Self::locate_agent) — a listing that disagreed with
    /// the resolver about which row is authoritative would be worse than no
    /// listing, because it would name a target that delivery then routes
    /// somewhere else. Ids are meant to be fleet-global, so a duplicate is a
    /// stale gossip snapshot of an agent that has since moved here.
    pub(crate) fn collect_fleet_agents(&self) -> Vec<crate::api::schema::FleetAgentInfo> {
        let host = crate::app::short_host_name();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut rows: Vec<crate::api::schema::FleetAgentInfo> = Vec::new();

        // Local rows reuse the ONE local agent walk rather than repeating it.
        for agent in self.collect_agent_infos() {
            if !seen.insert(agent.agent_id.clone()) {
                continue;
            }
            rows.push(crate::api::schema::FleetAgentInfo {
                agent_id: agent.agent_id,
                host: host.clone(),
                pane_id: agent.pane_id,
                local: true,
                route: None,
                agent: agent.agent,
                name: agent.name,
                status: agent.agent_status,
            });
        }

        for entry in self.remote_agent_entries() {
            if !seen.insert(entry.location.agent_id.clone()) {
                continue;
            }
            rows.push(crate::api::schema::FleetAgentInfo {
                agent_id: entry.location.agent_id,
                host: entry.location.host,
                pane_id: entry.location.pane_id,
                local: false,
                route: entry.location.route,
                agent: entry.agent,
                // A gossiped summary carries no operator-assigned name.
                name: None,
                status: entry.status,
            });
        }

        rows
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

    /// A local agent terminal: `collect_agent_infos` only reports panes that
    /// look like agents, so a bare test terminal has to be named first.
    fn name_local_agent(app: &mut App, name: &str) -> String {
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("main")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].focused_pane_id().expect("pane");
        let terminal_id = app.state.workspaces[0]
            .pane_state(pane_id)
            .expect("pane state")
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).expect("terminal");
        terminal.set_agent_name(name.to_string());
        terminal.agent_id.to_string()
    }

    #[test]
    fn the_fleet_listing_carries_local_and_remote_rows_with_the_route() {
        // #320: an MCP client could not message across hosts because nothing
        // would tell it a remote agent's id. The listing exists to be that
        // answer, so every field addressing needs has to be on the row.
        let mut app = test_app();
        let local_id = name_local_agent(&mut app, "here");
        app.state.peer_summaries = vec![peer_with_agent("anvil", "vm-dev", "agent_vm-dev_1")];

        let fleet = app.collect_fleet_agents();
        assert_eq!(fleet.len(), 2, "one local, one remote: {fleet:?}");

        let local = &fleet[0];
        assert_eq!(local.agent_id, local_id);
        assert!(local.local);
        assert_eq!(local.host, crate::app::short_host_name());
        assert_eq!(local.name.as_deref(), Some("here"));
        assert!(local.route.is_none());

        let remote = &fleet[1];
        assert_eq!(remote.agent_id, "agent_vm-dev_1");
        assert!(
            !remote.local,
            "a peer's agent is not addressable by pane id"
        );
        assert_eq!(remote.host, "vm-dev", "where it is");
        assert_eq!(
            remote.route.as_deref(),
            Some("anvil"),
            "how we reach it — the [[peers]] name, which routinely differs"
        );
        assert_eq!(remote.pane_id, "w1:p1", "a routing detail on THAT host");
    }

    #[test]
    fn the_listing_agrees_with_the_resolver_on_a_duplicated_id() {
        // Ids are meant to be fleet-global, so a collision is a stale snapshot
        // of an agent that has since moved here. The listing must resolve it
        // the same way `locate_agent` does — a listing that named a row
        // delivery then routes elsewhere is worse than no listing at all.
        let mut app = test_app();
        let local_id = name_local_agent(&mut app, "here");
        app.state.peer_summaries = vec![peer_with_agent("anvil", "anvil-dev", &local_id)];

        let fleet = app.collect_fleet_agents();
        let rows: Vec<&crate::api::schema::FleetAgentInfo> = fleet
            .iter()
            .filter(|row| row.agent_id == local_id)
            .collect();
        assert_eq!(rows.len(), 1, "one id, one row: {fleet:?}");
        assert!(rows[0].local);
        assert_eq!(
            rows[0].host,
            app.locate_agent(&local_id).expect("resolves").host,
            "the listing and the resolver must name the same host"
        );
    }

    #[test]
    fn the_listing_reads_the_gossip_snapshot_and_never_polls() {
        // The cheap-call property (#320): a fresh server with peers CONFIGURED
        // but nothing gossiped yet answers empty and instantly, rather than
        // fanning out to ask. Nothing here can reach the network to begin
        // with, so the assertion is that no peer produces a row.
        let app = test_app();
        assert!(app.state.peer_summaries.is_empty());
        assert!(app.collect_fleet_agents().is_empty());
    }
}
