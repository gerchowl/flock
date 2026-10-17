//! Session-derived state, independent of PTYs, queues and placement (ADR-0036 §1/§2/S4).
//!
//! Own one reducer per `agent_id` and launch generation. #1439 feeds its stream
//! and publishes snapshots through agent.get/list; #1746 consumes the same seam
//! for `agent/<id>` and fleet-lite. Control answers are host facts, not stdout
//! control_response events (those answer requests initiated by the host).
//! Exit and top-level result retire all open requests; CLI control_cancel_request
//! retires only its named id. The S0 catalogue lacks cancellation, but the CLI
//! emits it and the official SDK receiver handles it by request_id:
//! <https://github.com/anthropics/claude-agent-sdk-python/blob/main/src/claude_agent_sdk/_internal/query.py>
//! Only rejected establishes a provider limit. allowed/allowed_warning clear it.
//! Unknown statuses are ignored, preserving the last known observation (ADR-0028 §3a).
//!
//! Notifications are in-process facts, NOT EventHub events. Mesh v5's
//! AgentPhaseEvent requires a pane. #1804 owns pane-free, versioned events.
//! Phase transitions here are immediate projections, not settled supervisor
//! announcements. Background and registered waits remain unknown in this slice.
#![deny(missing_docs)]

use super::claude::SessionEvent;
use crate::api::agent_state::{
    AgentState, Attention, Background, BlockedReason, HumanReason, Lifecycle, Phase, StateSource,
    StateSources, Turn, Waiting,
};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// Facts supplied by the stream owner, never inferred from screen text.
pub enum Input<'a> {
    /// Every stdout event, including unknown lines, counts as stream progress.
    Event(&'a SessionEvent),
    /// An answer was sent for this exact incoming permission request.
    ControlAnswered(&'a str),
    /// The process exited. A started input without result is crashed, not idle.
    ProcessExited,
    /// Lifecycle is owned by the registry; it outranks every session axis.
    Lifecycle(Lifecycle),
    /// Supplied-clock observation, including silence and settling boundaries.
    Observe,
}

/// One projection for the keyed agent. This is not a persisted event envelope.
#[derive(Debug, Clone)]
pub struct Update {
    /// Stable identity, never a fabricated pane or terminal id.
    pub agent_id: String,
    /// ADR-0028's unchanged axis vocabulary and phase precedence.
    pub state: AgentState,
    /// Immediate phase edge since the previous reduction, if any.
    pub phase_changed: Option<(Phase, Phase)>,
    /// All currently open permission/question requests, in id order.
    pub needs_human: Vec<(String, HumanReason)>,
    /// Last stdout's Unix ms, once per silent interval; no phase is changed.
    pub stalled_since_ms: Option<u64>,
    /// Next monotonic observation boundary, if silence or settling needs one.
    pub observe_at: Option<Instant>,
}

/// Synchronous integration seam. Implementations must not block the stream pump.
/// A caller that needs a channel must handle capacity/disconnection explicitly.
pub trait StateSink {
    /// Publish this agent's snapshot and in-process observations.
    fn publish(&mut self, update: Update);
}

/// Pure per-agent reducer. Replace it on a new launch; do not share across agents.
/// No timers, process inspection, I/O, event hub, retry or automatic input exists.
pub struct SessionState {
    agent_id: String,
    turn: Turn,
    turn_since: Option<Instant>,
    lifecycle: Lifecycle,
    controls: BTreeMap<String, HumanReason>,
    tools: BTreeSet<String>,
    limit: Option<Option<u64>>,
    crashed: bool,
    started: bool,
    stdout: Option<(Instant, u64)>,
    stalled: bool,
    phase: Phase,
    phase_since: Option<Instant>,
}
impl SessionState {
    /// Start with unknown turn/background and a registry-owned live lifecycle.
    pub fn new(agent_id: String) -> Self {
        Self {
            agent_id,
            turn: Turn::Unknown,
            turn_since: None,
            lifecycle: Lifecycle::Live,
            controls: BTreeMap::new(),
            tools: BTreeSet::new(),
            limit: None,
            crashed: false,
            started: false,
            stdout: None,
            stalled: false,
            phase: Phase::Unknown,
            phase_since: None,
        }
    }

    /// Set the initial registry lifecycle before any stream observations exist.
    /// Used for cold restored records, without manufacturing a clock or event.
    pub(super) fn set_initial_lifecycle(&mut self, lifecycle: Lifecycle) {
        self.lifecycle = lifecycle;
        self.phase = AgentState::derive(
            Turn::Unknown,
            Background::Unknown,
            Attention::Unknown,
            lifecycle,
            Waiting::Unknown,
            None,
            StateSources::default(),
        )
        .phase;
    }

    /// Reduce one fact using monotonic time for ages and Unix ms for reset times.
    /// `stall_after` is the caller's `[sessions] stall_ms` policy, not a guessed
    /// turn timeout. Call Observe at its boundary; never poll or kill on silence.
    pub fn reduce(
        &mut self,
        input: Input<'_>,
        now: Instant,
        unix_ms: u64,
        stall_after: Duration,
    ) -> Update {
        match input {
            Input::Event(event) => {
                self.stdout = Some((now, unix_ms));
                self.stalled = false;
                self.event(event, now);
            }
            Input::ControlAnswered(id) => {
                self.controls.remove(id);
            }
            Input::ProcessExited => {
                self.crashed = self.started;
                self.controls.clear();
                self.tools.clear();
            }
            Input::Lifecycle(lifecycle) => {
                self.lifecycle = lifecycle;
            }
            Input::Observe => (),
        }
        let attention = if let Some(reset) = self.limit {
            Attention::Blocked {
                reason: BlockedReason::ProviderLimit,
                retry_after_ms: reset.map(|reset| reset.saturating_sub(unix_ms)),
            }
        } else if self.crashed {
            Attention::Blocked {
                reason: BlockedReason::Crashed,
                retry_after_ms: None,
            }
        } else if let Some(reason) = self.controls.values().next() {
            Attention::NeedsHuman {
                reason: *reason,
                question: None,
            }
        } else {
            Attention::None
        };
        // ADR-0028 §1 masks turn while attention is set. Retain the underlying
        // turn boundary so answering/cancelling a prompt restores its known state.
        let turn = if attention == Attention::None {
            self.turn
        } else {
            Turn::Unknown
        };
        let mut state = AgentState::derive(
            turn,
            Background::Unknown,
            attention,
            self.lifecycle,
            Waiting::Unknown,
            self.turn_since
                .map(|at| millis(now.saturating_duration_since(at))),
            StateSources {
                turn: Some(StateSource::Session),
                attention: Some(StateSource::Session),
                ..StateSources::default()
            },
        );
        let phase_changed = (self.phase != state.phase).then_some((self.phase, state.phase));
        if phase_changed.is_some() {
            self.phase = state.phase;
            self.phase_since = Some(now);
        }
        state.held_ms = self
            .phase_since
            .filter(|_| self.lifecycle == Lifecycle::Live)
            .map(|at| millis(now.saturating_duration_since(at)));
        let stalled_since_ms = self
            .stdout
            .filter(|(at, _)| {
                self.started
                    && !self.stalled
                    && !self.crashed
                    && self.lifecycle == Lifecycle::Live
                    && self.controls.is_empty()
                    && self.tools.is_empty()
                    && now.saturating_duration_since(*at) >= stall_after
            })
            .map(|(_, unix)| unix);
        self.stalled |= stalled_since_ms.is_some();
        let stall_due = self
            .stdout
            .filter(|_| {
                self.started
                    && !self.stalled
                    && !self.crashed
                    && self.lifecycle == Lifecycle::Live
                    && self.controls.is_empty()
                    && self.tools.is_empty()
            })
            .and_then(|(at, _)| at.checked_add(stall_after));
        let settle_due = self
            .turn_since
            .filter(|_| state.phase == Phase::Settling)
            .and_then(|at| {
                at.checked_add(Duration::from_millis(crate::api::agent_state::SETTLE_MS))
            });
        let observe_at = [stall_due, settle_due].into_iter().flatten().min();

        Update {
            agent_id: self.agent_id.clone(),
            state,
            phase_changed,
            needs_human: self
                .controls
                .iter()
                .map(|(id, reason)| (id.clone(), *reason))
                .collect(),
            stalled_since_ms,
            observe_at,
        }
    }

    /// Reduce then publish through the registry/wire integration seam.
    pub fn publish(
        &mut self,
        input: Input<'_>,
        now: Instant,
        unix_ms: u64,
        stall_after: Duration,
        sink: &mut impl StateSink,
    ) {
        sink.publish(self.reduce(input, now, unix_ms, stall_after));
    }

    fn working(&mut self, now: Instant) {
        if self.turn != Turn::Working {
            self.turn_since = Some(now);
        }
        if !self.started {
            self.limit = None;
        }
        self.turn = Turn::Working;
        self.started = true;
        self.crashed = false;
    }

    fn event(&mut self, event: &SessionEvent, now: Instant) {
        match event {
            SessionEvent::CommandLifecycle(e) if e.state == "started" => self.working(now),
            SessionEvent::Status(e)
                if e.status.as_deref() == Some("requesting")
                    && e.parent_tool_use_id
                        .as_ref()
                        .and_then(Option::as_ref)
                        .is_none() =>
            {
                self.working(now)
            }
            SessionEvent::MessageStart(e)
                if e.parent_tool_use_id
                    .as_ref()
                    .and_then(Option::as_ref)
                    .is_none() =>
            {
                self.working(now)
            }
            SessionEvent::Success(e)
                if e.parent_tool_use_id
                    .as_ref()
                    .and_then(Option::as_ref)
                    .is_none() =>
            {
                self.result(now)
            }
            SessionEvent::ExecutionError(e)
                if e.parent_tool_use_id
                    .as_ref()
                    .and_then(Option::as_ref)
                    .is_none() =>
            {
                self.result(now)
            }
            SessionEvent::CanUseTool(e) => {
                self.controls.insert(
                    e.request_id.clone(),
                    if e.request.tool_name == "AskUserQuestion" {
                        HumanReason::Question
                    } else {
                        HumanReason::Permission
                    },
                );
            }
            SessionEvent::RateLimit(e) => {
                match e.rate_limit_info.status.as_str() {
                    "allowed" | "allowed_warning" => self.limit = None,
                    "rejected" => {
                        // SDK resets_at is a Unix timestamp (seconds), not a duration.
                        // https://github.com/anthropics/claude-agent-sdk-python/blob/main/src/claude_agent_sdk/types.py
                        self.limit = Some(
                            e.rate_limit_info
                                .resets_at
                                .as_ref()
                                .and_then(Option::as_ref)
                                .and_then(serde_json::Value::as_u64)
                                .and_then(|s| s.checked_mul(1000)),
                        );
                    }
                    _ => (), // Unknown future status is not evidence of a new limit.
                }
            }
            SessionEvent::ControlCancel(e) => {
                self.controls.remove(&e.request_id);
            }

            SessionEvent::ContentBlockStart(e) => self.tool_start(e.event.get("content_block")),
            SessionEvent::Assistant(e) => {
                for block in e
                    .message
                    .get("content")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    self.tool_start(Some(block));
                }
            }
            SessionEvent::User(e) => {
                for block in e
                    .message
                    .get("content")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if block["type"] == "tool_result" {
                        if let Some(id) = block["tool_use_id"].as_str() {
                            self.tools.remove(id);
                        }
                    }
                }
            }
            _ => (),
        }
    }
    fn tool_start(&mut self, block: Option<&serde_json::Value>) {
        if let Some(block) = block.filter(|b| b["type"] == "tool_use") {
            if let Some(id) = block["id"].as_str() {
                self.tools.insert(id.into());
            }
        }
    }
    fn result(&mut self, now: Instant) {
        if self.turn != Turn::Idle {
            self.turn_since = Some(now);
        }
        self.turn = Turn::Idle;
        self.started = false;
        self.crashed = false;
        self.tools.clear();
        self.controls.clear();
    }
}
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
