use super::App;

impl App {
    /// Preserve socket attribution across queued or parked requests and lease release.
    pub(super) fn lease_caller(&self) -> crate::structured::lease::Caller {
        use crate::structured::identity::Principal;
        let principal = self
            .current_api_peer_uid
            .map(|uid| self.runtime_config.uid_policy.classify(uid));
        if principal == Some(Principal::Unknown)
            || (self.current_api_peer_uid.is_some() && self.current_api_peer_pid.is_none())
        {
            return crate::structured::lease::Caller::Uncertain;
        }
        if !self.socket_birth_valid() {
            return crate::structured::lease::Caller::Uncertain;
        }
        let evidence = self.caller_pane_evidence();
        let pane = evidence
            .as_ref()
            .ok()
            .copied()
            .flatten()
            .and_then(|(_, pane)| self.pane_child_pid(pane));
        let caller = self
            .session_leases
            .caller_with_pane(self.current_api_peer_pid, pane);
        if caller == crate::structured::lease::Caller::Unrelated
            && (evidence.is_err() || (principal == Some(Principal::Agent) && pane.is_none()))
        {
            crate::structured::lease::Caller::Uncertain
        } else {
            caller
        }
    }

    pub(super) fn socket_birth_valid(&self) -> bool {
        let Some(birth) = self.current_api_peer_birth else {
            return true;
        };
        self.current_api_peer_pid.is_none()
            || match birth {
                Some(birth) => {
                    self.current_api_peer_pid
                        .and_then(crate::platform::process_start_time)
                        == Some(birth)
                }
                None => self.session_leases.is_empty(),
            }
    }

    pub(crate) fn find_pane(
        &self,
        pane_id: crate::layout::PaneId,
    ) -> Option<(usize, &crate::pane::PaneState)> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(ws_idx, ws)| ws.pane_state(pane_id).map(|pane| (ws_idx, pane)))
    }

    pub(super) fn public_workspace_id(&self, ws_idx: usize) -> String {
        self.state.public_workspace_id(ws_idx)
    }

    pub(super) fn public_tab_id(&self, ws_idx: usize, tab_idx: usize) -> Option<String> {
        self.state.public_tab_id(ws_idx, tab_idx)
    }

    pub(super) fn public_pane_id(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<String> {
        self.state.public_pane_id(ws_idx, pane_id)
    }

    pub(super) fn parse_workspace_id(&self, id: &str) -> Option<usize> {
        self.state
            .workspaces
            .iter()
            .position(|workspace| workspace.id == id)
            .or_else(|| id.strip_prefix("w_")?.parse::<usize>().ok()?.checked_sub(1))
            .or_else(|| id.parse::<usize>().ok()?.checked_sub(1))
            .filter(|&index| index < self.state.workspaces.len())
    }

    pub(super) fn parse_tab_id(&self, id: &str) -> Option<(usize, usize)> {
        if let Some(rest) = id.strip_prefix("t_") {
            let (ws_raw, tab_raw) = rest.rsplit_once('_')?;
            let ws_idx = self.parse_workspace_id(ws_raw)?;
            let tab_idx = tab_raw.parse::<usize>().ok()?.checked_sub(1)?;
            self.state.workspaces.get(ws_idx)?.tabs.get(tab_idx)?;
            return Some((ws_idx, tab_idx));
        }

        let (ws_raw, tab_raw) = id.rsplit_once(':')?;
        let ws_idx = self.parse_workspace_id(ws_raw)?;
        // Public format `:tN` (Crockford base32, #25). Look up by stable
        // tab.number so the id keeps targeting the same tab even after
        // closes, moves, or reorders.
        let tab_idx = if let Some(encoded) = tab_raw.strip_prefix('t') {
            let tab_number = crate::workspace::decode_public_number(encoded)?;
            self.state
                .workspaces
                .get(ws_idx)?
                .tabs
                .iter()
                .position(|tab| tab.number == tab_number)?
        } else {
            // Legacy format `:N` is interpreted as a 1-based tab position.
            tab_raw.parse::<usize>().ok()?.checked_sub(1)?
        };
        self.state.workspaces.get(ws_idx)?.tabs.get(tab_idx)?;
        Some((ws_idx, tab_idx))
    }

    fn resolve_raw_pane_id(&self, raw: u32) -> Option<crate::layout::PaneId> {
        if let Some(alias) = self.state.pane_id_aliases.get(&raw).copied() {
            return self.find_pane(alias).map(|_| alias);
        }
        let pane_id = crate::layout::PaneId::from_raw(raw);
        if self.find_pane(pane_id).is_some() {
            return Some(pane_id);
        }
        None
    }

    /// Like parse_pane_id, but with the API caller's process ancestry as the
    /// authority when a peer pid is available. An env-baked FLOCK_PANE_ID can
    /// be stale in two ways: dangling (no such pane — heal by ancestry) or,
    /// worse, colliding with a *different* live pane after pane renumbering —
    /// in which case trusting it silently delivers reports to the wrong pane.
    /// So a parsed claim is only accepted once the caller is verified to be a
    /// descendant of the claimed pane's child process.
    pub(super) fn parse_pane_id_or_peer(
        &mut self,
        id: &str,
        peer_pid: Option<u32>,
    ) -> Option<(usize, crate::layout::PaneId)> {
        let parsed = self.parse_pane_id(id);
        let Some(peer) = peer_pid else {
            return parsed;
        };
        let chain = peer_ancestor_chain(peer);
        if id.is_empty() {
            return self.trusted_pane_in_chain(&chain);
        }
        self.reconcile_pane_claim(id, parsed, &chain)
    }

    /// Decide between a parsed pane claim and the caller's process ancestry.
    /// Ancestry wins on a positive mismatch; the claim survives only when it
    /// is verified or when ancestry yields no evidence at all (re-parented or
    /// daemonized callers).
    fn reconcile_pane_claim(
        &mut self,
        id: &str,
        parsed: Option<(usize, crate::layout::PaneId)>,
        chain: &[u32],
    ) -> Option<(usize, crate::layout::PaneId)> {
        if let Some((_, pane_id)) = parsed {
            if self
                .pane_child_pid(pane_id)
                .is_some_and(|child| chain.contains(&child))
            {
                return parsed;
            }
        }
        let resolved = self.resolve_pane_in_chain(chain);
        let Some((ws_idx, pane_id)) = resolved else {
            // No ancestry evidence: keep the claim rather than dropping the
            // report (the claimed pane may simply have no live child pid).
            return parsed;
        };
        if parsed.is_some_and(|(_, claimed)| claimed == pane_id) {
            return resolved;
        }
        tracing::info!(
            stale_id = id,
            ws_idx,
            resolved = pane_id.raw(),
            claimed_live_pane = parsed.is_some(),
            "resolved pane by process ancestry over claimed id"
        );
        // Memoize: the next report from this environment takes the alias fast
        // path instead of re-walking the process tree. Only safe when the raw
        // id is dangling or already alias-routed — aliasing a raw id that
        // still names a live pane directly would shadow that pane's own
        // truthful reports.
        if let Some(stale_raw) = id
            .strip_prefix("p_")
            .and_then(|rest| rest.parse::<u32>().ok())
        {
            let raw_names_live_pane = self
                .find_pane(crate::layout::PaneId::from_raw(stale_raw))
                .is_some();
            let already_aliased = self.state.pane_id_aliases.contains_key(&stale_raw);
            if stale_raw != pane_id.raw() && (!raw_names_live_pane || already_aliased) {
                self.state.pane_id_aliases.insert(stale_raw, pane_id);
            }
        }
        resolved
    }

    /// PID of the pane's direct child process, when alive.
    pub(crate) fn pane_child_pid(&self, pane_id: crate::layout::PaneId) -> Option<u32> {
        #[cfg(test)]
        if let Some(pid) = self.test_pane_child_pids.get(&pane_id) {
            return Some(*pid);
        }
        let (_ws_idx, pane) = self.find_pane(pane_id)?;
        self.terminal_runtimes
            .get(&pane.attached_terminal_id)
            .and_then(crate::terminal::TerminalRuntime::child_pid)
    }

    /// ADR-0036 §1a: authority needs the pane child's original birth, not only PID.
    fn trusted_pane_in_chain(&self, chain: &[u32]) -> Option<(usize, crate::layout::PaneId)> {
        let pane = self.resolve_pane_in_chain(chain)?;
        let (_, state) = self.find_pane(pane.1)?;
        let runtime = self.terminal_runtimes.get(&state.attached_terminal_id)?;
        let pid = runtime.child_pid()?;
        let birth = runtime.child_birth()?;
        (crate::platform::process_start_time(pid) == Some(birth)).then_some(pane)
    }

    /// A possible caller pane for refusal, self-protection and warning routing only.
    /// PID ancestry is not identity evidence and must never grant authority.
    pub(super) fn caller_pane_candidate(&self) -> Option<(usize, crate::layout::PaneId)> {
        self.resolve_pane_in_chain(&peer_ancestor_chain(self.current_api_peer_pid?))
    }

    /// The API caller's own pane, bound to the child's original birth identity.
    pub(super) fn caller_pane(&self) -> Option<(usize, crate::layout::PaneId)> {
        self.caller_pane_evidence().ok().flatten()
    }

    fn caller_pane_evidence(&self) -> Result<Option<(usize, crate::layout::PaneId)>, ()> {
        let Some(peer) = self.current_api_peer_pid else {
            return Ok(None);
        };
        let chain = peer_ancestor_chain(peer);
        let candidate = self.resolve_pane_in_chain(&chain);
        let trusted = self.trusted_pane_in_chain(&chain);
        if candidate.is_some() && trusted.is_none() {
            Err(())
        } else {
            Ok(trusted)
        }
    }

    fn resolve_pane_in_chain(&self, chain: &[u32]) -> Option<(usize, crate::layout::PaneId)> {
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            for tab in &ws.tabs {
                for pane_id in tab.panes.keys() {
                    if self
                        .pane_child_pid(*pane_id)
                        .is_some_and(|child| chain.contains(&child))
                    {
                        return Some((ws_idx, *pane_id));
                    }
                }
            }
        }
        None
    }

    pub(super) fn parse_pane_id(&self, id: &str) -> Option<(usize, crate::layout::PaneId)> {
        if let Some(rest) = id.strip_prefix("p_") {
            if let Some((ws_raw, pane_raw)) = rest.rsplit_once('_') {
                let ws_idx = self.parse_workspace_id(ws_raw)?;
                let pane_id = self.resolve_raw_pane_id(pane_raw.parse::<u32>().ok()?)?;
                self.state.workspaces.get(ws_idx)?.pane_state(pane_id)?;
                return Some((ws_idx, pane_id));
            }

            let pane_id = self.resolve_raw_pane_id(rest.parse::<u32>().ok()?)?;
            return self.find_pane(pane_id).map(|(ws_idx, _)| (ws_idx, pane_id));
        }

        // New public format `<workspace>:p<encoded>` (#25). Stable: closed
        // numbers are not reused, so old hook ids can't retarget a new pane.
        if let Some((ws_raw, pane_number_raw)) = id.rsplit_once(":p") {
            let ws_idx = self.parse_workspace_id(ws_raw)?;
            let pane_number = crate::workspace::decode_public_number(pane_number_raw)?;
            let ws = self.state.workspaces.get(ws_idx)?;
            let pane_id = ws
                .public_pane_numbers
                .iter()
                .find_map(|(pane_id, number)| (*number == pane_number).then_some(*pane_id))?;
            return Some((ws_idx, pane_id));
        }

        // Legacy public format `<workspace>-<decimal>`.
        let (ws_raw, pane_number_raw) = id.rsplit_once('-')?;
        let ws_idx = self.parse_workspace_id(ws_raw)?;
        let pane_number = pane_number_raw.parse::<usize>().ok()?;
        let ws = self.state.workspaces.get(ws_idx)?;
        let pane_id = ws
            .public_pane_numbers
            .iter()
            .find_map(|(pane_id, number)| (*number == pane_number).then_some(*pane_id))?;
        Some((ws_idx, pane_id))
    }
}

/// Collect the peer's ancestor chain (bounded; refreshes one pid at a time).
pub(super) fn peer_ancestor_chain(peer: u32) -> Vec<u32> {
    let mut system = sysinfo::System::new();
    let mut ancestors = Vec::with_capacity(16);
    let mut current = peer;
    for _ in 0..16 {
        ancestors.push(current);
        system.refresh_processes(
            sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(current)]),
            true,
        );
        let Some(parent) = system
            .process(sysinfo::Pid::from_u32(current))
            .and_then(|process| process.parent())
        else {
            break;
        };
        current = parent.as_u32();
        if current <= 1 {
            break;
        }
    }
    ancestors
}

/// Public id resolution. These live on `AppState` rather than `App` because
/// they read nothing else — and the keyboard mutation paths only ever hold
/// `&mut AppState`, yet must name a pane, tab or workspace *before* removing
/// it in order to report what closed (#175 ADR-0005).
impl crate::app::state::AppState {
    pub(crate) fn public_workspace_id(&self, ws_idx: usize) -> String {
        self.workspaces[ws_idx].id.clone()
    }

    pub(crate) fn public_tab_id(&self, ws_idx: usize, tab_idx: usize) -> Option<String> {
        let ws = self.workspaces.get(ws_idx)?;
        let tab_number = ws.public_tab_number(tab_idx)?;
        Some(crate::workspace::public_tab_id_for_number(
            &ws.id, tab_number,
        ))
    }

    pub(crate) fn public_pane_id(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<String> {
        let ws = self.workspaces.get(ws_idx)?;
        let pane_number = ws.public_pane_number(pane_id)?;
        Some(crate::workspace::public_pane_id_for_number(
            &ws.id,
            pane_number,
        ))
    }
}
#[cfg(test)]
mod tests {
    use super::super::App;

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

    #[test]
    fn workspace_ids_reject_out_of_range_aliases() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("main")];
        assert_eq!(app.parse_workspace_id(&app.state.workspaces[0].id), Some(0));
        for id in ["1", "w_1"] {
            assert_eq!(app.parse_workspace_id(id), Some(0));
        }
        for id in ["0", "w_0", "2", "w_2", "999999", "w_999999", "unknown"] {
            assert_eq!(app.parse_workspace_id(id), None, "{id}");
        }
        app.state.workspaces.clear();
        assert_eq!(app.parse_workspace_id("1"), None);
        assert_eq!(app.parse_workspace_id("w_1"), None);
    }

    #[test]
    fn alias_gc_drops_entries_for_dead_target_panes() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("main")];
        let live = app.state.workspaces[0].tabs[0].root_pane;
        let dead = crate::layout::PaneId::from_raw(9_999);

        app.state.pane_id_aliases.insert(1_000, live);
        app.state.pane_id_aliases.insert(1_001, dead);

        app.state
            .remove_alias_shadowed_by_new_pane(crate::layout::PaneId::from_raw(7_777));

        assert_eq!(app.state.pane_id_aliases.get(&1_000), Some(&live));
        assert_eq!(app.state.pane_id_aliases.get(&1_001), None);
    }

    #[test]
    fn memoized_alias_takes_the_fast_path_after_ancestry_resolution() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("main")];
        let pane = app.state.workspaces[0].tabs[0].root_pane;

        // Simulate the memoization a successful ancestry walk performs.
        app.state.pane_id_aliases.insert(123_456, pane);

        // The stale id now resolves through parse_pane_id without any peer.
        let resolved = app.parse_pane_id_or_peer("p_123456", None);
        assert_eq!(resolved.map(|(_, id)| id), Some(pane));
    }

    /// Two live panes with known child pids; the caller's ancestor chain
    /// places it in pane B while its env claims pane A.
    fn two_pane_app() -> (App, crate::layout::PaneId, crate::layout::PaneId) {
        let mut app = test_app();
        app.state.workspaces = vec![
            crate::workspace::Workspace::test_new("a"),
            crate::workspace::Workspace::test_new("b"),
        ];
        let pane_a = app.state.workspaces[0].tabs[0].root_pane;
        let pane_b = app.state.workspaces[1].tabs[0].root_pane;
        app.test_pane_child_pids.insert(pane_a, 11_111);
        app.test_pane_child_pids.insert(pane_b, 22_222);
        (app, pane_a, pane_b)
    }

    #[test]
    fn colliding_stale_id_is_overridden_by_process_ancestry() {
        let (mut app, pane_a, pane_b) = two_pane_app();
        let id = format!("p_{}", pane_a.raw());
        let claimed = app.parse_pane_id(&id);
        assert_eq!(claimed.map(|(_, p)| p), Some(pane_a));

        // hook script (33_333) -> agent process = pane B's child (22_222)
        let resolved = app.reconcile_pane_claim(&id, claimed, &[33_333, 22_222]);
        assert_eq!(resolved.map(|(_, p)| p), Some(pane_b));
        // Never alias a raw id that still names a live pane: it would shadow
        // pane A's own truthful reports.
        assert!(!app.state.pane_id_aliases.contains_key(&pane_a.raw()));
    }

    #[test]
    fn truthful_id_is_verified_by_the_ancestry_fast_path() {
        let (mut app, pane_a, _pane_b) = two_pane_app();
        let id = format!("p_{}", pane_a.raw());
        let claimed = app.parse_pane_id(&id);

        let resolved = app.reconcile_pane_claim(&id, claimed, &[33_333, 11_111]);
        assert_eq!(resolved.map(|(_, p)| p), Some(pane_a));
        assert!(app.state.pane_id_aliases.is_empty());
    }

    #[test]
    fn claim_survives_when_ancestry_yields_no_evidence() {
        // Re-parented/daemonized callers produce a chain that matches no
        // pane; the claimed pane must keep receiving reports.
        let (mut app, pane_a, _pane_b) = two_pane_app();
        let id = format!("p_{}", pane_a.raw());
        let claimed = app.parse_pane_id(&id);

        let resolved = app.reconcile_pane_claim(&id, claimed, &[999_999]);
        assert_eq!(resolved.map(|(_, p)| p), Some(pane_a));
    }

    #[test]
    fn dangling_id_resolves_by_ancestry_and_memoizes_an_alias() {
        let (mut app, _pane_a, pane_b) = two_pane_app();

        let resolved = app.reconcile_pane_claim("p_999999", None, &[33_333, 22_222]);
        assert_eq!(resolved.map(|(_, p)| p), Some(pane_b));
        assert_eq!(app.state.pane_id_aliases.get(&999_999), Some(&pane_b));
    }
    async fn pane_birth_handoff_case(metadata: &str) {
        use std::sync::{atomic::AtomicBool, Arc};
        use std::time::Duration;
        let root =
            crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("birth-handoff")];
        app.state.ensure_test_terminals();
        let pane = app.state.workspaces[0].focused_pane_id().unwrap();
        let terminal = app.state.workspaces[0]
            .pane_state(pane)
            .unwrap()
            .attached_terminal_id
            .clone();
        let runtime = crate::terminal::TerminalRuntime::spawn_shell_command(
            pane,
            24,
            80,
            root.path().to_owned(),
            "read line",
            &[],
            4096,
            Default::default(),
            app.event_tx.clone(),
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let pid = runtime.child_pid().unwrap();
        let birth = crate::platform::process_start_time(pid).unwrap();
        app.current_api_peer_pid = Some(pid);
        app.current_api_peer_birth = Some(Some(birth));
        let reservation = app
            .session_leases
            .reserve("agent", "pending", None)
            .unwrap();
        app.terminal_runtimes.insert(terminal.clone(), runtime);
        let spawned_evidence = app.caller_pane();
        let runtime = app.terminal_runtimes.remove(&terminal).unwrap();
        let state = runtime.handoff_runtime_state(pane.raw());
        let captured = state.child_birth;
        let mut encoded = serde_json::to_value(&state).unwrap();
        match metadata {
            "legacy" => {
                encoded.as_object_mut().unwrap().remove("child_birth");
            }
            "reused" => {
                encoded["child_birth"] = serde_json::json!(birth.wrapping_add(1));
            }
            _ => {}
        }
        let transferred: crate::handoff_runtime::HandoffRuntimeState =
            serde_json::from_value(encoded).unwrap();
        runtime
            .start_handoff_pause(Duration::from_secs(2), false)
            .unwrap()
            .wait()
            .unwrap();
        let fd = runtime.duplicate_handoff_fd().unwrap();
        runtime.preserve_for_handoff();
        let mut adopted = crate::terminal::TerminalRuntime::from_handoff_fd(
            crate::handoff_runtime::ImportedHandoffRuntime {
                master_fd: fd,
                state: transferred,
            },
            4096,
            Default::default(),
            app.event_tx.clone(),
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        adopted.assume_handoff_ownership();
        adopted.set_handoff_reader_paused(false);
        let imported = adopted.child_birth();
        app.terminal_runtimes.insert(terminal.clone(), adopted);
        let evidence = app.caller_pane();
        let empty_claim = app.parse_pane_id_or_peer("", Some(pid));
        let caller = app.lease_caller();
        let named_claim = app.parse_pane_id_or_peer(&format!("p_{}", pane.raw()), Some(pid));
        let runtime = app.terminal_runtimes.remove(&terminal).unwrap();
        let stopped = runtime.shutdown_for_restart(vec![pid], Duration::ZERO);
        assert!(tokio::time::timeout(Duration::from_secs(10), stopped)
            .await
            .unwrap()
            .unwrap());
        drop(reservation);
        assert_eq!(captured, Some(birth));
        assert_eq!(spawned_evidence, Some((0, pane)));
        assert_eq!(
            named_claim,
            Some((0, pane)),
            "named hook claim healing is preserved"
        );
        if metadata == "valid" {
            assert_eq!(imported, Some(birth));
            assert_eq!(evidence, Some((0, pane)));
            assert_eq!(empty_claim, Some((0, pane)));
            assert_eq!(caller, crate::structured::lease::Caller::Unrelated);
        } else {
            assert_eq!(evidence, None);
            assert_eq!(empty_claim, None);
            assert_eq!(caller, crate::structured::lease::Caller::Uncertain);
            if metadata == "legacy" {
                assert_eq!(imported, None);
            }
        }
        assert!(app.test_pane_child_pids.is_empty());
    }

    #[tokio::test]
    async fn real_pane_birth_survives_serialized_handoff_and_grants_pane_evidence() {
        pane_birth_handoff_case("valid").await;
    }

    #[tokio::test]
    async fn legacy_handoff_without_birth_cannot_override_launcher_uncertainty() {
        pane_birth_handoff_case("legacy").await;
    }

    #[tokio::test]
    async fn reused_pane_birth_cannot_override_launcher_uncertainty() {
        pane_birth_handoff_case("reused").await;
    }
}
