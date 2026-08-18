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
    fn own_relayed_fleet(&self) -> Vec<RelayedFleetPeer> {
        let origin = short_host_name();
        self.state
            .peer_summaries
            .iter()
            .map(|peer| {
                let age_secs = peer.last_ok.map(|at| at.elapsed().as_secs());
                RelayedFleetPeer {
                    name: peer.peer.clone(),
                    ssh_target: peer.ssh_target.clone(),
                    host: peer.host.clone(),
                    version: peer.version.clone(),
                    protocol: peer.protocol,
                    system: peer.system.clone(),
                    latency_ms: peer.latency_ms,
                    workspaces: peer.workspaces.clone(),
                    age_secs,
                    error: peer.error.clone(),
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
                let entry = &self.state.relayed_fleet_cache.get(&host_key)?.peer;
                let ssh_target = entry.ssh_target.clone();
                let name = entry.host.clone().unwrap_or_else(|| entry.peer.clone());
                let target = ws_idx.and_then(|ws_idx| entry.workspaces.get(ws_idx));
                let label = switch_label(&name, target);
                let focus_workspace = focus_target(target);
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
        // peer we contribute — the client's next-leg bridge uses it as
        // `-o ProxyJump=<us>` to reach peers only routable through this hub.
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
        let mut relayed: Vec<_> = self.state.relayed_fleet_cache.iter().collect();
        relayed.sort_by_key(|(host_key, _)| *host_key);
        ours.extend(
            relayed
                .into_iter()
                .map(|(_, entry)| relayed_peer_to_wire(entry))
                .map(stamp_proxy_jump),
        );

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
                Some(crate::detect::short_agent_label(&detail.agent_label).to_string()),
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
                    agent: Some(crate::detect::short_agent_label(&detail.agent_label).to_string()),
                    status: super::super::api_helpers::pane_agent_status(detail.state, detail.seen),
                })
            })
            .collect(),
    }
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
            origin: "mba22".to_string(),
            peers: vec![summary("anvil", "lars@anvil"), summary("ksb", "lars@ksb")],
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
        assert!(prepared.label.contains("mba22"));
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
        assert_eq!(prepared.ssh_target, "lars@anvil");
        let fleet = prepared.fleet.expect("nested leap carries the snapshot");
        // Pass-through, not re-stamp: the ORIGINAL origin survives, and the
        // hop target drops out (it becomes the self row over there).
        assert_eq!(fleet.origin, "mba22");
        let targets: Vec<&str> = fleet
            .peers
            .iter()
            .map(|peer| peer.ssh_target.as_str())
            .collect();
        assert!(
            targets.contains(&"lars@ksb"),
            "carried peers survive: {targets:?}"
        );
        assert!(
            !targets.contains(&"lars@anvil"),
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
            summary("anvil", "lars@anvil"),
            summary("spoke2.invalid", "lars@spoke2.invalid"),
        ];

        let prepared = app
            .prepare_switch_server(PeerSwitchRequest::ConfigPeer {
                peer_idx: 1,
                ws_idx: Some(0),
            })
            .expect("config peer resolves");
        assert_eq!(prepared.ssh_target, "lars@spoke2.invalid");
        let fleet = prepared.fleet.expect("hub leap stamps a fresh snapshot");
        assert_eq!(fleet.origin, crate::app::short_host_name());
        // The hop target is excluded from its own snapshot.
        assert_eq!(fleet.peers.len(), 1);
        assert_eq!(fleet.peers[0].ssh_target, "lars@anvil");
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
        let mut origin = summary("mba22", crate::protocol::HOME_SWITCH_TARGET);
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
        // Gossip v3 (#101) part 1 (RED): hub polls anvil, anvil relays twohop
        // (twohop lives one hop past anvil). The fixture host is deliberately
        // not a real machine name: an entry about the host RUNNING the test is
        // dropped as "that's us", so a peer named after the developer's box
        // failed here for reasons that had nothing to do with the relay. anvil's spoke1 attaches to hub —
        // hub's outgoing_fleet_snapshot must include sage in its `peers`
        // vector so the FULL fleet is visible on spoke1. Without the relay
        // merge this test fails (only anvil appears).
        let mut app = test_app();
        app.state.peer_summaries = vec![summary("anvil", "lars@anvil")];
        app.state.relayed_fleet_cache.insert(
            "spoke2.invalid".to_string(),
            crate::peers::relayed_entry_from_wire(crate::api::schema::RelayedFleetPeer {
                name: "spoke2.invalid".into(),
                ssh_target: "lars@spoke2.invalid".into(),
                host: Some("spoke2.invalid".into()),
                version: Some("0.9.0".into()),
                protocol: None,
                system: None,
                latency_ms: Some(12),
                workspaces: Vec::new(),
                age_secs: Some(4),
                error: None,
                origin: "anvil".into(),
                origin_last_ok_secs: Some(4),
                proxy_jump: Some("anvil".into()),
                icon: None,
            }),
        );

        let prepared = app
            .prepare_switch_server(PeerSwitchRequest::ConfigPeer {
                peer_idx: 0,
                ws_idx: Some(0),
            })
            .expect("hub stamps a snapshot on switch to anvil");
        let fleet = prepared.fleet.expect("hub leap carries a snapshot");
        // anvil is the hop target — dropped. sage rides through as a
        // relayed row, so a spoke1 attaching to anvil sees the fleet.
        let targets: Vec<&str> = fleet
            .peers
            .iter()
            .map(|peer| peer.ssh_target.as_str())
            .collect();
        assert!(
            targets.contains(&"lars@spoke2.invalid"),
            "relayed peer must ride the wire: {targets:?}"
        );
        assert!(
            !targets.contains(&"lars@anvil"),
            "hop target excluded: {targets:?}"
        );
    }

    #[tokio::test]
    async fn own_relayed_fleet_never_includes_relayed_cache_one_hop_only() {
        // Loop prevention (#101 part 1): a hub's OWN peers.summary response
        // only relays its DIRECTLY polled peers, never entries received via
        // relay. Result: an entry travels exactly one hop, breaking the
        // ping-pong you'd get if two hubs both re-relayed each other's rows.
        let mut app = test_app();
        app.state.peer_summaries = vec![summary("anvil", "lars@anvil")];
        // Simulate anvil having relayed sage to us on a prior poll.
        app.state.relayed_fleet_cache.insert(
            "spoke2.invalid".to_string(),
            crate::peers::relayed_entry_from_wire(crate::api::schema::RelayedFleetPeer {
                name: "spoke2.invalid".into(),
                ssh_target: "lars@spoke2.invalid".into(),
                host: Some("spoke2.invalid".into()),
                version: None,
                protocol: None,
                system: None,
                latency_ms: None,
                workspaces: Vec::new(),
                age_secs: Some(3),
                error: None,
                origin: "anvil".into(),
                origin_last_ok_secs: Some(3),
                proxy_jump: Some("anvil".into()),
                icon: None,
            }),
        );

        let entries = app.own_relayed_fleet();
        // Only anvil (our own polled peer) — sage was received via relay and
        // must NOT ride our outgoing summary.
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["anvil"],
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
            name: "anvil".into(),
            ..Default::default()
        }];
        let mut app = App::new(&config, true, None, api_rx, crate::api::EventHub::default());
        let self_host = crate::app::short_host_name();

        app.handle_internal_event(crate::events::AppEvent::PeerSummaryFetched(
            crate::peers::PeerSummaryFetch {
                peer: "anvil".into(),
                result: Ok(crate::peers::PeerSummaryPayload {
                    host: "anvil".into(),
                    version: None,
                    protocol: None,
                    system: None,
                    latency_ms: 5,
                    workspaces: Vec::new(),
                    relayed_fleet: vec![crate::api::schema::RelayedFleetPeer {
                        name: "loop-back".into(),
                        ssh_target: "lars@loop".into(),
                        host: Some("loop-back".into()),
                        version: None,
                        protocol: None,
                        system: None,
                        latency_ms: None,
                        workspaces: Vec::new(),
                        age_secs: Some(1),
                        error: None,
                        // This is us: an anvil that received our relay and
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
}
