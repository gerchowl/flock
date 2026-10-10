use crate::api::schema::{
    PeerSystemSummary, PeerWorkspaceSummary, RelayedFleetPeer, ResponseResult,
};
use crate::app::App;

use super::responses::{encode_error, encode_success};

impl App {
    /// Serve this server's federated summary: one entry per workspace with
    /// project identity + attention-leading agent status. Peers poll this
    /// over SSH to fold our workspaces into their sidebars.
    pub(super) fn handle_peers_summary(&mut self, id: String) -> String {
        encode_success(
            id,
            ResponseResult::PeersSummary {
                outbound_pending: self
                    .node_id
                    .as_deref()
                    .zip(
                        self.inbound
                            .edge(
                                self.current_api_peer_pid,
                                crate::platform::process_start_time,
                            )
                            .map(|edge| &edge.enrollment)
                            .filter(|edge| edge.state == "pinned")
                            .and_then(|edge| edge.node_id.as_deref()),
                    )
                    .is_some_and(|(_, hub)| {
                        crate::mesh::hello::with_store(|store| {
                            store
                                .has_outbound(hub, super::messages::now_ms() as i64)
                                .map_err(|e| e.to_string())
                        })
                        .unwrap_or(false)
                    }),
                node_id: self.node_id.clone(),
                clone_detection_warning: self.clone_detection_warning.clone(),
                host: short_host_name(),
                version: Some(crate::build_info::version()),
                protocol: Some(crate::protocol::PROTOCOL_VERSION),
                icon: configured_node_icon(),
                system: self.state.system_stats.as_ref().map(system_summary),
                workspaces: self.self_workspace_summaries(),
                // Gossip v3 (#101): relay this server's OWN polled peers only —
                // never the cache we received via relay ourselves. That bounds
                // hop count to one and precludes gossip loops.
                relayed_fleet: self.own_relayed_fleet(),
                pollers: Some(self.poller_health()),
            },
        )
    }

    /// Health of this server's periodic in-process pollers (#295).
    ///
    /// Thresholds come from the configured gossip staleness window rather
    /// than fresh constants, so "stale" means the same thing here as it does
    /// for a peer that stopped answering — one operator threshold, applied
    /// uniformly. The projection is deliberately cheap: no filesystem, no
    /// network, no per-workspace traversal. This runs on the peers-summary
    /// path (external monitor over SSH), not the render hot loop, so the
    /// per-frame realpath rules from #262 / #265 don't apply — but the same
    /// idea does: read stamped facts, don't compute them here.
    fn poller_health(&self) -> crate::api::schema::PollerHealthSummary {
        let now = std::time::Instant::now();
        let stale_after = self.state.config.gossip.stale_after().as_secs();
        let broken_mult = crate::app::state::RELAYED_ENTRY_TTL_STALE_MULTIPLE;

        let pr = project_poller_health(
            &self.pr_poll_health,
            now,
            stale_after,
            broken_mult,
            self.pr_poll_health.in_flight_since,
            |kind| kind.as_str().to_string(),
        );

        let git_refresh = Some(project_poller_health(
            &self.git_refresh_health,
            now,
            stale_after,
            broken_mult,
            self.git_refresh_health.in_flight_since,
            |kind| kind.as_str().to_string(),
        ));

        // A `[checks] enable = false` server has no runner tick to have
        // health for; report `None` rather than a permanent Broken row that
        // would misdirect a fleet monitor.
        let checks_runner = self.state.config.checks.enable.then(|| {
            project_poller_health(
                &self.checks_runner_health,
                now,
                stale_after,
                broken_mult,
                self.checks_runner_health.in_flight_since,
                |kind| kind.as_str().to_string(),
            )
        });

        // The peer poller only exists if peers are configured; a server with
        // no `[[peers]]` should not report a broken poller for a job it does
        // not have. The aggregate in-flight is projected from the tracker's
        // per-peer state — it's the fleet-wide "oldest fetch still out",
        // which is the wedge signal an operator alerts on.
        let peer_poll = (!self.state.peers.is_empty()).then(|| {
            project_poller_health(
                &self.peer_poll_health,
                now,
                stale_after,
                broken_mult,
                self.peer_poll_tracker.oldest_in_flight_since(),
                |kind| kind.as_str().to_string(),
            )
        });

        crate::api::schema::PollerHealthSummary {
            pr,
            git_refresh,
            checks_runner,
            peer_poll,
        }
    }

    /// One-hop relay payload: THIS server's own polled peers as
    /// [`RelayedFleetPeer`] entries, stamped with the answering server as
    /// `origin`. Never includes `state.relayed_fleet_cache` — that would
    /// re-relay entries that already travelled one hop.
    /// `peers.hub_fleet` — the hub that polls this server shares what it knows
    /// about the rest of the fleet (#410).
    ///
    /// Merged through the ONE relay merge, so every row passes
    /// `relayed_entry_from_wire` (#392: an ssh_target or proxy_jump this host
    /// will not dial is dropped and logged) and freshest-wins against rows
    /// from any other source. Each row routes via the hub, which is what a
    /// message for one of its agents then hands up to.
    ///
    /// Accepted only from the relay bound by `mesh.hello`, and the rows
    /// it carries are display-only: a spoke holds no key to anything, so it
    /// never dials a hub-pushed row's ssh target at all.
    ///
    /// Never re-relayed: this server's own `relayed_fleet` is built from the
    /// peers IT polls, not from this cache, so the hub's view cannot come back
    /// up as the spoke's own.
    pub(super) fn handle_peers_hub_fleet(
        &mut self,
        id: String,
        params: crate::api::schema::PeersHubFleetParams,
    ) -> String {
        // The socket caller may speak only for its own enrolled edge.
        if let Some(refusal) = self.refuse_unless_edge(&id, "peers.hub_fleet") {
            return refusal;
        }
        // The verified key maps to this locally enrolled name.
        let Some(hub) = self
            .inbound
            .edge(
                self.current_api_peer_pid,
                crate::platform::process_start_time,
            )
            .filter(|edge| edge.enrolled())
            .map(|edge| edge.enrollment.peer.clone())
        else {
            return super::responses::encode_error(
                id,
                "mesh_not_enrolled",
                "peers.hub_fleet requires mesh.hello enrollment",
            );
        };
        let mut fleet = params.fleet;
        // #424: the hub's own row, heard only under the name bound to this
        // relay. A row about some other machine is not the hub speaking for
        // itself.
        //
        let hub_key = hub.to_ascii_lowercase();
        let hub_self = params
            .hub_self
            .map(|row| *row)
            .filter(|row| crate::peers::wire_row_identity(row) == hub_key);
        match self.state.fleet_snapshot.as_mut() {
            Some(snapshot) => {
                // The home row is the one the operator trusts, so exactly one
                // voice may repaint it: the bound hub, about itself, and only
                // when that hub IS the client's home. A `fleet` row claiming
                // to be home is another machine's say-so (any spoke the hub
                // polls can report any host), so it is dropped rather than
                // absorbed, and never stored as a second home row either.
                fleet = snapshot.without_origin_claims(fleet);
                if let Some(row) = hub_self {
                    if snapshot.origin.eq_ignore_ascii_case(&hub) {
                        snapshot.absorb_hub_self(row);
                    } else {
                        fleet.push(row);
                    }
                }
            }
            None => fleet.extend(hub_self),
        }
        crate::peers::merge_hub_pushed_fleet(&mut self.state.relayed_fleet_cache, fleet, &hub);
        self.note_mesh_owners();
        self.state.evict_expired_relayed_entries();
        self.render_dirty
            .store(true, std::sync::atomic::Ordering::Release);
        self.render_notify.notify_one();
        super::responses::encode_success(id, crate::api::schema::ResponseResult::Ok {})
    }

    /// This server's own row for the `peers.hub_fleet` push (#424): the same
    /// summary a hub stamps as a snapshot's origin at switch time, sent again
    /// on every poll so a client that left this hub keeps a live view of it.
    pub(crate) fn hub_self_row(&self) -> RelayedFleetPeer {
        let us = short_host_name();
        RelayedFleetPeer {
            node_id: self.node_id.clone(),
            name: us.clone(),
            // Never dialled by the spoke (hub-pushed rows are display-only),
            // and a spoke that already has a route to us keeps its own.
            ssh_target: us.clone(),
            host: Some(us.clone()),
            version: Some(crate::build_info::version()),
            protocol: Some(crate::protocol::PROTOCOL_VERSION),
            system: self.state.system_stats.as_ref().map(system_summary),
            latency_ms: None,
            workspaces: self.self_workspace_summaries(),
            // We ARE the origin of our own reading, taken right now.
            age_secs: Some(0),
            error: None,
            origin: us,
            origin_last_ok_secs: Some(0),
            proxy_jump: None,
            icon: configured_node_icon(),
            dial: None,
        }
    }

    pub(super) fn own_relayed_fleet(&self) -> Vec<RelayedFleetPeer> {
        let origin = short_host_name();
        self.state
            .peer_summaries
            .iter()
            .map(|peer| {
                let age_secs = peer.last_ok.map(|at| at.elapsed().as_secs());
                RelayedFleetPeer {
                    node_id: peer.node_id.clone(),
                    // #418: how this server's dials to the peer are failing,
                    // which is what `flk peers` shows.
                    dial: peer.dial_report(std::time::Instant::now()),
                    name: peer.peer.clone(),
                    ssh_target: peer.ssh_target.clone(),
                    host: peer.host.clone(),
                    version: peer.version.clone(),
                    protocol: peer.protocol,
                    system: peer.system.clone(),
                    latency_ms: peer.latency_ms,
                    workspaces: peer.workspaces.clone(),
                    age_secs,
                    // #428: the token only; ssh's words stay in our log.
                    error: peer.error.as_deref().map(crate::peers::wire_error),
                    origin: origin.clone(),
                    // Gossip v3 (#101 part 2): we ARE the origin for our own
                    // polled peers, so the origin's assertion is our
                    // last-successful-poll age at emission time.
                    origin_last_ok_secs: age_secs,
                    // Gossip v3 (#101 part 3): we ARE the reachable identity
                    // for peers we polled directly — a receiver dialing
                    // these needs `-o ProxyJump=<us>` to reach them.
                    proxy_jump: Some(origin.clone()),
                    // #164: carry the polled peer's self-declared icon through
                    // the one-hop relay so a two-hop viewer sees the same glyph.
                    icon: peer.icon.clone(),
                }
            })
            .collect()
    }

    /// Prepare one of THIS server's workspaces for a cross-machine checkout
    /// (#125, "defer to the client"): resolve the repo + branch from the named
    /// workspace, then probe (and optionally push) on our OWN git. The hub
    /// drives this over SSH and fetches the branch from origin afterwards — it
    /// never reaches into our `.git`, keeping the model hub-spoke.
    pub(super) fn handle_peers_checkout_prepare(
        &mut self,
        id: String,
        params: crate::api::schema::PeersCheckoutPrepareParams,
    ) -> String {
        let Some(ws) = self
            .state
            .workspaces
            .iter()
            .find(|ws| ws.id == params.workspace_id)
        else {
            return encode_error(
                id,
                "workspace_not_found",
                format!("workspace '{}' not found", params.workspace_id),
            );
        };
        let Some(branch) = ws.branch() else {
            return encode_error(
                id,
                "no_branch",
                format!(
                    "workspace '{}' has no git branch to prepare",
                    params.workspace_id
                ),
            );
        };
        // The branch above comes from the live probe (the root pane's CURRENT
        // cwd); `resolved_identity_cwd` is frozen at construction. Mixing them
        // pushes a branch that exists in one repo from the directory of
        // another — `prepare_peer_checkout` runs `git -C <checkout> push -u
        // origin <branch>`. Resolve the checkout the same live way the branch
        // was resolved.
        let Some(checkout) =
            ws.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
        else {
            return encode_error(
                id,
                "no_checkout",
                format!(
                    "workspace '{}' has no resolved checkout path",
                    params.workspace_id
                ),
            );
        };
        match crate::worktree::prepare_peer_checkout(&checkout, &branch, params.push) {
            Ok(report) => encode_success(
                id,
                ResponseResult::PeersCheckoutPrepared {
                    branch,
                    was_dirty: report.was_dirty,
                    was_unpushed: report.was_unpushed,
                    pushed: report.pushed,
                },
            ),
            Err(err) => encode_error(id, "checkout_prepare_failed", err),
        }
    }

    /// This server's own workspaces in the federated summary shape — the same
    /// rollup `peers.summary` serves, reused so the origin entry a hub stamps
    /// into its down-gossip snapshot (#66) is byte-identical to what a peer
    /// would poll.
    fn self_workspace_summaries(&self) -> Vec<PeerWorkspaceSummary> {
        self.state
            .workspaces
            .iter()
            .map(|ws| workspace_peer_summary(ws, &self.state.terminals))
            .collect()
    }

    /// This server's OWN entry for a down-gossip snapshot (#66): the self
    /// summary as a wire `FleetPeer`, targeted at the reserved home sentinel
    /// so a spoke selecting one of these workspace rows lands HOME (a spoke
    /// has no ssh route to the hub), with the workspace carried as the
    /// post-attach focus target. `age_secs = 0`: stamped fresh at switch.
    fn origin_self_summary(&self) -> crate::protocol::FleetPeer {
        crate::protocol::FleetPeer {
            name: short_host_name(),
            ssh_target: crate::protocol::HOME_SWITCH_TARGET.to_string(),
            host: Some(short_host_name()),
            version: Some(crate::build_info::version()),
            protocol: Some(crate::protocol::PROTOCOL_VERSION),
            system: self
                .state
                .system_stats
                .as_ref()
                .map(system_summary)
                .map(Into::into),
            latency_ms: None,
            workspaces: self
                .self_workspace_summaries()
                .into_iter()
                .map(Into::into)
                .collect(),
            age_secs: Some(0),
            error: None,
            // We ARE the origin for our own summary — stamped fresh at switch.
            origin_last_ok_secs: Some(0),
            // The client dials home directly via the reserved sentinel — no
            // ProxyJump involved.
            proxy_jump: None,
            // #164: our OWN self-declared icon, so a spoke's home row shows it.
            icon: configured_node_icon(),
        }
    }
}

/// Project a `PollerHealthCore` into the wire `PollerHealth`. Overrides the
/// core's own `in_flight_since` so aggregate pollers (peer_poll) can supply
/// the OLDEST across their tracked in-flights while single-round pollers
/// (pr, git_refresh) just pass their own field through.
fn project_poller_health<E: Copy + Eq>(
    health: &crate::health::PollerHealthCore<E>,
    now: std::time::Instant,
    stale_after_secs: u64,
    broken_multiple: u64,
    in_flight_since_override: Option<std::time::Instant>,
    err_as_str: impl Fn(&E) -> String,
) -> crate::api::schema::PollerHealth {
    let status = health
        .status_at(now, stale_after_secs, broken_multiple)
        .as_str()
        .to_string();
    crate::api::schema::PollerHealth {
        status,
        last_success_age_secs: health.last_success_age_secs(now),
        consecutive_failures: health.consecutive_failures,
        in_flight: in_flight_since_override.is_some(),
        in_flight_age_secs: in_flight_since_override
            .map(|at| now.saturating_duration_since(at).as_secs()),
        skipped_rounds: health.skipped_rounds,
        last_error: health.last_error.as_ref().map(&err_as_str),
    }
}

/// Switch popup label: the server, plus the space when the switch names one.
fn switch_label(server: &str, target: Option<&PeerWorkspaceSummary>) -> String {
    match target {
        Some(ws) => format!("{server}:{}", ws.workspace),
        None => server.to_string(),
    }
}

/// The workspace id the arriving client should focus, if the switch named a
/// space that still carries one. A server-assigned id is `ws_<n>`; an empty id
/// (a peer too old to report one) means "no target", not "focus nothing".
fn focus_target(target: Option<&PeerWorkspaceSummary>) -> Option<String> {
    target
        .map(|ws| ws.id.clone())
        .filter(|id| !id.trim().is_empty())
}

/// Host key a fleet entry dedupes under: the host it reports about itself,
/// falling back to its ssh target, lowercased. Mirrors the receiving end's
/// `row_host_key`, so emit-side and render-side collapse the same rows.
fn wire_host_key(peer: &crate::protocol::FleetPeer) -> String {
    peer.host
        .as_deref()
        .filter(|host| !host.is_empty())
        .unwrap_or(&peer.ssh_target)
        .to_ascii_lowercase()
}

/// Union two fleet peer lists into the one a leg carries.
///
/// Entries about the hop target (it becomes the self row over there) and about
/// the snapshot's origin (the home row owns that slot) are dropped. Otherwise
/// one entry survives per host: the FRESHER by `origin_last_ok_secs` (the
/// origin's reading plus however long it has since sat somewhere), ties going
/// to the earlier list — the same quantity `AppState::remote_peers` ranks on
/// when it renders them, so a row can't win here and lose there.
///
/// Capped like the carried snapshot itself: the list rides an env var between
/// attach legs, and an unbounded fleet could brush ARG_MAX and kill the leg.
/// Callers pass their FIRST-HAND rows as `first` for that reason — the cap
/// truncates the tail, so on a fleet large enough to hit it the rows that
/// survive should be the ones this server actually polled, not the stalest
/// entries of a list it was handed.
fn merge_fleet_peers(
    first: Vec<crate::protocol::FleetPeer>,
    rest: Vec<crate::protocol::FleetPeer>,
    exclude_ssh_target: &str,
    origin: &str,
) -> Vec<crate::protocol::FleetPeer> {
    let exclude_lower = exclude_ssh_target.to_ascii_lowercase();
    let origin_lower = origin.to_ascii_lowercase();
    let mut merged: Vec<crate::protocol::FleetPeer> = Vec::new();
    let mut by_host: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for peer in first.into_iter().chain(rest) {
        let key = wire_host_key(&peer);
        if key == exclude_lower
            || key == origin_lower
            || peer.ssh_target.eq_ignore_ascii_case(exclude_ssh_target)
        {
            continue;
        }
        match by_host.get(&key) {
            Some(&idx) => {
                // Smaller age = fresher; a known age beats an unknown.
                let current = merged[idx].origin_last_ok_secs;
                let replace = match (current, peer.origin_last_ok_secs) {
                    (Some(cur), Some(new)) => new < cur,
                    (None, Some(_)) => true,
                    _ => false,
                };
                if replace {
                    merged[idx] = peer;
                }
            }
            None => {
                by_host.insert(key, merged.len());
                merged.push(peer);
            }
        }
    }
    merged.truncate(crate::peers::FLEET_SNAPSHOT_MAX_PEERS);
    merged
}

/// Re-encode a relayed row for an outgoing snapshot.
///
/// `peer_to_wire` already carries the accumulated age (origin's reading plus
/// our dwell), so a receiver inherits an honest reading rather than the
/// capture-time one this hub was handed.
fn relayed_peer_to_wire(entry: &crate::peers::RelayedEntry) -> crate::protocol::FleetPeer {
    crate::peers::peer_to_wire(&entry.peer)
}

/// Map the local status-line stats sampler onto the federated summary shape.
fn system_summary(stats: &crate::system_stats::SystemStats) -> PeerSystemSummary {
    PeerSystemSummary {
        cpu_percent: stats
            .cpu_percent
            .map(|cpu| cpu.round().clamp(0.0, 100.0) as u8),
        mem_used: stats.mem_used,
        mem_total: stats.mem_total,
        disk_free: stats.disk_free,
        // #291: the sampler has read GPU utilization since the status line
        // shipped; until this bump it stopped at the box, so every viewer saw
        // a blank GPU column on every row but its own.
        gpu_percent: stats.gpu_percent,
        // #298: sanitize on the way OUT too. Every RECEIVE path normalizes a
        // peer's declaration; once a host reporter writes this field locally
        // we are the peer, and our own broken reporter must not be the one
        // thing that escapes the clamp.
        thermal: stats
            .thermal
            .clone()
            .map(crate::api::schema::ThermalReport::sanitized),
    }
}

/// A resolved server switch ready to send to the foreground client:
/// the next attach target plus the fleet snapshot that leg carries.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PreparedServerSwitch {
    pub(crate) ssh_target: String,
    pub(crate) label: String,
    pub(crate) fleet: Option<crate::protocol::FleetSnapshot>,
    /// Workspace id to focus once the next leg attaches. Set whenever the
    /// switch names a SPACE rather than just a server (#80) — any remote
    /// workspace row, or an origin-workspace row landing home. The client
    /// delivers it in band with `ClientMessage::FocusWorkspace` once it is
    /// attached, so it cannot lose a race with the attach.
    pub(crate) focus_workspace: Option<String>,
    /// Gossip v3 (#101 part 3): SSH ProxyJump identity for reaching
    /// `ssh_target` — set only for snapshot-derived rows the launcher cannot
    /// dial directly. `None` for a config peer the launcher already has a
    /// route to. The bridge appends `-o ProxyJump=<value>` when set.
    pub(crate) proxy_jump: Option<String>,
}

impl App {
    /// Record a fleet failure the launcher handed back in the attach notice
    /// (#420): a switch that never established, or a remote session that gave
    /// up reconnecting. The reason goes on that host's servers-band row, and
    /// the notice into the operator's notification log (ADR-0016), where it
    /// outlives the few seconds the top-right notice is up. Notices of any
    /// other shape are left to the action notice alone.
    pub(crate) fn note_fleet_failure_notice(&mut self, notice: &str) {
        let Some(failure) = crate::peers::FleetFailureNotice::parse(notice) else {
            return;
        };
        // The reason can be the first line of flock's own error, which can
        // carry remote output: mask credentials and strip control bytes
        // before it becomes a durable record.
        let title = crate::control_bytes::strip(&crate::report::redact::mask_credentials(notice));
        if let Some(reason) = failure.reason() {
            self.state
                .switch_failures
                .insert(failure.target().to_string(), reason);
        }
        self.state
            .file_notification(crate::app::notifications::NotificationEntry {
                id: crate::app::notifications::mint_notification_id(),
                title,
                body: None,
                kind: crate::api::schema::NotificationRecordKind::Notice,
                source: crate::api::schema::NotificationSource::Fleet,
                workspace_id: None,
                pane_id: None,
                origin_host: crate::app::short_host_name(),
                filed_at_ms: crate::app::notifications::now_ms(),
                seen: false,
            });
    }

    /// Resolve a server-switch request from the sidebar or the switch_home
    /// keybind into the SwitchServer payload. Returns None when the request
    /// no longer resolves (rows changed) — or for Home without an origin.
    pub(crate) fn prepare_switch_server(
        &mut self,
        request: crate::app::state::PeerSwitchRequest,
    ) -> Option<PreparedServerSwitch> {
        use crate::app::state::PeerSwitchRequest;
        match request {
            PeerSwitchRequest::ConfigPeer { peer_idx, ws_idx } => {
                let peer = self.state.peer_summaries.get(peer_idx)?;
                let ssh_target = peer.ssh_target.clone();
                let target = ws_idx.and_then(|ws_idx| peer.workspaces.get(ws_idx));
                let label = switch_label(peer.display_name(), target);
                let focus_workspace = focus_target(target);
                let fleet = Some(self.outgoing_fleet_snapshot(&ssh_target));
                Some(PreparedServerSwitch {
                    ssh_target,
                    label,
                    fleet,
                    focus_workspace,
                    // Config peer: launcher's box already has a direct SSH
                    // route (that's the definition of a `[[peers]]` entry).
                    proxy_jump: None,
                })
            }
            PeerSwitchRequest::SnapshotPeer { entry_idx, ws_idx } => {
                let entry = self.state.fleet_snapshot.as_ref()?.peers.get(entry_idx)?;
                let ssh_target = entry.ssh_target.clone();
                let target = ws_idx.and_then(|ws_idx| entry.workspaces.get(ws_idx));
                let label = switch_label(entry.display_name(), target);
                let focus_workspace = focus_target(target);
                // Gossip v3 (#101 part 3): the snapshot entry was stamped by
                // the hub that emitted it — use its ProxyJump identity so the
                // client dials via that hub instead of trying `ssh_target`
                // directly. `None` if the entry pre-dates v3.
                let proxy_jump = entry.proxy_jump.clone();
                let fleet = Some(self.outgoing_fleet_snapshot(&ssh_target));
                Some(PreparedServerSwitch {
                    ssh_target,
                    label,
                    fleet,
                    focus_workspace,
                    proxy_jump,
                })
            }
            PeerSwitchRequest::RelayedPeer { host_key, ws_idx } => {
                let relayed = self.state.relayed_fleet_cache.get(&host_key)?;
                // #410: a row a hub pushed down is display-only — never dialled.
                // #424: but when the client CARRIED a route to the same machine,
                // the live row is only the better reading of a server it can
                // already reach — dial the carried route, never the pushed one.
                if relayed.hub_pushed {
                    return self.switch_via_carried_route(&host_key, &relayed.peer, ws_idx);
                }
                let entry = &relayed.peer;
                let ssh_target = entry.ssh_target.clone();
                let name = entry.host.clone().unwrap_or_else(|| entry.peer.clone());
                let target = ws_idx.and_then(|ws_idx| entry.workspaces.get(ws_idx));
                let label = switch_label(&name, target);
                let focus_workspace = focus_target(target);
                let proxy_jump = Some(self.relayed_route(relayed, &short_host_name())?);
                let fleet = Some(self.outgoing_fleet_snapshot(&ssh_target));
                Some(PreparedServerSwitch {
                    ssh_target,
                    label,
                    fleet,
                    focus_workspace,
                    proxy_jump,
                })
            }
            PeerSwitchRequest::OriginWorkspace { ws_idx } => {
                // Land home (the spoke has no ssh route to the hub) with the
                // selected origin workspace as the post-attach focus target.
                let snapshot = self.state.fleet_snapshot.as_ref()?;
                let origin = snapshot.origin.clone();
                let ws = snapshot.origin_summary.as_ref()?.workspaces.get(ws_idx)?;
                let focus_workspace = (!ws.id.is_empty()).then(|| ws.id.clone());
                let label = format!("{origin}:{}", ws.workspace);
                Some(PreparedServerSwitch {
                    ssh_target: crate::protocol::HOME_SWITCH_TARGET.to_string(),
                    label,
                    fleet: None,
                    focus_workspace,
                    proxy_jump: None,
                })
            }
            PeerSwitchRequest::Home => {
                let origin = self.state.fleet_snapshot.as_ref()?.origin.clone();
                Some(PreparedServerSwitch {
                    ssh_target: crate::protocol::HOME_SWITCH_TARGET.to_string(),
                    label: format!("{origin} (home)"),
                    fleet: None,
                    focus_workspace: None,
                    proxy_jump: None,
                })
            }
        }
    }

    /// Switch to the machine a hub-pushed row describes, over the route the
    /// client's own carried snapshot holds for that machine (#424).
    ///
    /// Before the hub pushed its view down, such a machine rendered as its
    /// carried snapshot row, frozen but clickable. The pushed row now wins the
    /// dedup because it is live, and it is display-only, so without this the
    /// click that used to work silently did nothing. Everything that decides
    /// where ssh goes (target, ProxyJump) comes from the carried row. The
    /// pushed row supplies only the space to focus, by id, so a space opened
    /// there after the switch is reachable too. `None` when nothing carried a
    /// route: the row stays display-only.
    ///
    /// The match is the exact host the relay cache keys the row under, never
    /// the domain-stripped sort key: `kiln.other` is not `kiln`, and a click
    /// on one must not dial the other's route.
    fn switch_via_carried_route(
        &self,
        host_key: &str,
        pushed: &crate::peers::PeerSummaryState,
        ws_idx: Option<usize>,
    ) -> Option<PreparedServerSwitch> {
        let carried = self
            .state
            .fleet_snapshot
            .as_ref()?
            .peers
            .iter()
            .find(|entry| crate::app::state::row_host_key(entry) == host_key)?;
        let ssh_target = carried.ssh_target.clone();
        let proxy_jump = carried.proxy_jump.clone();
        let target = ws_idx.and_then(|ws_idx| pushed.workspaces.get(ws_idx));
        let label = switch_label(carried.display_name(), target);
        let focus_workspace = focus_target(target);
        let fleet = Some(self.outgoing_fleet_snapshot(&ssh_target));
        Some(PreparedServerSwitch {
            ssh_target,
            label,
            fleet,
            focus_workspace,
            proxy_jump,
        })
    }

    /// The fleet snapshot the next attach leg carries.
    ///
    /// The ORIGIN is pass-through and never re-stamped: a nested leap keeps
    /// the client's real home, so the way back is always the way it came. The
    /// PEER LIST is not — every leg unions in what THIS server can see (its
    /// own polled peers, the entries relayed to it, and itself). Forwarding a
    /// carried snapshot verbatim was the reason the fleet looked different
    /// from every machine: the chain could only ever propagate what the first
    /// hub happened to know, so a server two hops out saw a strictly smaller
    /// fleet than the one it was standing next to.
    ///
    /// The hop target is excluded throughout — it becomes the self row on the
    /// receiving end.
    fn outgoing_fleet_snapshot(&self, exclude_ssh_target: &str) -> crate::protocol::FleetSnapshot {
        let us = short_host_name();
        // Gossip v3 (#101 part 3): stamp our own reachable identity on every
        // peer we polled ourselves. The client's next-leg bridge uses it as
        // `-o ProxyJump=<us>` to reach peers only routable through this hub.
        // A relayed row needs the relayer too, see `relayed_route` (#441).
        let stamp_proxy_jump = |mut peer: crate::protocol::FleetPeer| {
            peer.proxy_jump.get_or_insert_with(|| us.clone());
            peer
        };
        // Our own view, in dedup priority order: peers we polled ourselves
        // (first-hand, real-time age) ahead of entries relayed to us.
        let mut ours: Vec<crate::protocol::FleetPeer> = self
            .state
            .peer_summaries
            .iter()
            .map(crate::peers::peer_to_wire)
            .map(stamp_proxy_jump)
            .collect();
        // #410: never a row a hub pushed down. It is another server's view that
        // THIS node never verified, and display-only here — but a snapshot row
        // is dialled on the next server, on a click and by warm slots with no
        // click at all, so carrying it on would undo display-only one hop later.
        let mut relayed: Vec<_> = self
            .state
            .relayed_fleet_cache
            .iter()
            .filter(|(_, entry)| !entry.hub_pushed)
            .collect();
        relayed.sort_by_key(|(host_key, _)| *host_key);
        // A row with no route (its relayer left our `[[peers]]`) is display
        // only here, so it is not carried to where it would be dialled.
        ours.extend(relayed.into_iter().filter_map(|(_, entry)| {
            let mut peer = relayed_peer_to_wire(entry);
            peer.proxy_jump = Some(self.relayed_route(entry, &us)?);
            Some(peer)
        }));

        match self.state.fleet_snapshot.as_ref() {
            Some(carried) => {
                let mut snapshot = carried.to_wire(exclude_ssh_target);
                // Nothing else in the chain carries US: the origin slot belongs
                // to the client's home, and the hub that told us about our
                // peers excluded us from the snapshot it sent. Emit ourselves
                // so a leap never loses the server it just left. Best-effort
                // target: our short host name — the same identity we already
                // stamp as `proxy_jump` for peers only routable through us.
                // (When we ARE the carried origin, the merge drops this again:
                // the home row already stands for us over there.)
                ours.push(self.self_peer_entry(&us));
                snapshot.peers =
                    merge_fleet_peers(ours, snapshot.peers, exclude_ssh_target, &snapshot.origin);
                snapshot
            }
            None => crate::protocol::FleetSnapshot {
                peers: merge_fleet_peers(ours, Vec::new(), exclude_ssh_target, &us),
                origin: us,
                // The hub is not its own peer; embed its own workspaces so
                // the spoke can see the way-home spaces, not just peers (#66).
                origin_summary: Some(Box::new(self.origin_self_summary())),
            },
        }
    }

    /// The `ProxyJump` chain that reaches a relayed row, written from the
    /// client's side of this server (#441): through us, then through the
    /// relayer that polls the row.
    ///
    /// A relayed row's `ssh_target` is the RELAYER's name for that machine
    /// (kiln's `ws00860001` for node-b), which resolves nowhere else. Stamping
    /// it with this server alone sent `ssh -J hopper ws00860001`, a name hopper
    /// cannot resolve. The relayer hop is how THIS server reaches the relayer:
    /// its own `[[peers]]` target, nothing the row says about itself.
    ///
    /// `None` when the relayer is no longer one of our `[[peers]]` (the cache
    /// outlives a config edit until eviction). The row is then display-only:
    /// a relayer the operator removed does not get to pick a jump host.
    ///
    /// The client cuts whichever leading hops are its own machine at dial
    /// time, so the chain stays correct whether the client runs here, on the
    /// relayer, or anywhere else. Only a first-hand relayed row reaches here:
    /// a hub-pushed row is never dialled.
    fn relayed_route(&self, entry: &crate::peers::RelayedEntry, us: &str) -> Option<String> {
        let via = entry.via.as_deref()?;
        let relayer = self
            .state
            .peer_summaries
            .iter()
            .find(|peer| peer.peer == via)
            .map(|peer| peer.ssh_target.as_str())
            .filter(|hop| !hop.is_empty())?;
        Some(format!("{us},{relayer}"))
    }

    /// This server as a PEER entry for a pass-through snapshot — unlike
    /// [`Self::origin_self_summary`], which claims the origin slot and dials
    /// via the reserved home sentinel, this one is dialled like any other
    /// server (by host name, no ProxyJump: the client is attached to us right
    /// now, so it has a route).
    fn self_peer_entry(&self, us: &str) -> crate::protocol::FleetPeer {
        crate::protocol::FleetPeer {
            ssh_target: us.to_string(),
            proxy_jump: None,
            ..self.origin_self_summary()
        }
    }
}

/// Short, stable hostname for the status line and peer identity. Cached for the
/// session. On macOS this prefers the user-set `LocalHostName` over the network
/// hostname, which on corp/campus DHCP (e.g. ETH `staff-net-*.intern.ethz.ch`)
/// is an unstable name nobody recognizes.
pub(crate) fn short_host_name() -> String {
    use std::sync::OnceLock;
    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED.get_or_init(compute_short_host_name).clone()
}

fn compute_short_host_name() -> String {
    // ADR-0002 phase (d): FLOCK_HOST_NAME is no longer read here — it's now a
    // one-release deprecated alias that lands on Config.name via the generic
    // FLOCK_<UPPER_SNAKE> env layer (src/config/env.rs), which sits BELOW the
    // file. The test-suite host pin (short, deterministic host on CI runners
    // with long `fv-az…` names) still works because `configured_node_name()`
    // reads the same loaded config the env alias populates.
    //
    // Read once (short_host_name caches), so a changed name takes effect on
    // restart, matching how the OS host name is treated as fixed per-process.
    if let Some(name) = configured_node_name() {
        return name;
    }
    #[cfg(target_os = "macos")]
    if let Some(name) = macos_local_host_name() {
        return name;
    }
    sysinfo::System::host_name()
        .map(|h| h.split('.').next().unwrap_or(&h).to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// The `name` from config.toml, if set — the node's friendly self-label (#42).
/// Uses the overlay-aware load so a `name` set in `config.local.toml` works for
/// nix/HM users whose base `config.toml` is a read-only symlink (the exact
/// centrally-managed-box population this targets). A broken config falls back to
/// the OS host name rather than failing hostname resolution.
fn configured_node_name() -> Option<String> {
    let name = crate::config::load_live_config().ok()?.config.name;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// This node's SELF-DECLARED fleet icon NAME (#164) to gossip, overlay-aware
/// like [`configured_node_name`]. Emitted ONLY when the configured `icon`
/// resolves to a known registry glyph — an unknown/typo'd name yields `None`,
/// so garbage never enters the wire (and a receiver would drop it anyway).
/// Cached once (restart-to-apply), matching how `short_host_name` treats the
/// self label — avoids a config parse on every peer poll.
pub(crate) fn configured_node_icon() -> Option<String> {
    use std::sync::OnceLock;
    static CACHED: OnceLock<Option<String>> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            let icon = crate::config::load_live_config().ok()?.config.icon?;
            let name = icon.trim();
            if name.is_empty() {
                return None;
            }
            if crate::server_icons::is_renderable(name) {
                // Gossip the value verbatim (a registry name or a raw glyph) —
                // every receiver resolves it the same way we do.
                return Some(name.to_string());
            }
            // A typo or an oversized/unsafe value: warn once (this init runs
            // once), listing the known names so the fix is obvious; don't
            // gossip garbage, render no icon.
            crate::logging::unknown_server_icon(
                name,
                &crate::server_icons::known_names().join(", "),
            );
            None
        })
        .clone()
}

#[cfg(target_os = "macos")]
fn macos_local_host_name() -> Option<String> {
    let out = crate::process::TracedCommand::new("/usr/sbin/scutil", "peers")
        .args(["--get", "LocalHostName"])
        .output_traced()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

fn workspace_peer_summary(
    ws: &crate::workspace::Workspace,
    terminals: &std::collections::HashMap<
        crate::terminal::TerminalId,
        crate::terminal::TerminalState,
    >,
) -> PeerWorkspaceSummary {
    let (state, seen) = ws.aggregate_state(terminals);
    // The attention-leading pane: highest priority, oldest transition first —
    // mirrors the local focus_attention ordering. Panes without a transition
    // timestamp sort as newest.
    let now = std::time::Instant::now();
    let leading = ws
        .pane_details(terminals)
        .into_iter()
        .filter(|detail| (detail.state, detail.seen) == (state, seen))
        .min_by_key(|detail| detail.state_changed_at.unwrap_or(now));
    let (agent, status_age_secs, activity) = leading
        .map(|detail| {
            (
                Some(agent_label_on_the_wire(&detail)),
                detail
                    .state_changed_at
                    .map(|changed| changed.elapsed().as_secs()),
                detail.live_activity,
            )
        })
        .unwrap_or((None, None, None));

    // The git-space cache is populated by the periodic async refresh, so a
    // freshly-created workspace may not have it yet. Derive the project
    // identity live from the checkout in that cold-start window so the peer
    // row can still fold by project.
    let derived_space = ws
        .git_space()
        .is_none()
        .then(|| ws.resolved_identity_cwd())
        .flatten()
        .and_then(|cwd| crate::workspace::git_space_metadata(&cwd));
    let project_key = ws.project_key().map(str::to_string).or_else(|| {
        derived_space
            .as_ref()
            .map(|space| space.project_key.clone())
    });
    let project_label = ws
        .git_space()
        .map(|space| space.label.clone())
        .or_else(|| derived_space.as_ref().map(|space| space.label.clone()))
        .or_else(|| ws.worktree_space().map(|space| space.label.clone()));

    PeerWorkspaceSummary {
        id: ws.id.clone(),
        workspace: ws.display_name(),
        project_key,
        project_label,
        branch: ws.branch(),
        is_linked_worktree: ws
            .git_space()
            .map(|space| space.is_linked_worktree)
            .or_else(|| ws.worktree_space().map(|space| space.is_linked_worktree))
            .unwrap_or(false),
        agent,
        status: super::super::api_helpers::pane_agent_status(state, seen),
        status_age_secs,
        activity,
        // Directory rows: every agent here, by identity (ADR-0008). The
        // sidebar wants the leading pane; a directory wants all of them.
        agents: ws
            .pane_details(terminals)
            .into_iter()
            .filter_map(|detail| {
                let terminal =
                    terminals.get(&ws.pane_state(detail.pane_id)?.attached_terminal_id)?;
                if !terminal.is_agent_terminal() {
                    return None;
                }
                let pane_number = ws.public_pane_number(detail.pane_id)?;
                Some(crate::api::schema::PeerAgentSummary {
                    agent_id: terminal.agent_id.to_string(),
                    pane_id: crate::workspace::public_pane_id_for_number(&ws.id, pane_number),
                    agent: Some(agent_label_on_the_wire(&detail)),
                    status: super::super::api_helpers::pane_agent_status(detail.state, detail.seen),
                })
            })
            .collect(),
    }
}

/// What a peer summary says the agent IS, rather than what it is called (#542).
///
/// `PaneDetail.agent_label` is a *display* string: `effective_display_agent()`
/// first, then `terminal.agent_name`, then the canonical label. For an agent
/// spawned through `flock_agent_start` with a caller-supplied name, that is the
/// calling model's invention — so sending it verbatim means the viewer receives
/// `"researcher"`, cannot parse a harness out of it, and its default `symbol`
/// mode prints `researcher`. That is #542 again, on every remote row.
///
/// The sender holds the fact the viewer needs: `detail.agent` is the harness the
/// detector or hook identified. So the wire carries the harness's label, and the
/// free-text label is the fallback for a pane with no identifiable harness —
/// which is all the viewer could have done with it anyway.
///
/// The cost, stated rather than hidden: a remote agent's CUSTOM name is not
/// carried for display, so a viewer in `name` mode sees the harness where the
/// server itself shows the name. The name still resolves for addressing
/// (`agent get`, `agent send`) — it just is not what a neighbour draws. That is
/// the bargain `server_icons` makes too, where only a name crosses the wire and
/// the glyph is the receiver's to choose.
fn agent_label_on_the_wire(detail: &crate::workspace::PaneDetail) -> String {
    detail
        .agent
        .map(|agent| crate::detect::agent_label(agent).to_string())
        .unwrap_or_else(|| detail.agent_label.clone())
}

#[cfg(test)]
mod tests {
    use crate::app::state::PeerSwitchRequest;
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

    fn summary(name: &str, ssh_target: &str) -> crate::peers::PeerSummaryState {
        crate::peers::PeerSummaryState {
            node_id: None,
            dial: Default::default(),
            stream_error: None,
            peer: name.to_string(),
            ssh_target: ssh_target.to_string(),
            host: Some(name.to_string()),
            version: None,
            protocol: None,
            system: None,
            latency_ms: Some(10),
            // Deliberately empty: prepare_peer_switch must not spawn the
            // remote pre-focus ssh in tests.
            workspaces: Vec::new(),
            last_ok: Some(std::time::Instant::now()),
            error: None,
            origin_last_ok_secs: None,
            ingested_at: None,
            proxy_jump: None,
            icon: None,
        }
    }

    fn carried_snapshot() -> crate::peers::FleetSnapshotState {
        crate::peers::FleetSnapshotState {
            origin: "hopper".to_string(),
            peers: vec![
                summary("kiln", "operator@kiln"),
                summary("node-b", "operator@node-b"),
            ],
            origin_summary: None,
            received_at: std::time::Instant::now(),
        }
    }

    #[tokio::test]
    async fn checkout_prepare_unknown_workspace_is_rejected() {
        let mut app = test_app();
        let response = app.handle_peers_checkout_prepare(
            "req".into(),
            crate::api::schema::PeersCheckoutPrepareParams {
                workspace_id: "ws_nope".into(),
                push: false,
            },
        );
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(value["error"]["code"], "workspace_not_found");
    }

    #[tokio::test]
    async fn home_request_resolves_to_reserved_target_without_fleet() {
        let mut app = test_app();
        app.state.fleet_snapshot = Some(carried_snapshot());

        let prepared = app
            .prepare_switch_server(PeerSwitchRequest::Home)
            .expect("home resolves when an origin was carried");
        assert_eq!(prepared.ssh_target, crate::protocol::HOME_SWITCH_TARGET);
        assert!(prepared.label.contains("hopper"));
        // Going home carries nothing: the local server needs no snapshot.
        assert!(prepared.fleet.is_none());
    }

    #[tokio::test]
    async fn home_request_without_origin_resolves_to_none() {
        let mut app = test_app();
        assert!(app.prepare_switch_server(PeerSwitchRequest::Home).is_none());
    }

    #[tokio::test]
    async fn snapshot_row_switch_passes_snapshot_through_with_original_origin() {
        let mut app = test_app();
        app.state.fleet_snapshot = Some(carried_snapshot());

        let prepared = app
            .prepare_switch_server(PeerSwitchRequest::SnapshotPeer {
                entry_idx: 0,
                ws_idx: None,
            })
            .expect("snapshot row resolves");
        assert_eq!(prepared.ssh_target, "operator@kiln");
        let fleet = prepared.fleet.expect("nested leap carries the snapshot");
        // Pass-through, not re-stamp: the ORIGINAL origin survives, and the
        // hop target drops out (it becomes the self row over there).
        assert_eq!(fleet.origin, "hopper");
        let targets: Vec<&str> = fleet
            .peers
            .iter()
            .map(|peer| peer.ssh_target.as_str())
            .collect();
        assert!(
            targets.contains(&"operator@node-b"),
            "carried peers survive: {targets:?}"
        );
        assert!(
            !targets.contains(&"operator@kiln"),
            "hop target excluded: {targets:?}"
        );
        // #80: the leg also carries THIS server, so the next hop can see the
        // machine the client just came through. Nothing else in the chain
        // holds it — the origin slot belongs to home, and the hub that sent us
        // this snapshot excluded us from it.
        assert!(
            fleet
                .peers
                .iter()
                .any(|peer| peer.host.as_deref() == Some(crate::app::short_host_name().as_str())),
            "the forwarded fleet must include the server forwarding it: {targets:?}"
        );
    }

    #[tokio::test]
    async fn config_peer_switch_from_hub_stamps_own_origin_and_peers() {
        let mut app = test_app();
        app.state.peer_summaries = vec![
            summary("kiln", "operator@kiln"),
            summary("spoke2.invalid", "operator@spoke2.invalid"),
        ];

        let prepared = app
            .prepare_switch_server(PeerSwitchRequest::ConfigPeer {
                peer_idx: 1,
                ws_idx: Some(0),
            })
            .expect("config peer resolves");
        assert_eq!(prepared.ssh_target, "operator@spoke2.invalid");
        let fleet = prepared.fleet.expect("hub leap stamps a fresh snapshot");
        assert_eq!(fleet.origin, crate::app::short_host_name());
        // The hop target is excluded from its own snapshot.
        assert_eq!(fleet.peers.len(), 1);
        assert_eq!(fleet.peers[0].ssh_target, "operator@kiln");
        // The hub stamps its OWN summary so a spoke sees the way-home spaces
        // (#66): home-targeted, never an ssh dial.
        let origin = fleet.origin_summary.expect("hub stamps its own summary");
        assert_eq!(origin.ssh_target, crate::protocol::HOME_SWITCH_TARGET);
        assert_eq!(
            origin.host.as_deref(),
            Some(crate::app::short_host_name()).as_deref()
        );
    }

    #[tokio::test]
    async fn origin_workspace_switch_lands_home_with_focus_target() {
        let mut app = test_app();
        let mut origin = summary("hopper", crate::protocol::HOME_SWITCH_TARGET);
        origin.workspaces = vec![crate::api::schema::PeerWorkspaceSummary {
            id: "ws_7".to_string(),
            workspace: "keyboard-shorcuts".to_string(),
            project_key: Some("github.com/gerchowl/flock".to_string()),
            project_label: Some("flock".to_string()),
            branch: Some("keyboard-shorcuts".to_string()),
            is_linked_worktree: true,
            agent: Some("cc".to_string()),
            status: crate::api::schema::AgentStatus::Working,
            status_age_secs: Some(4),
            activity: None,
            agents: Vec::new(),
        }];
        let mut snapshot = carried_snapshot();
        snapshot.origin_summary = Some(origin);
        app.state.fleet_snapshot = Some(snapshot);

        let prepared = app
            .prepare_switch_server(PeerSwitchRequest::OriginWorkspace { ws_idx: 0 })
            .expect("origin workspace resolves");
        // The way home is the sentinel, never an ssh dial (a spoke has no
        // route to the hub), and the chosen workspace rides along to focus.
        assert_eq!(prepared.ssh_target, crate::protocol::HOME_SWITCH_TARGET);
        assert!(prepared.fleet.is_none());
        assert_eq!(prepared.focus_workspace.as_deref(), Some("ws_7"));
        assert!(prepared.label.contains("keyboard-shorcuts"));
    }

    #[tokio::test]
    async fn stale_snapshot_row_index_resolves_to_none() {
        let mut app = test_app();
        app.state.fleet_snapshot = Some(carried_snapshot());
        assert!(app
            .prepare_switch_server(PeerSwitchRequest::SnapshotPeer {
                entry_idx: 99,
                ws_idx: None,
            })
            .is_none());
    }

    #[tokio::test]
    async fn outgoing_fleet_snapshot_from_hub_merges_relayed_cache_into_wire() {
        // Gossip v3 (#101) part 1 (RED): hub polls kiln, kiln relays twohop
        // (twohop lives one hop past kiln). The fixture host is deliberately
        // not a real machine name: an entry about the host RUNNING the test is
        // dropped as "that's us", so a peer named after the developer's box
        // failed here for reasons that had nothing to do with the relay. kiln's spoke1 attaches to hub —
        // hub's outgoing_fleet_snapshot must include atlas in its `peers`
        // vector so the FULL fleet is visible on spoke1. Without the relay
        // merge this test fails (only kiln appears).
        let mut app = test_app();
        app.state.peer_summaries = vec![summary("kiln", "operator@kiln")];
        app.state.relayed_fleet_cache.insert(
            "spoke2.invalid".to_string(),
            crate::peers::relayed_entry_from_wire(crate::api::schema::RelayedFleetPeer {
                node_id: None,
                dial: None,
                name: "spoke2.invalid".into(),
                ssh_target: "operator@spoke2.invalid".into(),
                host: Some("spoke2.invalid".into()),
                version: Some("0.9.0".into()),
                protocol: None,
                system: None,
                latency_ms: Some(12),
                workspaces: Vec::new(),
                age_secs: Some(4),
                error: None,
                origin: "kiln".into(),
                origin_last_ok_secs: Some(4),
                proxy_jump: Some("kiln".into()),
                icon: None,
            })
            .map(|mut entry| {
                // Relayed by the kiln we poll, as a real poll merge records.
                entry.via = Some("kiln".into());
                entry
            })
            .expect("fixture destination is a valid ssh target"),
        );

        let prepared = app
            .prepare_switch_server(PeerSwitchRequest::ConfigPeer {
                peer_idx: 0,
                ws_idx: Some(0),
            })
            .expect("hub stamps a snapshot on switch to kiln");
        let fleet = prepared.fleet.expect("hub leap carries a snapshot");
        // kiln is the hop target — dropped. atlas rides through as a
        // relayed row, so a spoke1 attaching to kiln sees the fleet.
        let targets: Vec<&str> = fleet
            .peers
            .iter()
            .map(|peer| peer.ssh_target.as_str())
            .collect();
        assert!(
            targets.contains(&"operator@spoke2.invalid"),
            "relayed peer must ride the wire: {targets:?}"
        );
        assert!(
            !targets.contains(&"operator@kiln"),
            "hop target excluded: {targets:?}"
        );
    }

    /// A row `kiln` relays about `node-b`, with the target only kiln resolves.
    fn relayed_by_anvil(name: &str, ssh_target: &str) -> crate::api::schema::RelayedFleetPeer {
        crate::api::schema::RelayedFleetPeer {
            node_id: None,
            dial: None,
            name: name.into(),
            ssh_target: ssh_target.into(),
            host: Some(name.into()),
            version: None,
            protocol: None,
            system: None,
            latency_ms: None,
            workspaces: Vec::new(),
            age_secs: Some(3),
            error: None,
            origin: "kiln.invalid".into(),
            origin_last_ok_secs: Some(3),
            proxy_jump: Some("kiln.invalid".into()),
            icon: None,
        }
    }

    fn snapshot_jump<'a>(fleet: &'a crate::protocol::FleetSnapshot, name: &str) -> Option<&'a str> {
        fleet
            .peers
            .iter()
            .find(|peer| peer.name == name)
            .unwrap_or_else(|| panic!("{name} rides the snapshot: {:?}", fleet.peers))
            .proxy_jump
            .as_deref()
    }

    #[tokio::test]
    async fn a_relayed_row_routes_via_its_relayer_and_a_polled_one_via_the_hub() {
        // #441: the hub stamped EVERY row with itself, so node-b (which only
        // kiln can resolve as `ws00860001`) became `ssh -J hopper ws00860001`.
        let us = crate::app::short_host_name();
        let mut app = test_app();
        app.state.peer_summaries = vec![
            summary("kiln.invalid", "operator@kiln.tailnet"),
            summary("spoke1.invalid", "operator@spoke1.invalid"),
        ];
        crate::peers::merge_relayed_fleet(
            &mut app.state.relayed_fleet_cache,
            vec![relayed_by_anvil("node-b.invalid", "ws00860001")],
            "kiln.invalid",
        );

        let prepared = app
            .prepare_switch_server(PeerSwitchRequest::ConfigPeer {
                peer_idx: 1,
                ws_idx: None,
            })
            .expect("switch to spoke1");
        let fleet = prepared.fleet.expect("a snapshot rides the leg");
        let chain = format!("{us},operator@kiln.tailnet");
        assert_eq!(snapshot_jump(&fleet, "kiln.invalid"), Some(us.as_str()));
        assert_eq!(
            snapshot_jump(&fleet, "node-b.invalid"),
            Some(chain.as_str())
        );

        // The hub's own click on the relayed row takes the same route.
        let direct = app
            .prepare_switch_server(PeerSwitchRequest::RelayedPeer {
                host_key: "node-b.invalid".into(),
                ws_idx: None,
            })
            .expect("a relayed row is dialable");
        assert_eq!(direct.ssh_target, "ws00860001");
        assert_eq!(direct.proxy_jump.as_deref(), Some(chain.as_str()));

        // Dialled from the hub itself: the polled peer goes direct, the
        // relayed one via the relayer only. From anywhere else, both hops.
        let from_hub = |jump: Option<&str>| {
            jump.and_then(|j| crate::remote::relative_proxy_jump(j, &[us.as_str()]))
        };
        assert_eq!(from_hub(snapshot_jump(&fleet, "kiln.invalid")), None);
        assert_eq!(
            from_hub(snapshot_jump(&fleet, "node-b.invalid")).as_deref(),
            Some("operator@kiln.tailnet")
        );
        assert_eq!(
            crate::remote::relative_proxy_jump(&chain, &["hopper.invalid"]).as_deref(),
            Some(chain.as_str())
        );
    }

    #[tokio::test]
    async fn a_relayed_row_whose_relayer_left_config_is_display_only() {
        // The relay cache outlives a `[[peers]]` edit until eviction. A relayer
        // the operator removed must not keep choosing a jump host: the row is
        // neither dialled nor carried to a server that would dial it.
        let mut app = test_app();
        app.state.peer_summaries = vec![summary("spoke1.invalid", "operator@spoke1.invalid")];
        crate::peers::merge_relayed_fleet(
            &mut app.state.relayed_fleet_cache,
            vec![relayed_by_anvil("node-b.invalid", "ws00860001")],
            "kiln.invalid",
        );
        assert!(app
            .prepare_switch_server(PeerSwitchRequest::RelayedPeer {
                host_key: "node-b.invalid".into(),
                ws_idx: None,
            })
            .is_none());
        let fleet = app
            .prepare_switch_server(PeerSwitchRequest::ConfigPeer {
                peer_idx: 0,
                ws_idx: None,
            })
            .and_then(|prepared| prepared.fleet)
            .expect("a snapshot rides the leg");
        assert!(
            fleet.peers.iter().all(|peer| peer.name != "node-b.invalid"),
            "{:?}",
            fleet.peers
        );
    }

    #[tokio::test]
    async fn a_hub_pushed_row_still_cannot_be_dialled_or_carried() {
        // #410/#425: the route change must not widen what a push can make a
        // client dial. A pushed row with no carried route stays display-only,
        // and never rides an outgoing snapshot where it WOULD be dialled.
        let mut app = test_app();
        app.state.peer_summaries = vec![summary("spoke1.invalid", "operator@spoke1.invalid")];
        crate::peers::merge_hub_pushed_fleet(
            &mut app.state.relayed_fleet_cache,
            vec![hub_row("node-b.invalid", "ws00860001", 0)],
            "hopper",
        );
        assert!(app.state.relayed_fleet_cache["node-b.invalid"].hub_pushed);
        assert!(app
            .prepare_switch_server(PeerSwitchRequest::RelayedPeer {
                host_key: "node-b.invalid".into(),
                ws_idx: None,
            })
            .is_none());
        let fleet = app
            .prepare_switch_server(PeerSwitchRequest::ConfigPeer {
                peer_idx: 0,
                ws_idx: None,
            })
            .and_then(|prepared| prepared.fleet)
            .expect("a snapshot rides the leg");
        assert!(
            fleet.peers.iter().all(|peer| peer.name != "node-b.invalid"),
            "{:?}",
            fleet.peers
        );
    }

    #[tokio::test]
    async fn a_dial_error_leaves_as_a_token_not_ssh_text() {
        // #428: `error` crossed to every poller and spoke as ssh's stderr
        // line, host names and ports included. Only the token leaves now.
        let mut app = test_app();
        let mut failing = summary("spoke1.invalid", "operator@spoke1.invalid");
        failing.error =
            Some("ssh: connect to host atlas.internal port 22: Connection refused".into());
        app.state.peer_summaries = vec![failing, summary("kiln", "operator@kiln")];

        let row = app.own_relayed_fleet().remove(0);
        assert_eq!(row.error.as_deref(), Some("connect_refused"));

        let fleet = app
            .prepare_switch_server(PeerSwitchRequest::ConfigPeer {
                peer_idx: 1,
                ws_idx: None,
            })
            .and_then(|prepared| prepared.fleet)
            .expect("a snapshot rides the leg");
        let carried = fleet
            .peers
            .iter()
            .find(|peer| peer.name == "spoke1.invalid")
            .unwrap();
        assert_eq!(carried.error.as_deref(), Some("connect_refused"));

        // And the receiver still reads the reason out of it.
        let received = crate::peers::peer_from_wire(carried.clone());
        assert_eq!(
            received.shown_failure_reason(),
            Some(crate::peers::SshFailureReason::ConnectRefused)
        );
    }

    fn hub_row(name: &str, ssh_target: &str, age: u64) -> crate::api::schema::RelayedFleetPeer {
        crate::api::schema::RelayedFleetPeer {
            node_id: None,
            dial: None,
            name: name.into(),
            ssh_target: ssh_target.into(),
            host: Some(name.into()),
            version: None,
            protocol: None,
            system: None,
            latency_ms: None,
            workspaces: Vec::new(),
            age_secs: Some(age),
            error: None,
            origin: "hub".into(),
            origin_last_ok_secs: Some(age),
            proxy_jump: Some("hub".into()),
            icon: None,
        }
    }

    #[tokio::test]
    async fn a_spoke_learns_the_fleet_its_hub_pushes_down_and_routes_it_via_the_hub() {
        // #410: a spoke polls nobody, so gossip — which only flowed UP — left
        // it knowing nothing past itself. The hub's push is merged through the
        // same validated, freshest-wins path a poller uses.
        let mut app = test_app();
        // Only an enrolled edge may push (#416 review),
        // this live test process stands in for it.
        let relay = std::process::id();
        let started = crate::platform::process_start_time(relay).expect("own start time");
        app.inbound
            .attach(relay, started, crate::platform::process_start_time)
            .expect("bound");
        enroll_test_edge(&mut app, "hub");
        app.current_api_peer_pid = Some(relay);
        let response = app.handle_api_request(crate::api::schema::Request {
            id: "down".into(),
            method: crate::api::schema::Method::PeersHubFleet(
                crate::api::schema::PeersHubFleetParams {
                    hub: "hub".into(),
                    fleet: vec![
                        hub_row("node-b", "operator@node-b", 2),
                        // #392: a row this host will not dial is dropped.
                        hub_row("evil", "-oProxyCommand=touch /tmp/x", 1),
                        // A row about this server itself is never stored.
                        hub_row(&crate::app::short_host_name(), "self", 1),
                    ],
                    hub_self: None,
                },
            ),
        });
        assert!(response.contains("\"result\""), "{response}");
        // A later push naming a different hub is heard under the FIRST name.
        let _ = app.handle_api_request(crate::api::schema::Request {
            id: "down2".into(),
            method: crate::api::schema::Method::PeersHubFleet(
                crate::api::schema::PeersHubFleetParams {
                    hub: "impostor".into(),
                    fleet: vec![hub_row("node-b", "operator@node-b", 1)],
                    hub_self: None,
                },
            ),
        });
        app.current_api_peer_pid = None;

        let keys: Vec<&String> = app.state.relayed_fleet_cache.keys().collect();
        assert_eq!(
            keys,
            vec!["node-b"],
            "only the valid, foreign row: {keys:?}"
        );
        let entry = app.state.relayed_fleet_cache["node-b"].clone();
        assert_eq!(entry.via.as_deref(), Some("hub"), "routed via the hub");
        assert!(entry.hub_pushed, "display-only");
        assert_eq!(
            entry.peer.proxy_jump.as_deref(),
            Some("hub"),
            "nothing about where it would be dialled comes from the row"
        );
        // Display-only: clicking it dials nothing, whatever its ssh target says.
        assert!(
            app.prepare_switch_server(crate::app::state::PeerSwitchRequest::RelayedPeer {
                host_key: "node-b".into(),
                ws_idx: None,
            })
            .is_none(),
            "a hub-pushed row is never switched to"
        );
        // Nor carried on: the snapshot the next attach leg takes — which that
        // server dials on a click and warms with no click — leaves it out.
        let snapshot = app.outgoing_fleet_snapshot("operator@elsewhere");
        assert!(
            snapshot
                .peers
                .iter()
                .all(|peer| peer.ssh_target != "operator@node-b" && peer.name != "node-b"),
            "a hub-pushed row never crosses a leap: {:?}",
            snapshot
                .peers
                .iter()
                .map(|peer| &peer.name)
                .collect::<Vec<_>>()
        );
        // So the next server's warm slots — which dial with no click — never
        // see it: fed exactly the way the client feeds them.
        let carried: Vec<String> = snapshot
            .peers
            .iter()
            .map(|peer| peer.ssh_target.clone())
            .collect();
        let warmed = crate::client::slots::warm_all_targets(&[], &carried, 8);
        assert!(
            !format!("{warmed:?}").contains("operator@node-b"),
            "never warm-dialled downstream: {warmed:?}"
        );

        // Honest freshness: once the hub stops pushing, the row goes stale on
        // its own clock rather than looking live forever.
        let stale_after = app.state.config.gossip.stale_after().as_secs();
        let later = std::time::Instant::now() + std::time::Duration::from_secs(stale_after + 1);
        assert!(
            entry.peer.is_stale_at(later, stale_after),
            "a row nobody refreshes ages into stale"
        );

        // Aged OUT, too, not just stale: the spoke's own tick evicts it once no
        // push has refreshed it for the relay TTL.
        let ttl = stale_after * crate::app::state::RELAYED_ENTRY_TTL_STALE_MULTIPLE;
        app.state.relayed_fleet_cache.insert(
            "old".into(),
            crate::peers::relayed_entry_from_wire(hub_row("old", "operator@old", ttl + 5))
                .expect("valid"),
        );
        app.expire_relayed_entries();
        assert!(
            !app.state.relayed_fleet_cache.contains_key("old"),
            "an expired row is removed on the tick"
        );

        // And the hub's view never comes back up as this server's own.
        assert!(
            app.own_relayed_fleet().is_empty(),
            "a spoke with no peers relays nothing, whatever its hub told it"
        );
    }

    #[tokio::test]
    async fn own_relayed_fleet_never_includes_relayed_cache_one_hop_only() {
        // Loop prevention (#101 part 1): a hub's OWN peers.summary response
        // only relays its DIRECTLY polled peers, never entries received via
        // relay. Result: an entry travels exactly one hop, breaking the
        // ping-pong you'd get if two hubs both re-relayed each other's rows.
        let mut app = test_app();
        app.state.peer_summaries = vec![summary("kiln", "operator@kiln")];
        // Simulate kiln having relayed atlas to us on a prior poll.
        app.state.relayed_fleet_cache.insert(
            "spoke2.invalid".to_string(),
            crate::peers::relayed_entry_from_wire(crate::api::schema::RelayedFleetPeer {
                node_id: None,
                dial: None,
                name: "spoke2.invalid".into(),
                ssh_target: "operator@spoke2.invalid".into(),
                host: Some("spoke2.invalid".into()),
                version: None,
                protocol: None,
                system: None,
                latency_ms: None,
                workspaces: Vec::new(),
                age_secs: Some(3),
                error: None,
                origin: "kiln".into(),
                origin_last_ok_secs: Some(3),
                proxy_jump: Some("kiln".into()),
                icon: None,
            })
            .expect("fixture destination is a valid ssh target"),
        );

        let entries = app.own_relayed_fleet();
        // Only kiln (our own polled peer) — atlas was received via relay and
        // must NOT ride our outgoing summary.
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["kiln"],
            "relayed cache must not re-relay: {names:?}"
        );
        assert_eq!(entries[0].origin, crate::app::short_host_name());
    }

    #[tokio::test]
    async fn relay_merge_drops_entries_whose_origin_is_self() {
        // Loop prevention (#101 part 1): if a peer relays entries whose
        // origin is our OWN short host, we drop them. That's the only cycle
        // the one-hop rule can't close on its own — a peer that received a
        // relay from us and echoed it back on its next summary.
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = crate::config::Config::default();
        config.peers = vec![crate::config::PeerConfig {
            name: "kiln".into(),
            ..Default::default()
        }];
        let mut app = App::new(&config, true, None, api_rx, crate::api::EventHub::default());
        let self_host = crate::app::short_host_name();

        app.handle_internal_event(crate::events::AppEvent::PeerSummaryFetched(
            crate::peers::PeerSummaryFetch {
                peer: "kiln".into(),
                stream_error: None,
                result: Ok(crate::peers::PeerSummaryPayload {
                    node_id: None,
                    outbound_pending: false,
                    host: "kiln".into(),
                    version: None,
                    protocol: None,
                    system: None,
                    latency_ms: 5,
                    workspaces: Vec::new(),
                    relayed_fleet: vec![crate::api::schema::RelayedFleetPeer {
                        node_id: None,
                        dial: None,
                        name: "loop-back".into(),
                        ssh_target: "operator@loop".into(),
                        host: Some("loop-back".into()), // guardrails-ok(fixture): a ProxyJump chain hop, not a fleet member
                        version: None,
                        protocol: None,
                        system: None,
                        latency_ms: None,
                        workspaces: Vec::new(),
                        age_secs: Some(1),
                        error: None,
                        // This is us: a kiln that received our relay and
                        // echoed us as its own origin. Must be dropped.
                        origin: self_host.clone(),
                        origin_last_ok_secs: Some(1),
                        proxy_jump: Some(self_host.clone()),
                        icon: None,
                    }],
                    icon: None,
                }),
            },
        ));

        assert!(
            app.state.relayed_fleet_cache.is_empty(),
            "an entry whose origin is self must never enter the cache: {:?}",
            app.state.relayed_fleet_cache
        );
    }

    /// A workspace with a resolved git identity, the way the git-space cache
    /// holds it once a checkout has been probed.
    fn git_workspace(name: &str, project_key: &str) -> crate::workspace::Workspace {
        let mut ws = crate::workspace::Workspace::test_new(name);
        ws.cached_git_space = Some(crate::workspace::GitSpaceMetadata {
            key: format!("/repos/{name}/.git"),
            checkout_key: format!("/repos/{name}"),
            label: name.to_string(),
            repo_root: std::path::PathBuf::from(format!("/repos/{name}")),
            is_linked_worktree: false,
            project_key: project_key.to_string(),
        });
        ws
    }

    fn enroll_test_edge(app: &mut App, peer: &str) {
        let edge = app
            .inbound
            .edge_mut(
                Some(std::process::id()),
                crate::platform::process_start_time,
            )
            .unwrap();
        edge.enrollment.peer = peer.into();
        edge.enrollment.node_id = Some(format!("test-node-{peer}"));
        edge.enrollment.state = "pinned".into();
    }

    /// Bind this test process as the relay, as `mesh.hello` would.
    fn bind_relay(app: &mut App, peer: &str) {
        let relay = std::process::id();
        let started = crate::platform::process_start_time(relay).expect("own start time");
        app.inbound
            .attach(relay, started, crate::platform::process_start_time)
            .expect("bound");
        enroll_test_edge(app, peer);
        app.current_api_peer_pid = Some(relay);
    }

    fn push_down(spoke: &mut App, params: crate::api::schema::PeersHubFleetParams) {
        let response = spoke.handle_api_request(crate::api::schema::Request {
            id: "down".into(),
            method: crate::api::schema::Method::PeersHubFleet(params),
        });
        assert!(response.contains("\"result\""), "{response}");
    }

    /// Every space name the spoke's sidebar shows for the client's HOME.
    fn home_space_names(spoke: &App) -> Vec<String> {
        let origin = spoke
            .state
            .fleet_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.origin_summary.as_ref())
            .expect("carried origin");
        crate::ui::workspace_list_entries(&spoke.state)
            .into_iter()
            .filter_map(|entry| match entry {
                crate::ui::WorkspaceListEntry::Remote {
                    peer: crate::app::state::RemotePeerRef::Origin,
                    ws_idx,
                    ..
                } => Some(origin.workspaces[ws_idx].workspace.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_switch_away_from_the_hub_keeps_the_view_of_it_live() {
        // #424: the client switches A (the hub) → B (a spoke A polls). The
        // snapshot B receives was stamped once, at switch time, and nothing
        // refreshed it, so a space renamed or opened on A afterwards never
        // showed on B. A's own row now rides the push it already makes down the
        // relay it holds into B, on every poll.
        let mut hub = test_app();
        hub.state.workspaces = vec![
            git_workspace("flock", "github.com/gerchowl/flock"),
            git_workspace("notes", "github.com/gerchowl/notes"),
        ];
        hub.state.workspaces[0].custom_name = Some("flock".into());
        hub.state.peer_summaries = vec![summary("spoke", "operator@spoke")];
        let prepared = hub
            .prepare_switch_server(PeerSwitchRequest::ConfigPeer {
                peer_idx: 0,
                ws_idx: None,
            })
            .expect("switch resolves");

        let mut spoke = test_app();
        spoke.state.fleet_snapshot = Some(crate::peers::FleetSnapshotState::from_wire(
            prepared.fleet.expect("the hub stamps a snapshot"),
        ));
        assert_eq!(home_space_names(&spoke), vec!["flock", "notes"]);

        // On A, after the switch: rename a space, open a new solo one.
        hub.state.workspaces[0].custom_name = Some("flock-renamed".into());
        hub.state
            .workspaces
            .push(git_workspace("scratch", "github.com/gerchowl/scratch"));
        // Without a push B still shows the switch-time copy: the freeze.
        assert_eq!(home_space_names(&spoke), vec!["flock", "notes"]);

        // One refresh window: A's next poll of B pushes down, exactly what
        // `PeerPollDue` hands `push_hub_fleet`.
        bind_relay(&mut spoke, &crate::app::short_host_name());
        push_down(
            &mut spoke,
            crate::api::schema::PeersHubFleetParams {
                hub: crate::app::short_host_name(),
                fleet: hub.own_relayed_fleet(),
                hub_self: Some(Box::new(hub.hub_self_row())),
            },
        );
        spoke.current_api_peer_pid = None;

        assert_eq!(
            home_space_names(&spoke),
            vec!["flock-renamed", "notes", "scratch"],
            "B shows A's rename and A's new space"
        );
        let origin = spoke
            .state
            .fleet_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.origin_summary.as_ref())
            .unwrap();
        // Still the way HOME: never an ssh dial to wherever the push said.
        assert_eq!(origin.ssh_target, crate::protocol::HOME_SWITCH_TARGET);
        assert!(origin.proxy_jump.is_none());
        // And one row for the hub, not the home row plus a relayed copy of it.
        assert!(spoke.state.relayed_fleet_cache.is_empty());
    }

    fn planted_space(name: &str) -> crate::api::schema::PeerWorkspaceSummary {
        crate::api::schema::PeerWorkspaceSummary {
            id: "ws_9".into(),
            workspace: name.into(),
            project_key: Some("github.com/x/y".into()),
            project_label: None,
            branch: None,
            is_linked_worktree: false,
            agent: None,
            status: crate::api::schema::AgentStatus::Idle,
            status_age_secs: None,
            activity: None,
            agents: Vec::new(),
        }
    }

    fn carried_home() -> crate::peers::FleetSnapshotState {
        crate::peers::FleetSnapshotState {
            origin: "hopper".into(),
            peers: Vec::new(),
            origin_summary: Some(summary("hopper", crate::protocol::HOME_SWITCH_TARGET)),
            received_at: std::time::Instant::now(),
        }
    }

    fn home_workspaces(spoke: &App) -> Vec<String> {
        spoke
            .state
            .fleet_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.origin_summary.as_ref())
            .expect("carried origin")
            .workspaces
            .iter()
            .map(|ws| ws.workspace.clone())
            .collect()
    }

    #[tokio::test]
    async fn only_the_home_hub_itself_can_repaint_the_home_row() {
        // #425 review blocker: the home row is the one the operator trusts.
        // Any spoke the hub polls can REPORT any host, so a row in `fleet`
        // claiming to be home (fresh, age 0) must not replace it — and must
        // not be stored as a second home row either. Nor may a `hub_self` from
        // a hub that is not home, or one naming a machine other than the hub.
        let mut spoke = test_app();
        spoke.state.fleet_snapshot = Some(carried_home());
        let mut claim = hub_row("hopper", "hopper", 0);
        claim.workspaces = vec![planted_space("planted-by-a-spoke")];
        let mut forged_self = hub_row("hopper", "hopper", 0);
        forged_self.workspaces = vec![planted_space("planted-by-kiln")];
        bind_relay(&mut spoke, "kiln");
        // The bound hub is kiln, not home: its fleet row about hopper and its
        // `hub_self` naming hopper are both claims about someone else.
        push_down(
            &mut spoke,
            crate::api::schema::PeersHubFleetParams {
                hub: "kiln".into(),
                fleet: vec![claim.clone()],
                hub_self: Some(Box::new(forged_self)),
            },
        );
        spoke.current_api_peer_pid = None;
        assert!(
            home_workspaces(&spoke).is_empty(),
            "{:?}",
            home_workspaces(&spoke)
        );
        assert!(
            !spoke.state.relayed_fleet_cache.contains_key("hopper"),
            "a claim to be home is dropped, not stored"
        );

        // Even when the bound hub IS home, a `fleet` row about home is some
        // polled node's say-so. Only the hub's own `hub_self` counts.
        let mut spoke = test_app();
        spoke.state.fleet_snapshot = Some(carried_home());
        bind_relay(&mut spoke, "hopper");
        let mut genuine = hub_row("hopper", "hopper", 0);
        genuine.workspaces = vec![planted_space("home-for-real")];
        push_down(
            &mut spoke,
            crate::api::schema::PeersHubFleetParams {
                hub: "hopper".into(),
                fleet: vec![claim],
                hub_self: None,
            },
        );
        assert!(
            home_workspaces(&spoke).is_empty(),
            "{:?}",
            home_workspaces(&spoke)
        );
        push_down(
            &mut spoke,
            crate::api::schema::PeersHubFleetParams {
                hub: "hopper".into(),
                fleet: Vec::new(),
                hub_self: Some(Box::new(genuine)),
            },
        );
        spoke.current_api_peer_pid = None;
        assert_eq!(home_workspaces(&spoke), vec!["home-for-real"]);
        assert!(spoke.state.relayed_fleet_cache.is_empty());
    }

    #[tokio::test]
    async fn a_hub_that_is_not_home_shows_as_its_own_display_only_row() {
        // A spoke used to see its hub's peers but never the hub. When the hub
        // is not the client's home, its `hub_self` is its own row.
        let mut spoke = test_app();
        spoke.state.fleet_snapshot = Some(carried_home());
        bind_relay(&mut spoke, "kiln");
        push_down(
            &mut spoke,
            crate::api::schema::PeersHubFleetParams {
                hub: "kiln".into(),
                fleet: Vec::new(),
                hub_self: Some(Box::new(hub_row("kiln", "kiln", 0))),
            },
        );
        spoke.current_api_peer_pid = None;
        let entry = &spoke.state.relayed_fleet_cache["kiln"];
        assert!(entry.hub_pushed);
        assert!(home_workspaces(&spoke).is_empty());
        // Display-only: nothing was carried for kiln, so a click dials nothing.
        assert!(spoke
            .prepare_switch_server(PeerSwitchRequest::RelayedPeer {
                host_key: "kiln".into(),
                ws_idx: None,
            })
            .is_none());
    }

    #[tokio::test]
    async fn a_host_with_control_bytes_is_not_a_second_home() {
        // #425 review r2: identity is keyed on the raw host, so `hopper\x07`
        // got past `without_origin_claims` and rendered as a second hopper.
        let mut spoke = test_app();
        spoke.state.fleet_snapshot = Some(carried_home());
        bind_relay(&mut spoke, "kiln");
        let mut disguised = hub_row("hopper", "hopper", 0);
        disguised.host = Some("hopper\u{7}".into());
        push_down(
            &mut spoke,
            crate::api::schema::PeersHubFleetParams {
                hub: "kiln".into(),
                fleet: vec![disguised, hub_row("node-b", "node-b", 0)],
                hub_self: None,
            },
        );
        spoke.current_api_peer_pid = None;
        let keys: Vec<&String> = spoke.state.relayed_fleet_cache.keys().collect();
        assert_eq!(
            keys,
            vec!["node-b"],
            "the disguised row is dropped: {keys:?}"
        );
        assert!(home_workspaces(&spoke).is_empty());
    }

    #[tokio::test]
    async fn pushed_names_lose_control_bytes_before_they_render() {
        // #425 review: the hub_fleet receive path renders what it is handed.
        let mut spoke = test_app();
        spoke.state.fleet_snapshot = Some(carried_home());
        bind_relay(&mut spoke, "hopper");
        let mut row = hub_row("hopper", "hopper", 0);
        row.workspaces = vec![planted_space("ok\u{1b}]52;c;cGF5bG9hZA==\u{7}name")];
        push_down(
            &mut spoke,
            crate::api::schema::PeersHubFleetParams {
                hub: "hopper".into(),
                fleet: Vec::new(),
                hub_self: Some(Box::new(row)),
            },
        );
        spoke.current_api_peer_pid = None;
        let names = home_workspaces(&spoke);
        assert_eq!(names.len(), 1);
        assert!(
            !names[0].chars().any(char::is_control),
            "control bytes stripped: {:?}",
            names[0]
        );
    }

    #[tokio::test]
    async fn a_live_hub_pushed_row_still_switches_over_the_carried_route() {
        // #424: the hub's push makes a carried machine's row live, and the live
        // row wins the dedup. It is display-only, so before this the click that
        // worked on the frozen row silently did nothing. The route comes from
        // the carried row; the push supplies only which space to focus.
        let mut spoke = test_app();
        let mut carried = summary("kiln", "operator@kiln");
        carried.proxy_jump = Some("hopper".into());
        spoke.state.fleet_snapshot = Some(crate::peers::FleetSnapshotState {
            origin: "hopper".into(),
            peers: vec![carried],
            origin_summary: None,
            received_at: std::time::Instant::now(),
        });
        let mut live = hub_row("kiln", "-oProxyCommand=evil", 0);
        live.ssh_target = "kiln-elsewhere".into();
        live.workspaces = vec![crate::api::schema::PeerWorkspaceSummary {
            id: "ws_42".into(),
            workspace: "opened-after-the-switch".into(),
            project_key: Some("github.com/gerchowl/flock".into()),
            project_label: None,
            branch: Some("main".into()),
            is_linked_worktree: false,
            agent: None,
            status: crate::api::schema::AgentStatus::Idle,
            status_age_secs: None,
            activity: None,
            agents: Vec::new(),
        }];
        bind_relay(&mut spoke, "hopper");
        push_down(
            &mut spoke,
            crate::api::schema::PeersHubFleetParams {
                hub: "hopper".into(),
                fleet: vec![live],
                hub_self: None,
            },
        );
        spoke.current_api_peer_pid = None;
        assert!(spoke.state.relayed_fleet_cache["kiln"].hub_pushed);

        let prepared = spoke
            .prepare_switch_server(PeerSwitchRequest::RelayedPeer {
                host_key: "kiln".into(),
                ws_idx: Some(0),
            })
            .expect("the carried route makes the live row clickable");
        assert_eq!(prepared.ssh_target, "operator@kiln");
        assert_eq!(prepared.proxy_jump.as_deref(), Some("hopper"));
        assert_eq!(prepared.focus_workspace.as_deref(), Some("ws_42"));
    }

    #[tokio::test]
    async fn a_pushed_row_never_borrows_the_route_of_a_merely_similar_host() {
        // #425 review: `kiln.other` is not `kiln`. The domain-stripped sort
        // key must never decide whose route a click dials.
        let mut spoke = test_app();
        let mut carried = summary("kiln", "operator@kiln");
        carried.proxy_jump = Some("hopper".into());
        spoke.state.fleet_snapshot = Some(crate::peers::FleetSnapshotState {
            origin: "hopper".into(),
            peers: vec![carried],
            origin_summary: None,
            received_at: std::time::Instant::now(),
        });
        bind_relay(&mut spoke, "hopper");
        push_down(
            &mut spoke,
            crate::api::schema::PeersHubFleetParams {
                hub: "hopper".into(),
                fleet: vec![hub_row("kiln.other", "kiln.other", 0)],
                hub_self: None,
            },
        );
        spoke.current_api_peer_pid = None;
        assert!(spoke
            .prepare_switch_server(PeerSwitchRequest::RelayedPeer {
                host_key: "kiln.other".into(),
                ws_idx: None,
            })
            .is_none());
    }

    #[tokio::test]
    async fn a_hub_pushed_row_about_home_never_renders_home_twice() {
        // A row about the client's home stored before the client attached
        // (a spoke hears its hub whether or not anyone is attached) must not
        // stand beside the home row. The origin slot stands for that machine.
        let mut spoke = test_app();
        bind_relay(&mut spoke, "hopper");
        push_down(
            &mut spoke,
            crate::api::schema::PeersHubFleetParams {
                hub: "hopper".into(),
                fleet: Vec::new(),
                hub_self: Some(Box::new(hub_row("hopper", "hopper", 0))),
            },
        );
        spoke.current_api_peer_pid = None;
        assert!(spoke.state.relayed_fleet_cache.contains_key("hopper"));

        spoke.state.fleet_snapshot = Some(crate::peers::FleetSnapshotState {
            origin: "hopper".into(),
            peers: Vec::new(),
            origin_summary: Some(summary("hopper", crate::protocol::HOME_SWITCH_TARGET)),
            received_at: std::time::Instant::now(),
        });
        let rows: Vec<_> = spoke
            .state
            .remote_peers()
            .into_iter()
            .map(|(peer_ref, _)| peer_ref)
            .collect();
        assert_eq!(rows, vec![crate::app::state::RemotePeerRef::Origin]);
    }

    #[test]
    #[allow(clippy::disallowed_methods)] // Tests exec real git to prime fixtures.
    fn a_new_git_space_reports_its_project_before_the_git_cache_fills() {
        // #424 investigation, candidate (b): a remote row with no project key
        // is dropped from the spaces list, so a space whose summary was built
        // before its git identity resolved would vanish on every other server.
        // For a git checkout it cannot: the summary derives the identity live
        // from the checkout while the cache is empty.
        let dir = std::env::temp_dir().join(format!(
            "flock-424-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec![
                "remote",
                "add",
                "origin",
                "git@github.com:gerchowl/flock.git",
            ],
        ] {
            let status = std::process::Command::new("git")
                .args(&args)
                .current_dir(&dir)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        }
        let mut ws = crate::workspace::Workspace::test_new("main");
        ws.identity_cwd = dir.clone();
        ws.cached_git_space = None;
        let summary = super::workspace_peer_summary(&ws, &std::collections::HashMap::new());
        assert_eq!(
            summary.project_key.as_deref(),
            Some("github.com/gerchowl/flock")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
