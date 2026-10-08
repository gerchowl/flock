//! Server-owned restart lifecycle. Scheduling data lives in AppState and process
//! sampling / shutdown receivers live separately in RestartRuntime.
use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::App;
use crate::agent_restart::{fingerprint, rate_limited, resume_launch, RestartPolicy};
use crate::agent_resume::{AgentResumePlan, PersistedAgentSession};
use crate::api::schema::{
    AgentRestartParams, ErrorBody, EventData, EventEnvelope, EventKind, MessageTarget, MsgIntent,
    MsgSendParams,
};
use crate::detect::AgentState;
use crate::terminal::TerminalId;

const TICK: Duration = Duration::from_millis(250);
const SAMPLE: Duration = Duration::from_secs(2);

#[derive(Debug, Default, Clone)]
pub(crate) struct RestartStates {
    pending: HashMap<TerminalId, RestartRequest>,
    history: HashMap<String, Vec<Instant>>,
}

#[derive(Debug, Clone)]
struct RestartRequest {
    session: PersistedAgentSession,
    plan: AgentResumePlan,
    reason: String,
    continuation: String,
    requested_at: Instant,
    requested_ms: u64,
    phase: Phase,
    forced: Option<String>,
    old_pid: Option<u32>,
    rss_before: u64,
    lost: Vec<String>,
    flush_started: Option<Instant>,
    flushed: bool,
    answered_dialog: bool,
    stop_only: bool,
    grace_after_turn: bool,
    requester: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Waiting,
    Stopping,
    Verifying(Instant),
}

#[derive(Default)]
pub(crate) struct RestartRuntime {
    shutdowns: HashMap<TerminalId, tokio::sync::oneshot::Receiver<bool>>,
    system: sysinfo::System,
    samples: HashMap<TerminalId, Vitals>,
    sampled_at: Option<Instant>,
    ticked_at: Option<Instant>,
    refusals: HashMap<TerminalId, String>,
}

#[derive(Clone)]
struct Vitals {
    rss: u64,
    pids: Vec<u32>,
    processes: Vec<String>,
    spin_since: Option<Instant>,
    quiet_since: Instant,
    output: u64,
    revision: u64,
}

impl RestartStates {
    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

impl RestartRuntime {
    pub(crate) fn next_deadline(
        &self,
        session: &crate::config::model::SessionConfig,
        now: Instant,
    ) -> Option<Instant> {
        let automatic = session.restart.automatic()
            || session
                .restart_agents
                .values()
                .any(RestartPolicy::automatic);
        if self.shutdowns.is_empty() && !automatic {
            return None;
        }
        Some(self.ticked_at.unwrap_or(now) + TICK)
    }
}

impl App {
    fn restart_policy(&self, id: &TerminalId) -> RestartPolicy {
        let config = &self.state.config.session;
        // The host off-switch always wins over a per-agent override.
        if !config.restart.enabled {
            return config.restart.clone();
        }
        self.state
            .terminals
            .get(id)
            .and_then(|t| t.agent_name.as_ref())
            .and_then(|name| config.restart_agents.get(name))
            .unwrap_or(&config.restart)
            .clone()
    }

    pub(crate) fn queue_agent_restart(
        &mut self,
        params: AgentRestartParams,
        now: Instant,
    ) -> Result<(String, String, bool), ErrorBody> {
        self.queue_agent_restart_inner(params, now, None)
    }

    fn queue_agent_restart_inner(
        &mut self,
        params: AgentRestartParams,
        now: Instant,
        forced: Option<String>,
    ) -> Result<(String, String, bool), ErrorBody> {
        let refuse = |code: &str, message: String| ErrorBody {
            code: code.into(),
            message,
        };
        if params.when != "after_turn"
            || params.reason.trim().is_empty()
            || params.reason.len() > 1024
            || params.reason.chars().any(char::is_control)
        {
            return Err(refuse(
                "invalid_params",
                "reason must be nonempty plain text (at most 1024 bytes); when must be after_turn"
                    .into(),
            ));
        }
        let (ws_idx, pane_id) = if params.target == "self" {
            self.parse_pane_id_or_peer("", self.current_api_peer_pid)
                .ok_or_else(|| {
                    refuse(
                        "no_caller_pane",
                        "self requires an attested caller inside an agent pane".into(),
                    )
                })?
        } else {
            let target = self
                .resolve_terminal_target(&params.target)
                .map_err(|err| self.agent_target_error_body(err))?;
            (target.ws_idx, target.pane_id)
        };
        let id = self.state.workspaces[ws_idx]
            .terminal_id(pane_id)
            .cloned()
            .ok_or_else(|| refuse("not_found", "target terminal disappeared".into()))?;
        let pane = self
            .public_pane_id(ws_idx, pane_id)
            .ok_or_else(|| refuse("not_found", "target pane disappeared".into()))?;
        let policy = self.restart_policy(&id);
        if !policy.enabled {
            return Err(refuse(
                "restart_disabled",
                "agent restarts are disabled in session.restart".into(),
            ));
        }
        if self.state.agent_restarts.pending.contains_key(&id) {
            return Err(refuse(
                "restart_pending",
                "this agent already has a pending restart".into(),
            ));
        }
        let terminal = self
            .state
            .terminals
            .get(&id)
            .ok_or_else(|| refuse("not_found", "target terminal disappeared".into()))?;
        if terminal.hibernated_resume_plan.is_some()
            || terminal.pending_agent_resume_plan.is_some()
            || terminal.armed_self_compact.is_some()
            || self.idle_wake.in_flight(&pane)
        {
            return Err(refuse(
                "restart_busy",
                "agent is hibernated, resuming, compacting, or has a wake submission in flight"
                    .into(),
            ));
        }
        if self.terminal_runtimes.get(&id).is_none() {
            return Err(refuse(
                "restart_busy",
                "agent has no running process".into(),
            ));
        }
        let session = terminal.persisted_agent_session.clone().ok_or_else(|| {
            refuse(
                "restart_unsupported",
                "agent has no stored native session".into(),
            )
        })?;
        if !matches!(
            session.agent.as_str(),
            "claude" | "codex" | "opencode" | "copilot" | "pi" | "hermes"
        ) || !terminal.restart_session_confirmed()
        {
            return Err(refuse("restart_unsupported", "restart requires a supported harness with live session-reporting hooks; no confirmed session hook is available".into()));
        }
        let plan = resume_launch(
            &session,
            terminal.launch_argv.as_deref().unwrap_or_default(),
        )
        .map_err(|reason| refuse("restart_unsupported", reason))?;
        let continuation = params.continue_with.unwrap_or_else(|| {
            format!(
                "You restarted yourself for: {}. Continue your task.",
                params.reason
            )
        });
        if continuation.len() > 16 * 1024
            || continuation.trim().is_empty()
            || crate::agent_self_compact::check_continuation(&continuation).is_err()
        {
            return Err(refuse("invalid_params", "continue_with must be one line of plain instructions, at most 16 KiB, and cannot start with / or !".into()));
        }
        let agent = terminal.agent_id.to_string();
        let requester = self
            .parse_pane_id_or_peer("", self.current_api_peer_pid)
            .and_then(|(ws, pane)| self.state.workspaces[ws].terminal_id(pane))
            .and_then(|id| self.state.terminals.get(id))
            .map(|t| t.agent_id.to_string());
        let history = self.state.agent_restarts.history.entry(agent).or_default();
        let stop_only = rate_limited(history, now, &policy);
        if stop_only && forced.is_none() {
            let retry_after = history
                .iter()
                .min()
                .map(|at| {
                    let wait = Duration::from_secs(policy.window_secs)
                        .saturating_sub(now.saturating_duration_since(*at));
                    wait.as_secs()
                        .saturating_add(u64::from(wait.subsec_nanos() != 0))
                })
                .unwrap_or(policy.window_secs);
            return Err(refuse("restart_rate_limited", format!("restart rate limit reached; retry_after_secs={retry_after}; max_restarts={} (zero disables restarts)", policy.max_restarts)));
        }
        let value = session.session_ref.value.clone();
        self.state.agent_restarts.pending.insert(
            id.clone(),
            RestartRequest {
                session,
                plan,
                reason: params.reason,
                continuation,
                requested_at: now,
                requested_ms: super::notifications::now_ms(),
                phase: Phase::Waiting,
                forced,
                old_pid: None,
                rss_before: 0,
                lost: Vec::new(),
                flush_started: None,
                flushed: false,
                answered_dialog: false,
                stop_only,
                grace_after_turn: false,
                requester,
            },
        );
        if let Some(terminal) = self.state.terminals.get_mut(&id) {
            terminal.restart_in_progress = true;
        }
        Ok((pane, value, stop_only))
    }

    pub(crate) fn tick_agent_restarts(&mut self, now: Instant) -> bool {
        let automatic = self.state.config.session.restart.automatic()
            || self
                .state
                .config
                .session
                .restart_agents
                .values()
                .any(RestartPolicy::automatic);
        if self.state.agent_restarts.pending.is_empty() && !automatic {
            return false;
        }
        if self
            .restarts
            .ticked_at
            .is_some_and(|last| now.saturating_duration_since(last) < TICK)
        {
            return false;
        }
        self.restarts.ticked_at = Some(now);
        if self
            .restarts
            .sampled_at
            .is_none_or(|last| now.saturating_duration_since(last) >= SAMPLE)
        {
            self.sample_restart_vitals(now);
        }
        // Memory pressure chooses only one agent per sampling interval. Do not
        // cascade through the host while an earlier stop is freeing memory.
        let heaviest = self
            .restarts
            .samples
            .iter()
            .max_by_key(|(_, v)| v.rss)
            .map(|(id, _)| id.clone());
        let stopping = self
            .state
            .agent_restarts
            .pending
            .values()
            .any(|r| r.phase == Phase::Stopping);
        let mut forced = Vec::new();
        for (id, v) in &self.restarts.samples {
            let policy = self.restart_policy(id);
            if !policy.enabled {
                continue;
            }
            let reason = policy.hard_reason(
                v.rss,
                self.restarts.system.available_memory(),
                !stopping && heaviest.as_ref() == Some(id),
                v.spin_since
                    .map(|t| now.saturating_duration_since(t))
                    .unwrap_or_default(),
                now.saturating_duration_since(v.quiet_since),
            );
            if let Some(reason) = reason {
                forced.push((id.clone(), reason));
            }
        }
        for (id, reason) in forced {
            if !self.state.agent_restarts.pending.contains_key(&id) {
                let Some((ws, pane)) = self.restart_location(&id) else {
                    continue;
                };
                let Some(target) = self.public_pane_id(ws, pane) else {
                    continue;
                };
                let params = AgentRestartParams {
                    target,
                    reason: reason.clone(),
                    continue_with: None,
                    when: "after_turn".into(),
                };
                if let Err(err) = self.queue_agent_restart_inner(params, now, Some(reason.clone()))
                {
                    if self.restarts.refusals.get(&id) != Some(&err.code) {
                        crate::logging::agent_restart_refused(&id.to_string(), &err.code);
                        self.restarts.refusals.insert(id.clone(), err.code);
                    }
                    continue;
                }
            }
            if let Some(request) = self.state.agent_restarts.pending.get_mut(&id) {
                if request.phase == Phase::Waiting {
                    request.forced = Some(reason);
                }
            }
        }
        let mut changed = false;
        let ids: Vec<_> = self.state.agent_restarts.pending.keys().cloned().collect();
        for id in ids {
            let Some(mut request) = self.state.agent_restarts.pending.remove(&id) else {
                continue;
            };
            let previous = request.phase;
            let keep = self.advance_restart(&id, &mut request, now);
            changed |= previous != request.phase || !keep;
            if keep {
                self.state.agent_restarts.pending.insert(id, request);
            }
        }
        // State intents are pure. This deadline drives even a quiet server.
        if !self.state.agent_restarts.pending.is_empty() {
            self.restarts.ticked_at = Some(now);
        }
        changed
    }

    /// A fresh identity must come from the replacement process tree. A queued
    /// report from the old tree, or an operator's asserted session id, cannot
    /// verify a resume even if its session string happens to match.
    pub(crate) fn note_restart_identity_report(&mut self, ws: usize, pane: crate::layout::PaneId) {
        let Some(id) = self
            .state
            .workspaces
            .get(ws)
            .and_then(|ws| ws.terminal_id(pane))
            .cloned()
        else {
            return;
        };
        if !self
            .state
            .agent_restarts
            .pending
            .get(&id)
            .is_some_and(|request| matches!(request.phase, Phase::Verifying(_)))
        {
            return;
        }
        let Some(root) = self.terminal_runtimes.get(&id).and_then(|r| r.child_pid()) else {
            return;
        };
        if self
            .current_api_peer_pid
            .is_some_and(|peer| super::ids::peer_ancestor_chain(peer).contains(&root))
        {
            if let Some(terminal) = self.state.terminals.get_mut(&id) {
                terminal.restart_confirmation_pid = Some(root);
            }
        }
    }

    fn restart_location(&self, id: &TerminalId) -> Option<(usize, crate::layout::PaneId)> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(idx, ws)| {
                ws.tabs.iter().find_map(|tab| {
                    tab.panes
                        .iter()
                        .find(|(_, pane)| &pane.attached_terminal_id == id)
                        .map(|(pane_id, _)| (idx, *pane_id))
                })
            })
    }

    fn sample_restart_vitals(&mut self, now: Instant) {
        self.restarts.system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::All,
            true,
            sysinfo::ProcessRefreshKind::nothing()
                .with_memory()
                .with_cpu()
                .with_cmd(sysinfo::UpdateKind::OnlyIfNotSet),
        );
        self.restarts.system.refresh_memory();
        let mut samples = HashMap::new();
        for (id, terminal) in &self.state.terminals {
            if terminal.persisted_agent_session.is_none() {
                continue;
            }
            let Some(runtime) = self.terminal_runtimes.get(id) else {
                continue;
            };
            let Some(root) = runtime.child_pid() else {
                continue;
            };
            let mut pids = vec![root];
            // Walk the parent graph, including detached-session descendants.
            loop {
                let before = pids.len();
                for (pid, process) in self.restarts.system.processes() {
                    let pid = pid.as_u32();
                    if !pids.contains(&pid)
                        && process.parent().is_some_and(|p| pids.contains(&p.as_u32()))
                    {
                        pids.push(pid);
                    }
                }
                if pids.len() == before {
                    break;
                }
            }
            pids.extend(crate::platform::session_processes(root));
            pids.sort_unstable();
            pids.dedup();
            let mut rss = 0u64;
            let mut cpu = 0.0;
            let mut processes = Vec::new();
            for pid in &pids {
                if let Some(process) = self.restarts.system.process(sysinfo::Pid::from_u32(*pid)) {
                    rss = rss.saturating_add(process.memory());
                    cpu += process.cpu_usage();
                    let mut ancestor = Some(*pid);
                    let mut mcp = false;
                    while let Some(parent) = ancestor {
                        if parent == root {
                            break;
                        }
                        let Some(node) =
                            self.restarts.system.process(sysinfo::Pid::from_u32(parent))
                        else {
                            break;
                        };
                        let args = node
                            .cmd()
                            .iter()
                            .map(|arg| arg.to_string_lossy())
                            .collect::<Vec<_>>();
                        if args.iter().any(|arg| {
                            arg == "mcp" || arg.contains("mcp-server") || arg.contains("/mcp/")
                        }) {
                            mcp = true;
                            break;
                        }
                        ancestor = node.parent().map(|p| p.as_u32());
                    }
                    if *pid != root
                        && !mcp
                        && crate::detect::identify_agent(&process.name().to_string_lossy())
                            .is_none()
                    {
                        processes.push(format!("{} (pid {pid})", process.name().to_string_lossy()));
                    }
                }
            }
            let output = fingerprint(&runtime.recent_text(100));
            let previous = self.restarts.samples.get(id);
            let policy = self.restart_policy(id);
            let unchanged = previous
                .is_some_and(|old| old.output == output && old.revision == terminal.revision);
            // Idle silence is normal. Only a non-idle execution is unresponsive.
            let quiet_since = if unchanged && terminal.state != AgentState::Idle {
                previous.map(|p| p.quiet_since).unwrap_or(now)
            } else {
                now
            };
            let spin_since = (cpu >= policy.spin_cpu_percent)
                .then(|| previous.and_then(|p| p.spin_since).unwrap_or(now));
            samples.insert(
                id.clone(),
                Vitals {
                    rss,
                    pids,
                    processes,
                    output,
                    revision: terminal.revision,
                    quiet_since,
                    spin_since,
                },
            );
        }
        self.restarts
            .refusals
            .retain(|id, _| samples.contains_key(id));
        self.restarts.samples = samples;
        self.restarts.sampled_at = Some(now);
    }

    fn restart_ready(
        &self,
        id: &TerminalId,
        now: Instant,
        policy: &RestartPolicy,
        since: Instant,
    ) -> bool {
        let Some(terminal) = self.state.terminals.get(id) else {
            return false;
        };
        let Some(runtime) = self.terminal_runtimes.get(id) else {
            return false;
        };
        terminal.restart_idle_observed_since(since)
            && terminal
                .idle_wake_blocker(
                    now,
                    Duration::from_millis(policy.settle_ms),
                    Duration::from_millis(policy.fresh_ms),
                )
                .is_none()
            && runtime.last_operator_input_at().is_none_or(|at| {
                now.saturating_duration_since(at) >= Duration::from_millis(policy.operator_quiet_ms)
            })
            && crate::detect::parse_agent_label(
                &terminal
                    .persisted_agent_session
                    .as_ref()
                    .map(|s| s.agent.clone())
                    .unwrap_or_default(),
            )
            .and_then(|agent| {
                crate::detect::agent_prompt_is_empty(agent, &runtime.recent_text(100))
            }) == Some(true)
    }

    fn advance_restart(
        &mut self,
        id: &TerminalId,
        request: &mut RestartRequest,
        now: Instant,
    ) -> bool {
        let Some((_ws_idx, pane_id)) = self.restart_location(id) else {
            self.restarts.shutdowns.remove(id);
            self.recover_restart(id, request);
            return false;
        };
        let policy = self.restart_policy(id);
        match request.phase {
            Phase::Waiting => {
                if self.restart_runtime_gone(id) {
                    self.recover_restart(id, request);
                    self.report_restart(id, request, "restart_cancelled", "agent exited before stop; resume plan retained; retry with flk agent resume <pane>".into());
                    return false;
                }
                if !policy.enabled {
                    self.finish_restart_lock(id);
                    self.report_restart(
                        id,
                        request,
                        "restart_cancelled",
                        "disabled before stop".into(),
                    );
                    return false;
                }
                let Some(terminal) = self.state.terminals.get(id) else {
                    return false;
                };
                if terminal.persisted_agent_session.as_ref() != Some(&request.session) {
                    self.finish_restart_lock(id);
                    self.report_restart(
                        id,
                        request,
                        "restart_cancelled",
                        "session changed before stop".into(),
                    );
                    return false;
                }
                if now.saturating_duration_since(request.requested_at)
                    >= Duration::from_secs(policy.restart_grace_secs)
                {
                    if request.forced.is_none() {
                        request.grace_after_turn =
                            self.state.terminals.get(id).is_some_and(|t| {
                                t.restart_idle_observed_since(request.requested_at)
                            }) || self.restart_transcript_flushed(request);
                    }
                    request.forced.get_or_insert_with(|| {
                        format!(
                            "restart_grace: {} seconds expired",
                            policy.restart_grace_secs
                        )
                    });
                }
                if request.forced.is_none()
                    && !self.restart_ready(id, now, &policy, request.requested_at)
                {
                    return true;
                }
                if request
                    .forced
                    .as_ref()
                    .is_some_and(|reason| reason.starts_with("restart_grace:"))
                    && self.terminal_runtimes.get(id).is_some_and(|runtime| {
                        runtime.last_operator_input_at().is_some_and(|at| {
                            now.saturating_duration_since(at)
                                < Duration::from_millis(policy.operator_quiet_ms)
                        })
                    })
                {
                    return true;
                }
                let start = *request.flush_started.get_or_insert(now);
                request.flushed = self.restart_transcript_flushed(request);
                if request
                    .forced
                    .as_ref()
                    .is_some_and(|reason| reason.starts_with("restart_grace:"))
                {
                    request.grace_after_turn |=
                        request.flushed
                            || self.state.terminals.get(id).is_some_and(|t| {
                                t.restart_idle_observed_since(request.requested_at)
                            });
                }
                if !request.flushed
                    && now.saturating_duration_since(start)
                        < Duration::from_millis(policy.flush_wait_ms)
                {
                    return true;
                }
                let Some(runtime) = self.terminal_runtimes.remove(id) else {
                    self.recover_restart(id, request);
                    self.report_restart(id, request, "restart_cancelled", "runtime disappeared before stop; resume plan retained; retry with flk agent resume <pane>".into());
                    return false;
                };
                request.old_pid = runtime.child_pid();
                // Refresh the tree at the stop boundary, not at an earlier poll.
                self.terminal_runtimes.insert(id.clone(), runtime);
                self.sample_restart_vitals(now);
                let Some(runtime) = self.terminal_runtimes.remove(id) else {
                    return false;
                };
                let pids = self
                    .restarts
                    .samples
                    .get(id)
                    .map(|v| v.pids.clone())
                    .unwrap_or_default();
                if let Some(vitals) = self.restarts.samples.get(id) {
                    request.rss_before = vitals.rss;
                    request.lost = vitals.processes.clone();
                }
                if let Some(checkpoint) =
                    crate::agent_restart::checkpoint(&request.session.session_ref.value, "stop")
                {
                    request.lost.extend(checkpoint.background_tasks);
                }
                if let Some(terminal) = self.state.terminals.get_mut(id) {
                    terminal.set_hibernated_resume_plan(Some(request.plan.clone()));
                    terminal.respawn_shell_on_exit = false;
                    terminal.restart_in_progress = true;
                }
                self.restarts.shutdowns.insert(
                    id.clone(),
                    runtime.shutdown_for_restart(pids, Duration::from_secs(policy.kill_grace_secs)),
                );
                request.phase = Phase::Stopping;
                self.state.mark_session_dirty();
                self.report_restart(
                    id,
                    request,
                    "restart_stopping",
                    if request.stop_only {
                        "restart rate limit reached; stopping without resuming".into()
                    } else {
                        "stopping the snapshotted process tree".into()
                    },
                );
                true
            }
            Phase::Stopping => {
                let result = self.restarts.shutdowns.get_mut(id).map(|rx| rx.try_recv());
                match result {
                    Some(Err(tokio::sync::oneshot::error::TryRecvError::Empty)) => return true,
                    Some(Ok(true)) => {}
                    _ => {
                        self.restarts.shutdowns.remove(id);
                        self.recover_restart(id, request);
                        self.report_restart(id, request, "restart_stuck", "process tree did not stop; resume plan retained and restart lock released; stop remaining processes before retrying with flk agent resume <pane>".into());
                        return false;
                    }
                }
                self.restarts.shutdowns.remove(id);
                if let Some(terminal) = self.state.terminals.get_mut(id) {
                    if request.stop_only {
                        // Keep the pane parked and locked: focus cannot undo the cap.
                        terminal.restart_stopped = true;
                        terminal.restart_in_progress = true;
                        terminal.prepare_restart_resume();
                    } else {
                        terminal.restart_in_progress = false;
                        terminal.prepare_restart_resume();
                    }
                }
                if request.stop_only {
                    self.report_restart(
                        id,
                        request,
                        "restart_stopped",
                        "rate limit reached; operator intervention required".into(),
                    );
                    return false;
                }
                if let Err(reason) = self.resume_restart(id, pane_id, &request.plan) {
                    self.recover_restart(id, request);
                    self.report_restart(
                        id,
                        request,
                        "restart_stuck",
                        format!(
                            "{reason}; resume plan retained; retry with flk agent resume <pane>"
                        ),
                    );
                    return false;
                }
                if let Some(terminal) = self.state.terminals.get_mut(id) {
                    terminal.restart_in_progress = true;
                    self.state
                        .agent_restarts
                        .history
                        .entry(terminal.agent_id.to_string())
                        .or_default()
                        .push(now);
                }
                request.phase = Phase::Verifying(now);
                true
            }
            Phase::Verifying(started) => {
                if self.restart_runtime_gone(id) {
                    let output = self.restart_last_output(id);
                    self.recover_restart(id, request);
                    self.report_restart(id, request, "restart_stuck", format!("resumed process exited; resume plan retained; retry with flk agent resume <pane>. Last output: {output}"));
                    return false;
                }
                let new_pid = self.terminal_runtimes.get(id).and_then(|r| r.child_pid());
                let same = self.state.terminals.get(id).is_some_and(|t| {
                    t.restart_confirmation_pid == new_pid
                        && new_pid.is_some()
                        && t.restart_session_confirmed()
                        && t.persisted_agent_session.as_ref() == Some(&request.session)
                });
                let different = self.state.terminals.get(id).is_some_and(|t| {
                    t.restart_confirmation_pid == new_pid
                        && new_pid.is_some()
                        && t.restart_session_confirmed()
                        && t.persisted_agent_session.as_ref() != Some(&request.session)
                });
                if different {
                    self.recover_restart(id, request);
                    self.report_restart(
                        id,
                        request,
                        "restart_stuck",
                        "resumed harness reported a different session; continuation withheld; resume plan retained; close the running harness before flk agent resume <pane>"
                            .into(),
                    );
                    return false;
                }

                if same
                    && new_pid.is_some()
                    && new_pid != request.old_pid
                    && self.restart_ready(id, now, &policy, started)
                {
                    self.sample_restart_vitals(now);
                    let rss = self.restarts.samples.get(id).map(|v| v.rss);
                    let mut body = format!("Restart verified: old_pid={:?}, new_pid={:?}, session={}, reason={}, rss_before={}, rss_after={:?}. {}", request.old_pid, new_pid, request.session.session_ref.value, request.reason, request.rss_before, rss, request.continuation);
                    if let Some(limit) = &request.forced {
                        let tool = crate::agent_restart::checkpoint(
                            &request.session.session_ref.value,
                            "tool",
                        )
                        .and_then(|c| {
                            c.tool.map(|name| {
                                format!("{name}, started at {} ms since epoch", c.written_ms)
                            })
                        })
                        .unwrap_or_else(|| "unknown; the harness did not report it".into());
                        if request.grace_after_turn {
                            body = format!("You were force-restarted after the idle grace (reason: {limit}); your turn had ended but background activity prevented settled idle. Verify background tasks before continuing. {body}");
                        } else {
                            body = format!("You were force-restarted mid-turn (reason: {limit}). Your last tool call ({tool}) may be incomplete; verify the working-tree and process state before continuing. {body}");
                        }
                    }
                    if !request.flushed {
                        body.push_str(" The last turn could not be confirmed flushed before stop; verify the transcript and working-tree state.");
                    }
                    if !request.lost.is_empty() {
                        body.push_str(&format!(" These background tasks/processes were stopped by the restart: {}; restart any you still need.", request.lost.join(", ").chars().take(2048).collect::<String>()));
                    }
                    self.finish_restart_lock(id);
                    self.report_restart(
                        id,
                        request,
                        "restarted",
                        "same session and empty idle prompt verified".into(),
                    );
                    if body.len() > 16 * 1024 {
                        self.restart_message(id, request.continuation.clone());
                        let context = body.replace(
                            &request.continuation,
                            "Continuation is in the preceding message.",
                        );
                        self.restart_message(id, context);
                    } else {
                        self.restart_message(id, body);
                    }
                    return false;
                }
                self.answer_restart_dialog(id, request);
                if now.saturating_duration_since(started)
                    >= Duration::from_secs(policy.verify_timeout_secs)
                {
                    let screen = self
                        .terminal_runtimes
                        .get(id)
                        .map(|r| r.recent_text(20))
                        .unwrap_or_default();
                    self.recover_restart(id, request);
                    self.report_restart(id, request, "restart_stuck", format!("verification timed out; resume plan retained; close the running harness before flk agent resume <pane>. Last output: {}", screen.chars().take(1024).collect::<String>()));
                    return false;
                }
                true
            }
        }
    }

    /// Agent-start launches argv directly, so a restart uses that same seam.
    /// No shell parses captured flags or environment, and no shell startup
    /// file can change the original model, profile or permission mode.
    fn resume_restart(
        &mut self,
        id: &TerminalId,
        pane_id: crate::layout::PaneId,
        plan: &AgentResumePlan,
    ) -> Result<(), String> {
        let terminal = self.state.terminals.get(id).ok_or("terminal disappeared")?;
        let _env = crate::integration::set_pending_spawn_env(terminal.launch_env.clone());
        let _run = terminal
            .run_id
            .clone()
            .map(crate::integration::set_pending_run_id);
        let _allowlist = terminal.spawned_by.as_ref().map(|_| {
            crate::integration::set_pending_spawn_allowlist(crate::spawn::allowlist::for_argv(
                &plan.argv,
                &self.state.config.spawn.env,
            ))
        });
        let (rows, cols) = self.state.estimate_pane_size();
        let runtime = crate::terminal::TerminalRuntime::spawn_argv_command(
            pane_id,
            rows,
            cols,
            terminal.cwd.clone(),
            &plan.argv,
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        )
        .map_err(|err| format!("resume spawn failed: {err}"))?;
        self.terminal_runtimes.insert(id.clone(), runtime);
        if let Some(terminal) = self.state.terminals.get_mut(id) {
            terminal.set_hibernated_resume_plan(None);
        }
        self.state.mark_session_dirty();
        Ok(())
    }

    fn restart_runtime_gone(&self, id: &TerminalId) -> bool {
        self.terminal_runtimes.get(id).is_none_or(|runtime| {
            runtime
                .child_exit()
                .is_some_and(crate::pane::ChildExit::is_reaped)
        })
    }

    fn restart_last_output(&self, id: &TerminalId) -> String {
        self.terminal_runtimes
            .get(id)
            .map(|runtime| runtime.recent_text(100).chars().take(4096).collect())
            .unwrap_or_default()
    }

    fn recover_restart(&mut self, id: &TerminalId, request: &RestartRequest) {
        self.finish_restart_lock(id);
        if self.restart_runtime_gone(id) {
            if let Some(runtime) = self.terminal_runtimes.remove(id) {
                runtime.shutdown();
            }
        }
        if let Some(terminal) = self.state.terminals.get_mut(id) {
            terminal.set_hibernated_resume_plan(Some(request.plan.clone()));
            terminal.respawn_shell_on_exit = false;
        }
        self.state.mark_session_dirty();
    }

    pub(crate) fn handle_restart_runtime_exit(&mut self, pane: crate::layout::PaneId) -> bool {
        let Some((_, state)) = self.find_pane(pane) else {
            return false;
        };
        let id = state.attached_terminal_id.clone();
        if !self.restart_runtime_gone(&id) {
            return false;
        }
        let Some(mut request) = self.state.agent_restarts.pending.remove(&id) else {
            if self
                .state
                .terminals
                .get(&id)
                .is_some_and(|t| t.hibernated_resume_plan.is_some())
            {
                if let Some(runtime) = self.terminal_runtimes.remove(&id) {
                    runtime.shutdown();
                }
                return true;
            }
            return false;
        };
        if request.phase == Phase::Stopping {
            self.state.agent_restarts.pending.insert(id, request);
            return true;
        }
        self.advance_restart(&id, &mut request, Instant::now());
        true
    }

    /// Cancellation is durable before snapshotting. Do not transfer a tree
    /// while its asynchronous reaper is still sending signals.
    pub(crate) fn cancel_agent_restarts_for_handoff(&mut self) -> std::io::Result<()> {
        if self
            .state
            .agent_restarts
            .pending
            .values()
            .any(|r| r.phase == Phase::Stopping)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "agent restart process teardown is in progress; retry handoff after it finishes",
            ));
        }
        let pending = std::mem::take(&mut self.state.agent_restarts.pending);
        for (id, request) in pending {
            self.finish_restart_lock(&id);
            if self.restart_runtime_gone(&id) {
                self.recover_restart(&id, &request);
            }
            let detail = "server handoff cancelled the restart; continuation was not delivered; request agent.restart again after handoff".to_string();
            self.report_restart(&id, &request, "restart_cancelled", detail.clone());
            if let Some(terminal) = self.state.terminals.get_mut(&id) {
                terminal.record_recap(format!("restart_cancelled: {detail}"));
            }
        }
        self.drain_pending_ui_events();
        Ok(())
    }

    fn finish_restart_lock(&mut self, id: &TerminalId) {
        if let Some(terminal) = self.state.terminals.get_mut(id) {
            terminal.restart_in_progress = false;
        }
    }

    fn answer_restart_dialog(&mut self, id: &TerminalId, request: &mut RestartRequest) {
        if request.answered_dialog {
            return;
        }
        let Some(runtime) = self.terminal_runtimes.get(id) else {
            return;
        };
        let policy = self.restart_policy(id);
        if let Some(answer) = crate::agent_restart::dialog_answer(
            &policy,
            &request.session.agent,
            &request.plan.argv,
            &runtime.recent_text(100),
        ) {
            request.answered_dialog = runtime
                .try_send_bytes(bytes::Bytes::from_static(answer))
                .is_ok();
        }
    }

    fn restart_message(&mut self, id: &TerminalId, body: String) {
        let Some(agent) = self.state.terminals.get(id).map(|t| t.agent_id.to_string()) else {
            return;
        };
        self.restart_message_to(agent, body);
    }

    fn restart_message_to(&mut self, agent: String, body: String) {
        self.restart_message_with_intent(agent, body, MsgIntent::NeedsReply);
    }

    fn restart_message_with_intent(&mut self, agent: String, body: String, intent: MsgIntent) {
        let peer = self.current_api_peer_pid.take();
        let response = self.send_message(
            "server:restart".into(),
            MsgSendParams {
                to: MessageTarget::Agent { agent },
                body,
                intent,
                correlation_id: None,
                in_reply_to: None,
                from_agent: None,
                from_host: None,
                intent_unrecognised: None,
            },
            None,
        );
        self.current_api_peer_pid = peer;
        if serde_json::from_str::<serde_json::Value>(&response)
            .ok()
            .is_some_and(|v| v.get("error").is_some())
        {
            crate::logging::agent_restart_message_failed();
        }
    }

    fn report_restart(
        &mut self,
        id: &TerminalId,
        request: &RestartRequest,
        phase: &str,
        detail: String,
    ) {
        let Some((ws, pane)) = self.restart_location(id) else {
            return;
        };
        let Some(pane_id) = self.public_pane_id(ws, pane) else {
            return;
        };
        let Some(terminal) = self.state.terminals.get(id) else {
            return;
        };
        let agent_id = terminal.agent_id.to_string();
        let parent = terminal.spawned_by.clone();
        let new_pid = self.terminal_runtimes.get(id).and_then(|r| r.child_pid());
        let rss_after = (phase == "restarted")
            .then(|| self.restarts.samples.get(id).map(|v| v.rss))
            .flatten();
        self.event_hub.push(EventEnvelope {
            event: EventKind::AgentRestart,
            data: EventData::AgentRestart {
                pane_id: pane_id.clone(),
                agent_id: agent_id.clone(),
                phase: phase.into(),
                old_pid: request.old_pid,
                new_pid,
                session: request.session.session_ref.value.clone(),
                reason: request
                    .forced
                    .clone()
                    .unwrap_or_else(|| request.reason.clone()),
                forced: request.forced.is_some(),
                stop_only: request.stop_only,
                rss_before: request.rss_before,
                rss_after,
                detail: detail.clone(),
            },
        });
        if phase == "restart_stopping" && request.forced.is_none() && !request.stop_only {
            return;
        }
        let mut summary = format!(
            "{phase}: {agent_id}, session {}, pid {:?} → {:?}, RSS {} → {:?}, reason {}: {detail}",
            request.session.session_ref.value,
            request.old_pid,
            new_pid,
            request.rss_before,
            rss_after,
            request.forced.as_deref().unwrap_or(&request.reason)
        );
        if !request.lost.is_empty() {
            summary.push_str(&format!(
                " Lost background tasks/processes: {}",
                request
                    .lost
                    .join(", ")
                    .chars()
                    .take(2048)
                    .collect::<String>()
            ));
        }
        self.state
            .file_notification(super::notifications::NotificationEntry {
                id: super::notifications::mint_notification_id(),
                title: format!("Agent {phase}"),
                body: Some(summary.clone()),
                kind: if phase == "restarted" {
                    crate::api::schema::NotificationRecordKind::Outcome
                } else {
                    crate::api::schema::NotificationRecordKind::Attention
                },
                source: crate::api::schema::NotificationSource::AgentState,
                workspace_id: Some(self.public_workspace_id(ws)),
                pane_id: Some(pane_id),
                origin_host: super::short_host_name(),
                filed_at_ms: super::notifications::now_ms(),
                seen: false,
            });
        if matches!(phase, "restart_cancelled" | "restart_stuck") {
            self.restart_message_with_intent(
                request.requester.clone().unwrap_or(agent_id),
                summary.clone(),
                MsgIntent::Fyi,
            );
        }
        if let Some(parent) = parent {
            self.restart_message_to(parent, summary);
        }
    }

    fn restart_transcript_flushed(&self, request: &RestartRequest) -> bool {
        let session = &request.session;
        if session.agent == "claude" {
            let Some(checkpoint) =
                crate::agent_restart::checkpoint(&session.session_ref.value, "stop")
            else {
                return false;
            };
            return crate::agent_restart::checkpoint_flushed(&checkpoint, request.requested_ms);
        }
        if session.agent == "opencode" {
            let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
                return false;
            };
            let xdg = std::env::var_os("XDG_DATA_HOME").map(std::path::PathBuf::from);
            return crate::opencode_transcript::db_path(&home, xdg.as_deref())
                .and_then(|path| {
                    crate::opencode_transcript::read_session(
                        &path,
                        &session.session_ref.value,
                        crate::opencode_transcript::Window::Last(1),
                    )
                    .ok()
                })
                .and_then(|read| read.messages.last().cloned())
                .is_some_and(|m| {
                    m.completed && m.final_step && m.created_ms >= request.requested_ms
                });
        }
        if session.agent == "codex" {
            let home = std::env::var_os("CODEX_HOME")
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|p| std::path::PathBuf::from(p).join(".codex"))
                });
            return home.and_then(|home| crate::codex_transcript::rollout_path(&home, &session.session_ref.value).ok().flatten())
                .and_then(|path| crate::codex_transcript::read_tail(&path).ok()).is_some_and(|(events, _)| events.iter().rev().any(|event| matches!(event,
                    crate::agent_transcript::TranscriptEvent::Message { role: crate::agent_transcript::Role::Assistant, at: Some(at), .. } if at.duration_since(std::time::UNIX_EPOCH).is_ok_and(|d| d.as_millis() >= u128::from(request.requested_ms)))));
        }
        false
    }
}

#[cfg(test)]
#[path = "agent_restart_tests.rs"]
mod tests;
