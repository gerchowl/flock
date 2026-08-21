use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

// Effective state arbitration is intentionally centralized here. Hooks are the
// default authority for agent-owned internal state, but a narrow set of strong
// visible screen signals can veto stale hook reports. Precedence is:
// strong visible blocker > visible working/idle recovery > hook > fallback.
// Process-exit updates clear matching hook authority before recomputing state.

use crate::detect::{Agent, AgentState};
use crate::terminal::{AgentId, TerminalId};

#[path = "metadata.rs"]
mod metadata;
pub use metadata::{AgentMetadata, AgentMetadataReport, EffectivePresentation};

#[path = "header_fields.rs"]
mod header_fields;
pub use header_fields::{
    compact_header_fields, middle_truncate_chars, validate_header_field, HeaderField,
    HeaderFieldError,
};

#[path = "prompt_history.rs"]
mod prompt_history;
#[cfg(test)]
pub use prompt_history::MAX_PROMPT_HISTORY_ENTRIES;
pub use prompt_history::{
    append_with_cap as append_prompt_history_with_cap, PromptHistoryEntry, PromptHistoryKind,
};

const CLAUDE_WORKING_HOLD: Duration = Duration::from_millis(1200);
const STALE_HOOK_IDLE_GRACE: Duration = Duration::from_secs(2);

/// How long a hook report stays authoritative without fresh traffic (#309).
///
/// Hook authority used to be *durable*: it was cleared on process exit, agent
/// change and respawn, but never on age, and `reported_at` was only ever used
/// for relative ordering against the screen. A host that registers
/// `UserPromptSubmit` but not `Stop` — the shape a read-only, externally-owned
/// `settings.json` produces — therefore pinned every pane to `Working`
/// forever, and the only route back to `Idle` was the screen veto below.
///
/// With a TTL, an authority that has gone quiet simply stops winning and the
/// screen takes over cleanly, with no veto grace and nothing to flap against.
/// This is deliberately much longer than a turn boundary: while hooks work,
/// every prompt and every stop refreshes them, so the TTL never fires. It is a
/// bound on "this agent stopped talking to us", not a heartbeat.
const HOOK_AUTHORITY_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookAuthority {
    pub source: String,
    pub agent_label: String,
    pub state: AgentState,
    pub message: Option<String>,
    pub custom_status: Option<String>,
    pub reported_at: Instant,
    pub session_ref: Option<crate::agent_resume::AgentSessionRef>,
}

/// Which authority decided the effective state on the last recompute (#309).
///
/// Carried on every [`EffectiveStateChange`] so the state-change log can name
/// the deciding source. A ten-second false-`Idle` was only diagnosable with a
/// bespoke capture rig because nothing recorded this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateAuthority {
    /// A live blocker on screen overruled a non-blocked hook.
    VisibleBlocker,
    /// Live working chrome overruled a hook that claimed idle/blocked.
    VisibleWorking,
    /// The screen showed idle long enough to stale a hook that claimed
    /// working/blocked (`STALE_HOOK_IDLE_GRACE`).
    VisibleIdleVeto,
    /// A fresh hook report.
    Hook,
    /// A hook reported once and went quiet past [`HOOK_AUTHORITY_TTL`], so the
    /// screen decided instead.
    HookExpired,
    /// No hook has ever reported; the screen decided.
    Screen,
}

impl StateAuthority {
    pub fn label(self) -> &'static str {
        match self {
            Self::VisibleBlocker => "visible_blocker",
            Self::VisibleWorking => "visible_working",
            Self::VisibleIdleVeto => "visible_idle_veto",
            Self::Hook => "hook",
            Self::HookExpired => "hook_expired",
            Self::Screen => "screen",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveStateChange {
    pub previous_agent_label: Option<String>,
    pub previous_known_agent: Option<Agent>,
    pub previous_state: AgentState,
    pub previous_presentation: EffectivePresentation,
    pub agent_label: Option<String>,
    pub known_agent: Option<Agent>,
    pub state: AgentState,
    pub presentation: EffectivePresentation,
    /// Which source decided `state` (#309).
    pub authority: StateAuthority,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TerminalStateMutation {
    pub effective_state_change: Option<EffectiveStateChange>,
    pub session_ref_changed: bool,
    /// Session ref a report just attached to this terminal. A session id is
    /// unique to one live agent process, so the caller evicts the same ref
    /// from every other terminal (mis-deliveries from stale pane-id envs).
    pub applied_session_ref: Option<crate::agent_resume::AgentSessionRef>,
}

/// Pure state for a server-owned terminal.
///
/// During the migration this is still one-to-one with a pane-backed PTY, but
/// pane/view state no longer owns terminal identity, cwd, labels, or agent
/// metadata.
pub struct TerminalState {
    pub id: TerminalId,
    /// Fleet-global, restart-stable identity for the agent in this pane.
    ///
    /// `id` (the `TerminalId`) survives a pane move but is RE-MINTED on every
    /// server start — it names a running PTY, not an agent. Public pane ids
    /// (`w3:p1`) name a *placement*, so they change when the pane moves and
    /// mean nothing on another host. Neither can address an agent across a
    /// restart or a machine, which is why a cross-machine message could not
    /// say who sent it.
    ///
    /// This is minted once when the pane is created, persisted in the session
    /// snapshot, and never rewritten. Address ≠ location: host and pane are
    /// resolvable *metadata* about an agent, not its name.
    pub agent_id: AgentId,
    pub cwd: PathBuf,
    pub detected_agent: Option<Agent>,
    pub fallback_state: AgentState,
    fallback_visible_blocker: bool,
    fallback_visible_idle: bool,
    fallback_visible_working: bool,
    fallback_observed_at: Option<Instant>,
    /// The last user prompt submitted to this pane's agent, reported by the
    /// integration hook (Claude's UserPromptSubmit). Shown in the pane header
    /// collapsed view (mirrors the latest prompt entry in `prompt_history` so
    /// the byte-identical render path for the legacy single-prompt case is
    /// preserved).
    pub last_prompt: Option<String>,
    /// Per-pane prompt + recap scrollback (issue #96). Chronological,
    /// timestamped entries; capped at
    /// [`MAX_PROMPT_HISTORY_ENTRIES`] entries (drop oldest whole entries).
    /// Ephemeral by design — never persisted into session snapshots.
    pub prompt_history: Vec<PromptHistoryEntry>,
    /// Bumped on every `prompt_history` mutation.
    ///
    /// The panel's row layout is cached against this (#254): rows are wrapped,
    /// so re-deriving them per frame over a full transcript is not viable, and
    /// a length check would miss an in-place replacement like hydration that
    /// happens to preserve the count.
    prompt_history_generation: u64,
    /// The (session, detail) pair that last hydrated `prompt_history` (#246).
    /// The pair, not the session alone, so cycling the panel's detail level
    /// re-arms the reader without touching every hook path — a burst of hook
    /// reports at the same session+detail still spawns only one reader.
    hydrated_transcript: Option<(String, crate::agent_transcript::TranscriptDetail)>,
    /// Generation the last read was armed at, versus the one callers want.
    /// A counter rather than a flag so a burst of "the file moved" signals
    /// between reads still collapses to a single pending read (#254).
    hydrated_transcript_generation: u64,
    wanted_transcript_generation: u64,
    /// Ring length when a transcript read was armed. Entries past it arrived
    /// after the worker opened the file, so hydration carries them over
    /// instead of replacing them away (#246). `Some` also means "in flight".
    hydration_arm_index: Option<usize>,
    /// Session-promoted header fields ("chips": containers, progress, custom
    /// KV), insertion-ordered, optionally TTL-expiring. Ephemeral by design —
    /// never persisted into session snapshots.
    pub header_fields: Vec<HeaderField>,
    /// Latched once an agent is ever seen in this pane: keeps the pane header
    /// reservation stable so the PTY doesn't resize on detection flaps.
    pub header_reserved: bool,
    /// When the effective agent state last transitioned. Drives the
    /// oldest-first ordering of the attention queue.
    pub state_changed_at: Option<Instant>,
    /// Free-text activity from the agent's own status line while Working
    /// (e.g. Claude's "Implementing the parser"). Cleared by the detector
    /// when the agent stops working.
    pub live_activity: Option<String>,
    stale_hook_idle_since: Option<Instant>,
    /// Which authority decided the current `state` (#309); surfaced on every
    /// change so the log names the source rather than only the outcome.
    last_state_authority: StateAuthority,
    pub hook_authority: Option<HookAuthority>,
    pub agent_metadata: HashMap<String, AgentMetadata>,
    pub persisted_agent_session: Option<crate::agent_resume::PersistedAgentSession>,
    pub manual_label: Option<String>,
    pub agent_name: Option<String>,
    /// The `FLOCK_RUN_ID` this pane's agent was spawned under (#332).
    ///
    /// Minted per scheduler- or agent-initiated spawn, stamped into the
    /// child's environment, and appended to every commit that child makes
    /// as an `Agent-Run:` trailer — which is what `flk revert-run` joins
    /// on. It existed on the `AgentForked` event and in the child's env,
    /// but nowhere a reader could see it, so joining "this agent" to "these
    /// commits" to "that PR" meant inferring from `cwd`.
    ///
    /// `None` for an operator-started pane: run ids mark work an agent or
    /// the scheduler initiated, and minting one for a human's own pane
    /// would make `flk revert-run` offer to revert the operator's work.
    pub run_id: Option<String>,
    hook_report_sequences: HashMap<String, u64>,
    metadata_report_sequences: HashMap<String, u64>,
    pub state: AgentState,
    pub revision: u64,
    pub launch_argv: Option<Vec<String>>,
    pub respawn_shell_on_exit: bool,
    pub pending_agent_resume_plan: Option<crate::agent_resume::AgentResumePlan>,
    /// #175 C3: stashed resume plan for a pane the operator (or the
    /// hibernation check) parked. When set, the pane's child has been asked
    /// to exit; on child exit the pane is KEPT (no PaneClosed, no respawn),
    /// and the next focus / explicit `agent.resume` respawns the argv into
    /// the same pane and terminal.
    pub hibernated_resume_plan: Option<crate::agent_resume::AgentResumePlan>,
}

impl TerminalState {
    /// Mint a terminal with a fresh agent identity. Restore uses
    /// [`Self::with_agent_id`] so a persisted agent keeps its name.
    pub fn new(id: TerminalId, cwd: PathBuf) -> Self {
        let agent_id = AgentId::alloc(&crate::app::short_host_name());
        Self::with_agent_id(id, agent_id, cwd)
    }

    /// Rebuild a terminal around an identity that already exists — the restore
    /// path. Minting here instead would re-address every agent on restart and
    /// orphan any in-flight message thread.
    pub fn with_agent_id(id: TerminalId, agent_id: AgentId, cwd: PathBuf) -> Self {
        Self {
            id,
            agent_id,
            cwd,
            detected_agent: None,
            fallback_state: AgentState::Unknown,
            fallback_visible_blocker: false,
            fallback_visible_idle: false,
            fallback_visible_working: false,
            fallback_observed_at: None,
            last_prompt: None,
            prompt_history: Vec::new(),
            prompt_history_generation: 0,
            hydrated_transcript: None,
            hydrated_transcript_generation: 0,
            wanted_transcript_generation: 0,
            hydration_arm_index: None,
            header_fields: Vec::new(),
            header_reserved: false,
            state_changed_at: None,
            live_activity: None,
            stale_hook_idle_since: None,
            last_state_authority: StateAuthority::Screen,
            hook_authority: None,
            agent_metadata: HashMap::new(),
            persisted_agent_session: None,
            manual_label: None,
            agent_name: None,
            run_id: None,
            hook_report_sequences: HashMap::new(),
            metadata_report_sequences: HashMap::new(),
            state: AgentState::Unknown,
            revision: 0,
            launch_argv: None,
            respawn_shell_on_exit: false,
            pending_agent_resume_plan: None,
            hibernated_resume_plan: None,
        }
    }

    pub fn with_launch_argv(mut self, argv: Vec<String>) -> Self {
        self.launch_argv = Some(argv);
        self
    }

    pub fn with_respawn_shell_on_exit(mut self) -> Self {
        self.respawn_shell_on_exit = true;
        self
    }

    pub fn with_pending_agent_resume_plan(
        mut self,
        plan: crate::agent_resume::AgentResumePlan,
    ) -> Self {
        self.pending_agent_resume_plan = Some(plan);
        self
    }

    /// Update the scraped spinner activity, holding the last value through
    /// transient scrape misses while the agent is still working.
    ///
    /// The detector republishes whenever the spinner text changes, and a line
    /// caught mid-redraw (or between verb changes) scrapes as `None`. Writing
    /// that `None` straight through clears `live_activity` for a frame, so the
    /// sidebar status row flickers between the spinner text ("the original")
    /// and the bare state label ("ours"). Hold the last activity until the
    /// agent actually leaves the working state.
    pub fn update_live_activity(&mut self, activity: Option<String>, detected_state: AgentState) {
        if activity.is_some() {
            self.live_activity = activity;
        } else if detected_state != AgentState::Working {
            self.live_activity = None;
        }
    }

    #[cfg(test)]
    pub fn set_detected_state(
        &mut self,
        agent: Option<Agent>,
        fallback_state: AgentState,
    ) -> Option<EffectiveStateChange> {
        self.set_detected_state_with_visible_blocker(agent, fallback_state, false, false, false)
    }

    #[cfg(test)]
    pub fn set_detected_state_with_mutation(
        &mut self,
        agent: Option<Agent>,
        fallback_state: AgentState,
    ) -> TerminalStateMutation {
        self.set_detected_state_with_screen_signals_at(
            agent,
            fallback_state,
            false,
            false,
            false,
            false,
            Instant::now(),
        )
    }

    #[cfg(test)]
    pub fn set_detected_state_with_visible_blocker(
        &mut self,
        agent: Option<Agent>,
        fallback_state: AgentState,
        visible_blocker: bool,
        visible_idle: bool,
        process_exited: bool,
    ) -> Option<EffectiveStateChange> {
        self.set_detected_state_with_screen_signals_at(
            agent,
            fallback_state,
            visible_blocker,
            visible_idle,
            false,
            process_exited,
            Instant::now(),
        )
        .effective_state_change
    }

    pub fn set_detected_state_with_screen_signals_at(
        &mut self,
        agent: Option<Agent>,
        fallback_state: AgentState,
        visible_blocker: bool,
        visible_idle: bool,
        visible_working: bool,
        process_exited: bool,
        now: Instant,
    ) -> TerminalStateMutation {
        let previous_agent_label = self.effective_agent_label().map(str::to_string);
        let previous_known_agent = self.effective_known_agent();
        let previous_state = self.state;
        let previous_presentation = self.effective_presentation_for_state_at(previous_state, now);
        let previous_detected_agent = self.detected_agent;
        let previous_session = self.current_session_identity_for_persistence();
        if agent.is_some() {
            self.header_reserved = true;
        }
        self.detected_agent = agent;
        self.fallback_state = fallback_state;
        self.fallback_visible_blocker = visible_blocker && fallback_state == AgentState::Blocked;
        self.fallback_visible_idle = visible_idle && fallback_state == AgentState::Idle;
        self.fallback_visible_working = visible_working && fallback_state == AgentState::Working;
        self.fallback_observed_at = Some(now);
        if process_exited
            && self.hook_authority_not_newer_than(now)
            && self.hook_authority.as_ref().is_some_and(|authority| {
                crate::detect::parse_agent_label(&authority.agent_label) == agent
            })
        {
            self.hook_authority = None;
            self.stale_hook_idle_since = None;
        }
        if self.hook_authority_not_newer_than(now)
            && (self.hook_authority_conflicts_with_detected_agent(agent)
                || (previous_detected_agent.is_some()
                    && agent != previous_detected_agent
                    && self.hook_authority.as_ref().is_some_and(|authority| {
                        crate::detect::parse_agent_label(&authority.agent_label)
                            == previous_detected_agent
                    })))
        {
            self.hook_authority = None;
            self.stale_hook_idle_since = None;
        }
        let detected_agent_changed_or_disappeared =
            previous_detected_agent.is_some() && agent != previous_detected_agent;
        let persisted_agent_was_previously_detected =
            self.persisted_agent_session_belongs_to_detected_agent(previous_detected_agent);
        if self.persisted_agent_session_conflicts_with_detected_agent(agent)
            || detected_agent_changed_or_disappeared && persisted_agent_was_previously_detected
        {
            self.persisted_agent_session = None;
        }
        self.update_stale_hook_idle_window(now);
        TerminalStateMutation {
            effective_state_change: self.recompute_effective_state(
                previous_agent_label,
                previous_known_agent,
                previous_state,
                previous_presentation,
                now,
            ),
            session_ref_changed: previous_session
                != self.current_session_identity_for_persistence(),
            applied_session_ref: None,
        }
    }

    #[cfg(test)]
    pub fn set_hook_authority(
        &mut self,
        source: String,
        agent_label: String,
        state: AgentState,
        message: Option<String>,
        seq: Option<u64>,
    ) -> Option<EffectiveStateChange> {
        self.set_hook_authority_with_custom_status(source, agent_label, state, message, None, seq)
    }

    #[cfg(test)]
    pub fn set_hook_authority_with_custom_status(
        &mut self,
        source: String,
        agent_label: String,
        state: AgentState,
        message: Option<String>,
        custom_status: Option<String>,
        seq: Option<u64>,
    ) -> Option<EffectiveStateChange> {
        self.set_hook_authority_with_custom_status_at(
            source,
            agent_label,
            state,
            message,
            custom_status,
            None,
            seq,
            Instant::now(),
        )
        .and_then(|mutation| mutation.effective_state_change)
    }

    pub fn set_hook_authority_with_session_ref(
        &mut self,
        source: String,
        agent_label: String,
        state: AgentState,
        message: Option<String>,
        custom_status: Option<String>,
        session_ref: Option<crate::agent_resume::AgentSessionRef>,
        seq: Option<u64>,
    ) -> Option<TerminalStateMutation> {
        self.set_hook_authority_with_custom_status_at(
            source,
            agent_label,
            state,
            message,
            custom_status,
            session_ref,
            seq,
            Instant::now(),
        )
    }

    pub fn set_hook_authority_with_custom_status_at(
        &mut self,
        source: String,
        agent_label: String,
        state: AgentState,
        message: Option<String>,
        custom_status: Option<String>,
        session_ref: Option<crate::agent_resume::AgentSessionRef>,
        seq: Option<u64>,
        now: Instant,
    ) -> Option<TerminalStateMutation> {
        if !self.accept_hook_report(&source, seq) {
            return None;
        }

        let previous_agent_label = self.effective_agent_label().map(str::to_string);
        let previous_known_agent = self.effective_known_agent();
        let previous_state = self.state;
        let previous_presentation = self.effective_presentation_for_state_at(previous_state, now);
        let previous_session = self.current_session_identity_for_persistence();
        if self.known_agent_label_conflicts_with_detected_agent(&agent_label) {
            return None;
        }
        let session_ref = session_ref.map(|session_ref| {
            self.conflicting_current_session_ref(&source, &agent_label, &session_ref)
                .unwrap_or(session_ref)
        });
        self.persisted_agent_session = None;
        self.header_reserved = true;
        let applied_session_ref = session_ref.clone();
        self.hook_authority = Some(HookAuthority {
            source,
            agent_label,
            state,
            message,
            custom_status,
            reported_at: now,
            session_ref,
        });
        self.stale_hook_idle_since = None;
        let current_session = self.current_session_identity_for_persistence();
        Some(TerminalStateMutation {
            effective_state_change: self.recompute_effective_state(
                previous_agent_label,
                previous_known_agent,
                previous_state,
                previous_presentation,
                now,
            ),
            session_ref_changed: previous_session != current_session,
            applied_session_ref,
        })
    }

    fn hook_authority_not_newer_than(&self, observed_at: Instant) -> bool {
        self.hook_authority
            .as_ref()
            .is_none_or(|authority| authority.reported_at <= observed_at)
    }

    fn fallback_not_older_than_hook(&self) -> bool {
        self.hook_authority.as_ref().is_none_or(|authority| {
            self.fallback_observed_at
                .is_some_and(|observed_at| authority.reported_at <= observed_at)
        })
    }

    fn hook_authority_conflicts_with_detected_agent(&self, detected_agent: Option<Agent>) -> bool {
        let Some(detected_agent) = detected_agent else {
            return false;
        };
        self.hook_authority.as_ref().is_some_and(|authority| {
            crate::detect::parse_agent_label(&authority.agent_label)
                .is_some_and(|hook_agent| hook_agent != detected_agent)
        })
    }

    fn persisted_agent_session_conflicts_with_detected_agent(
        &self,
        detected_agent: Option<Agent>,
    ) -> bool {
        let Some(detected_agent) = detected_agent else {
            return false;
        };
        self.persisted_agent_session
            .as_ref()
            .and_then(|session| crate::detect::parse_agent_label(&session.agent))
            .is_some_and(|agent| agent != detected_agent)
    }

    fn persisted_agent_session_belongs_to_detected_agent(
        &self,
        detected_agent: Option<Agent>,
    ) -> bool {
        let Some(detected_agent) = detected_agent else {
            return false;
        };
        self.persisted_agent_session
            .as_ref()
            .and_then(|session| crate::detect::parse_agent_label(&session.agent))
            .is_some_and(|agent| agent == detected_agent)
    }

    fn persisted_agent_session_matches(&self, source: &str, agent: &str) -> bool {
        self.persisted_agent_session
            .as_ref()
            .is_some_and(|session| session.source == source && session.agent == agent)
    }

    fn current_session_identity_for_persistence(
        &self,
    ) -> Option<(
        String,
        String,
        crate::agent_resume::AgentSessionRefKind,
        String,
    )> {
        if let Some(authority) = self.hook_authority.as_ref() {
            if let Some(session_ref) = authority.session_ref.as_ref() {
                return Some((
                    authority.source.clone(),
                    authority.agent_label.clone(),
                    session_ref.kind,
                    session_ref.value.clone(),
                ));
            }
        }
        self.persisted_agent_session.as_ref().map(|session| {
            (
                session.source.clone(),
                session.agent.clone(),
                session.session_ref.kind,
                session.session_ref.value.clone(),
            )
        })
    }

    /// When the incoming `session_ref` came from a child terminal that
    /// inherited the owning pane's env (a nested agent session report), the
    /// child reports a different session id under the same source+agent. The
    /// owning pane's restored/current session id is authoritative, so we keep
    /// it and ignore the nested clobber. Ported from herdr #511.
    fn conflicting_current_session_ref(
        &self,
        source: &str,
        agent_label: &str,
        session_ref: &crate::agent_resume::AgentSessionRef,
    ) -> Option<crate::agent_resume::AgentSessionRef> {
        self.current_session_identity_for_persistence().and_then(
            |(current_source, current_agent, current_kind, current_value)| {
                (current_source == source
                    && current_agent == agent_label
                    && current_kind == crate::agent_resume::AgentSessionRefKind::Id
                    && session_ref.kind == crate::agent_resume::AgentSessionRefKind::Id
                    && (current_kind != session_ref.kind || current_value != session_ref.value))
                    .then_some(crate::agent_resume::AgentSessionRef {
                        kind: current_kind,
                        value: current_value,
                    })
            },
        )
    }

    pub fn set_persisted_agent_session(
        &mut self,
        session: crate::agent_resume::PersistedAgentSession,
    ) {
        self.persisted_agent_session = Some(session);
    }

    /// Returns true when the current persisted identity is an id for the same
    /// source+agent but with a *different* value than the incoming report, and
    /// the `session_start_source` does not authorize replacement. This is the
    /// guard that keeps a nested `claude -p` (which inherits the pane env and
    /// reports its own session id at startup) from clobbering the restored id
    /// belonging to the original pane occupant.
    fn has_conflicting_current_session_ref(
        &self,
        source: &str,
        agent_label: &str,
        session_ref: &crate::agent_resume::AgentSessionRef,
        session_start_source: Option<&str>,
    ) -> bool {
        let Some((current_source, current_agent, current_kind, current_value)) =
            self.current_session_identity_for_persistence()
        else {
            return false;
        };
        if current_source != source || current_agent != agent_label {
            return false;
        }
        if current_kind != crate::agent_resume::AgentSessionRefKind::Id
            || session_ref.kind != crate::agent_resume::AgentSessionRefKind::Id
        {
            return false;
        }
        if current_value == session_ref.value {
            return false;
        }
        !Self::session_start_source_allows_session_replacement(
            source,
            agent_label,
            session_start_source,
        )
    }

    fn session_start_source_allows_session_replacement(
        source: &str,
        agent_label: &str,
        session_start_source: Option<&str>,
    ) -> bool {
        source == "flock:claude"
            && agent_label == "claude"
            && matches!(session_start_source, Some("clear" | "resume" | "compact"))
    }

    pub fn set_agent_session_ref(
        &mut self,
        source: String,
        agent_label: String,
        session_ref: Option<crate::agent_resume::AgentSessionRef>,
        seq: Option<u64>,
    ) -> Option<TerminalStateMutation> {
        self.set_agent_session_ref_for_session_start(source, agent_label, session_ref, seq, None)
    }

    /// Same as [`set_agent_session_ref`] but also honours the Claude Code
    /// `SessionStart` `source` field. A startup-style report (no source, or
    /// `startup`) that would replace an existing restored Claude id is
    /// rejected — it's almost certainly a nested `claude -p` inheriting the
    /// pane environment. `clear`, `resume`, and `compact` are real identity
    /// changes the user just triggered, and we accept them.
    pub fn set_agent_session_ref_for_session_start(
        &mut self,
        source: String,
        agent_label: String,
        session_ref: Option<crate::agent_resume::AgentSessionRef>,
        seq: Option<u64>,
        session_start_source: Option<String>,
    ) -> Option<TerminalStateMutation> {
        self.header_reserved = true;
        let session_ref = session_ref?;
        if !self.accept_hook_report(&source, seq) {
            return None;
        }
        if self.known_agent_label_conflicts_with_detected_agent(&agent_label) {
            return None;
        }
        if self.has_conflicting_current_session_ref(
            &source,
            &agent_label,
            &session_ref,
            session_start_source.as_deref(),
        ) {
            return None;
        }

        let previous_session = self.current_session_identity_for_persistence();
        let applied_session_ref = session_ref.clone();
        self.persisted_agent_session = Some(crate::agent_resume::PersistedAgentSession {
            source,
            agent: agent_label,
            session_ref,
        });
        let current_session = self.current_session_identity_for_persistence();
        Some(TerminalStateMutation {
            effective_state_change: None,
            session_ref_changed: previous_session != current_session,
            applied_session_ref: Some(applied_session_ref),
        })
    }

    /// Drop any agent session identity equal to `session_ref`. A session id
    /// belongs to exactly one live agent process; when an ancestry-verified
    /// report lands that ref on another terminal, every other holder was a
    /// mis-delivery from an environment carrying a stale pane id.
    pub fn evict_session_ref(
        &mut self,
        session_ref: &crate::agent_resume::AgentSessionRef,
    ) -> bool {
        let mut changed = false;
        if let Some(authority) = self.hook_authority.as_mut() {
            if authority.session_ref.as_ref() == Some(session_ref) {
                authority.session_ref = None;
                changed = true;
            }
        }
        if self
            .persisted_agent_session
            .as_ref()
            .is_some_and(|session| &session.session_ref == session_ref)
        {
            self.persisted_agent_session = None;
            changed = true;
        }
        changed
    }

    fn known_agent_label_conflicts_with_detected_agent(&self, agent_label: &str) -> bool {
        let Some(detected_agent) = self.detected_agent else {
            return false;
        };
        crate::detect::parse_agent_label(agent_label)
            .is_some_and(|hook_agent| hook_agent != detected_agent)
    }

    fn accept_hook_report(&mut self, source: &str, seq: Option<u64>) -> bool {
        let Some(seq) = seq else {
            return !self.hook_report_sequences.contains_key(source);
        };

        if self
            .hook_report_sequences
            .get(source)
            .is_some_and(|last_seq| seq <= *last_seq)
        {
            return false;
        }

        self.hook_report_sequences.insert(source.to_string(), seq);
        true
    }

    #[cfg(test)]
    pub fn clear_hook_authority(
        &mut self,
        source: Option<&str>,
        seq: Option<u64>,
    ) -> Option<EffectiveStateChange> {
        self.clear_hook_authority_with_mutation(source, seq)
            .and_then(|mutation| mutation.effective_state_change)
    }

    pub fn clear_hook_authority_with_mutation(
        &mut self,
        source: Option<&str>,
        seq: Option<u64>,
    ) -> Option<TerminalStateMutation> {
        let sequence_source = source.map(str::to_string).or_else(|| {
            self.hook_authority
                .as_ref()
                .map(|authority| authority.source.clone())
        });
        if let Some(source) = sequence_source.as_deref() {
            if !self.accept_hook_report(source, seq) {
                return None;
            }
        }

        let now = Instant::now();
        let previous_agent_label = self.effective_agent_label().map(str::to_string);
        let previous_known_agent = self.effective_known_agent();
        let previous_state = self.state;
        let previous_presentation = self.effective_presentation_for_state_at(previous_state, now);
        let previous_session = self.current_session_identity_for_persistence();
        let should_clear = self
            .hook_authority
            .as_ref()
            .is_some_and(|authority| source.is_none_or(|source| authority.source == source));
        if !should_clear {
            return None;
        }
        self.hook_authority = None;
        self.stale_hook_idle_since = None;
        self.persisted_agent_session = None;
        Some(TerminalStateMutation {
            effective_state_change: self.recompute_effective_state(
                previous_agent_label,
                previous_known_agent,
                previous_state,
                previous_presentation,
                now,
            ),
            session_ref_changed: previous_session.is_some(),
            applied_session_ref: None,
        })
    }

    #[cfg(test)]
    pub fn release_agent(
        &mut self,
        source: &str,
        agent_label: &str,
        seq: Option<u64>,
    ) -> Option<EffectiveStateChange> {
        self.release_agent_with_mutation(source, agent_label, seq)
            .and_then(|mutation| mutation.effective_state_change)
    }

    pub fn release_agent_with_mutation(
        &mut self,
        source: &str,
        agent_label: &str,
        seq: Option<u64>,
    ) -> Option<TerminalStateMutation> {
        if !self.accept_hook_report(source, seq) {
            return None;
        }

        if self.hook_authority.as_ref().is_some_and(|authority| {
            authority.agent_label != agent_label || authority.source != source
        }) {
            return None;
        }

        let matches_current_agent = self.effective_agent_label() == Some(agent_label);
        let matches_persisted_session = self.persisted_agent_session_matches(source, agent_label);
        if !matches_current_agent && !matches_persisted_session {
            return None;
        }

        let now = Instant::now();
        let previous_agent_label = self.effective_agent_label().map(str::to_string);
        let previous_known_agent = self.effective_known_agent();
        let previous_state = self.state;
        let previous_presentation = self.effective_presentation_for_state_at(previous_state, now);
        let previous_session = self.current_session_identity_for_persistence();
        self.detected_agent = None;
        self.fallback_state = AgentState::Unknown;
        self.fallback_visible_blocker = false;
        self.fallback_visible_idle = false;
        self.fallback_visible_working = false;
        self.fallback_observed_at = None;
        self.hook_authority = None;
        self.stale_hook_idle_since = None;
        self.persisted_agent_session = None;
        Some(TerminalStateMutation {
            effective_state_change: self.recompute_effective_state(
                previous_agent_label,
                previous_known_agent,
                previous_state,
                previous_presentation,
                now,
            ),
            session_ref_changed: previous_session.is_some(),
            applied_session_ref: None,
        })
    }

    pub fn effective_agent_label(&self) -> Option<&str> {
        self.hook_authority
            .as_ref()
            .map(|authority| authority.agent_label.as_str())
            .or_else(|| self.detected_agent.map(crate::detect::agent_label))
    }

    pub fn effective_known_agent(&self) -> Option<Agent> {
        if let Some(authority) = &self.hook_authority {
            return crate::detect::parse_agent_label(&authority.agent_label);
        }
        self.detected_agent
    }

    fn visible_blocker_overrides_hook(&self) -> bool {
        self.fallback_visible_blocker
            && self.fallback_not_older_than_hook()
            && self.hook_authority.as_ref().is_some_and(|authority| {
                authority.state != AgentState::Blocked
                    && crate::detect::parse_agent_label(&authority.agent_label)
                        == self.detected_agent
            })
    }

    fn visible_working_overrides_hook(&self) -> bool {
        self.fallback_visible_working
            && self.visible_working_is_fresh_enough_for_hook()
            && self.hook_authority.as_ref().is_some_and(|authority| {
                (authority.state == AgentState::Idle || authority.state == AgentState::Blocked)
                    && crate::detect::parse_agent_label(&authority.agent_label)
                        == self.detected_agent
            })
    }

    fn visible_working_is_fresh_enough_for_hook(&self) -> bool {
        self.fallback_not_older_than_hook()
            || self
                .fallback_observed_at
                .zip(
                    self.hook_authority
                        .as_ref()
                        .map(|authority| authority.reported_at),
                )
                .is_some_and(|(observed_at, reported_at)| {
                    reported_at >= observed_at
                        && reported_at.duration_since(observed_at) < CLAUDE_WORKING_HOLD
                })
    }

    /// True when a hook report is present AND still within [`HOOK_AUTHORITY_TTL`].
    ///
    /// `None` (never reported) and `Some` past its TTL are different things and
    /// callers must not conflate them — see [`Self::hook_authority_expired`].
    fn hook_authority_is_fresh(&self, now: Instant) -> bool {
        self.hook_authority.as_ref().is_some_and(|authority| {
            now.checked_duration_since(authority.reported_at)
                .is_none_or(|age| age < HOOK_AUTHORITY_TTL)
        })
    }

    /// Compact hook facts for the state-change log (#309): the reported state
    /// and its age in ms, or `None` when no hook has ever reported. The log
    /// must be able to say "expired" as distinct from "absent".
    pub(crate) fn hook_authority_report(&self, now: Instant) -> Option<(AgentState, u128)> {
        self.hook_authority.as_ref().map(|authority| {
            (
                authority.state,
                now.checked_duration_since(authority.reported_at)
                    .unwrap_or_default()
                    .as_millis(),
            )
        })
    }

    /// True when a hook reported once and has since gone quiet past its TTL.
    pub(crate) fn hook_authority_expired(&self, now: Instant) -> bool {
        self.hook_authority.is_some() && !self.hook_authority_is_fresh(now)
    }

    fn visible_idle_stales_hook(&self, now: Instant) -> bool {
        self.stale_hook_idle_since
            .is_some_and(|since| now.duration_since(since) >= STALE_HOOK_IDLE_GRACE)
    }

    fn visible_idle_masks_hook_custom_status(&self, state: AgentState, now: Instant) -> bool {
        self.fallback_visible_idle
            && self.fallback_not_older_than_hook()
            && self.hook_authority.as_ref().is_some_and(|authority| {
                (authority.state == AgentState::Working || authority.state == AgentState::Blocked)
                    && crate::detect::parse_agent_label(&authority.agent_label)
                        == self.detected_agent
            })
            && (state == AgentState::Idle || self.visible_idle_stales_hook(now))
    }

    fn update_stale_hook_idle_window(&mut self, now: Instant) {
        let visible_idle_stales_hook = self.fallback_visible_idle
            && self.fallback_not_older_than_hook()
            && self.hook_authority.as_ref().is_some_and(|authority| {
                (authority.state == AgentState::Working || authority.state == AgentState::Blocked)
                    && crate::detect::parse_agent_label(&authority.agent_label)
                        == self.detected_agent
            });

        if visible_idle_stales_hook {
            self.stale_hook_idle_since.get_or_insert(now);
        } else {
            self.stale_hook_idle_since = None;
        }
    }

    pub fn set_manual_label(&mut self, label: String) {
        let label = label.trim().to_string();
        self.manual_label = (!label.is_empty()).then_some(label);
    }

    pub fn clear_manual_label(&mut self) {
        self.manual_label = None;
    }

    pub fn set_agent_name(&mut self, name: String) {
        let name = name.trim().to_string();
        self.agent_name = (!name.is_empty()).then_some(name);
    }

    pub fn clear_agent_name(&mut self) {
        self.agent_name = None;
    }

    pub fn clear_agent_runtime_identity_after_respawn(&mut self) {
        self.detected_agent = None;
        self.fallback_state = AgentState::Unknown;
        self.fallback_visible_blocker = false;
        self.fallback_visible_idle = false;
        self.fallback_visible_working = false;
        self.fallback_observed_at = None;
        self.stale_hook_idle_since = None;
        self.live_activity = None;
        self.hook_authority = None;
        self.persisted_agent_session = None;
        self.agent_metadata.clear();
        self.state = AgentState::Unknown;
        self.launch_argv = None;
        self.respawn_shell_on_exit = false;
        self.pending_agent_resume_plan = None;
        self.hibernated_resume_plan = None;
        self.clear_agent_name();
    }

    pub fn is_agent_terminal(&self) -> bool {
        self.agent_name.is_some()
            || self.effective_agent_label().is_some()
            || self.launch_argv.is_some()
    }

    pub fn border_label(&self, show_agent_labels: bool) -> Option<String> {
        self.effective_title().or_else(|| {
            self.manual_label.clone().or_else(|| {
                show_agent_labels
                    .then(|| {
                        self.effective_display_agent()
                            .or_else(|| self.effective_agent_label().map(str::to_string))
                    })
                    .flatten()
            })
        })
    }

    fn recompute_effective_state(
        &mut self,
        previous_agent_label: Option<String>,
        previous_known_agent: Option<Agent>,
        previous_state: AgentState,
        previous_presentation: EffectivePresentation,
        now: Instant,
    ) -> Option<EffectiveStateChange> {
        // Precedence is fresh-vs-stale, not source-vs-source (#309): a hook
        // that has gone quiet past its TTL stops winning, so the screen takes
        // over without needing the veto grace to fight it every tick.
        let (state, authority) = if self.visible_blocker_overrides_hook() {
            (AgentState::Blocked, StateAuthority::VisibleBlocker)
        } else if self.visible_working_overrides_hook() {
            (AgentState::Working, StateAuthority::VisibleWorking)
        } else if self.hook_authority_expired(now) {
            // Checked BEFORE the veto: the veto exists only to overrule a hook
            // that is still claiming authority. An expired one has none, so the
            // screen decides outright rather than having to earn a 2s grace.
            (self.fallback_state, StateAuthority::HookExpired)
        } else if self.visible_idle_stales_hook(now) {
            (AgentState::Idle, StateAuthority::VisibleIdleVeto)
        } else if let Some(authority) = self.hook_authority.as_ref() {
            (authority.state, StateAuthority::Hook)
        } else {
            (self.fallback_state, StateAuthority::Screen)
        };
        self.last_state_authority = authority;
        let agent_label = self.effective_agent_label().map(str::to_string);
        let known_agent = self.effective_known_agent();

        let presentation = self.effective_presentation_for_state_at(state, now);
        self.clear_expiry_pending_for_hidden_metadata();

        if previous_agent_label == agent_label
            && previous_state == state
            && previous_presentation == presentation
        {
            return None;
        }

        if previous_state != state {
            self.state_changed_at = Some(now);
        }
        self.state = state;
        Some(EffectiveStateChange {
            previous_agent_label,
            previous_known_agent,
            previous_state,
            previous_presentation,
            agent_label,
            known_agent,
            state,
            presentation,
            authority,
        })
    }

    /// The Claude session id for this pane, once a hook has established one.
    ///
    /// Only Claude is wired today; other sources get their own
    /// `TranscriptSource` impl rather than a branch here (#246).
    pub fn claude_session_id(&self) -> Option<String> {
        let authority = self.hook_authority.as_ref()?;
        if authority.source != "flock:claude" {
            return None;
        }
        let session_ref = authority.session_ref.as_ref()?;
        Some(session_ref.value.clone())
    }

    /// Ask for a re-read even though the `(session, detail)` pair is unchanged.
    ///
    /// The guard below is an *identity* check, which is correct only while
    /// hydration happens once per session: the same pair can never be due
    /// twice. That is exactly wrong once re-reads become routine — a new turn
    /// landing on disk is the same session at the same detail, and would be
    /// refused forever (#254). Bumping the generation makes the pair due again
    /// without weakening the burst protection: a hundred hooks between reads
    /// still bump to one pending generation, so still one reader.
    pub fn invalidate_transcript_hydration(&mut self) {
        self.wanted_transcript_generation = self.wanted_transcript_generation.wrapping_add(1);
    }

    /// True when the caller should read the transcript and hydrate at `detail`.
    ///
    /// Due when the `(session, detail)` pair changed, or when
    /// [`Self::invalidate_transcript_hydration`] has been called since the last
    /// read. Flips immediately so a burst of hook reports cannot spawn a reader
    /// per event.
    pub fn take_transcript_hydration_due(
        &mut self,
        session_id: &str,
        detail: crate::agent_transcript::TranscriptDetail,
    ) -> bool {
        let target = (session_id.to_string(), detail);
        let pair_changed = self.hydrated_transcript.as_ref() != Some(&target);
        let generation_stale =
            self.hydrated_transcript_generation != self.wanted_transcript_generation;
        if !pair_changed && !generation_stale {
            return false;
        }
        // A re-read of the SAME pair while a reader is already walking the file
        // would spend a second pass over ~15 MB to deliver the same content.
        // The pending generation is remembered, so the next poll after this one
        // lands picks it up. A changed pair still arms immediately: it is a
        // different request, and the input path throttles key-repeat itself.
        if !pair_changed && self.hydration_arm_index.is_some() {
            return false;
        }
        self.hydrated_transcript = Some(target);
        self.hydrated_transcript_generation = self.wanted_transcript_generation;
        // Hook entries appended from here on are newer than the read that is
        // about to start, so hydration must not swallow them (see
        // `hydrate_prompt_history`).
        self.hydration_arm_index = Some(self.prompt_history.len());
        true
    }

    /// True while a reader is armed but has not delivered.
    ///
    /// Cycling detail re-arms, and each arm spawns a worker over a file that
    /// reaches ~15 MB. Held key-repeat on the cycle key would otherwise stack
    /// readers, so the input path skips while one is in flight.
    pub fn transcript_read_in_flight(&self) -> bool {
        self.hydration_arm_index.is_some()
    }

    /// Re-arm hydration after a failed read.
    ///
    /// The guard flips before the worker starts so a burst of hook reports
    /// cannot spawn a reader each. If that read then fails — most likely
    /// because the agent has not written its transcript yet when the first
    /// hook fires — the pane would otherwise never hydrate for the rest of
    /// the session. Clearing it lets the next hook or cycle try again.
    pub fn rearm_transcript_hydration(&mut self) {
        self.hydrated_transcript = None;
        self.hydration_arm_index = None;
    }

    /// Whether a re-read is still owed, i.e. the file moved since the last one.
    #[cfg(test)]
    pub fn transcript_hydration_pending(&self) -> bool {
        self.hydrated_transcript_generation != self.wanted_transcript_generation
    }

    /// The `(session, detail)` a reader is currently in flight (or last
    /// delivered) for. The delivery path uses this to drop stale results
    /// arriving after the user cycled level.
    pub fn expected_transcript_detail(
        &self,
    ) -> Option<(String, crate::agent_transcript::TranscriptDetail)> {
        self.hydrated_transcript.clone()
    }

    /// Replace the history ring with the pane's own session transcript (#246).
    ///
    /// The ring is otherwise fed by lifecycle hooks, which only report what a
    /// hook chose to emit and only when hooks are installed. The transcript is
    /// the source of truth and is complete, so it wins wholesale — including
    /// retroactively, for turns that happened before hooks ever fired.
    ///
    /// Hooks keep appending afterwards for liveness: this runs once per pane
    /// when the session becomes known, not on every turn.
    pub fn hydrate_prompt_history(
        &mut self,
        turns: &[(crate::agent_transcript::Role, String, Option<SystemTime>)],
    ) {
        if turns.is_empty() {
            // Nothing readable — keep whatever hooks already gave us rather
            // than blanking a panel that had content.
            return;
        }
        // Entries appended after the read was armed are NEWER than the file
        // the worker saw: the turn that triggered hydration is typically not
        // on disk yet (the agent writes it after the hook fires, and a
        // half-written last line is deliberately skipped). Replacing the ring
        // wholesale would drop exactly that turn, so carry the tail over.
        let live_tail: Vec<PromptHistoryEntry> = self
            .hydration_arm_index
            .filter(|index| *index <= self.prompt_history.len())
            .map(|index| self.prompt_history[index..].to_vec())
            .unwrap_or_default();
        self.hydration_arm_index = None;
        let now = Instant::now();
        self.prompt_history.clear();
        for (role, text, at) in turns {
            self.push_prompt_history(PromptHistoryEntry {
                kind: match role {
                    crate::agent_transcript::Role::User => PromptHistoryKind::Prompt,
                    crate::agent_transcript::Role::Assistant => PromptHistoryKind::Reply,
                },
                text: text.clone(),
                recorded_at: now,
                wall_clock: *at,
            });
        }
        // Turns the worker could not have seen go back on the end, in order.
        let carried_prompt = live_tail
            .iter()
            .rev()
            .find(|entry| entry.kind == PromptHistoryKind::Prompt)
            .map(|entry| entry.text.clone());
        for entry in live_tail {
            self.push_prompt_history(entry);
        }

        // Keep the collapsed header consistent with the ring it summarises —
        // a carried-over live prompt is newer than anything in the file.
        if let Some(last_prompt) = carried_prompt.or_else(|| {
            turns
                .iter()
                .rev()
                .find(|(role, _, _)| matches!(role, crate::agent_transcript::Role::User))
                .map(|(_, text, _)| text.clone())
        }) {
            self.last_prompt = Some(last_prompt);
        }
    }

    /// Append a user prompt to the history ring and refresh `last_prompt`
    /// (the legacy collapsed-header field). Same as `record_prompt_at` with
    /// `Instant::now()`.
    pub fn record_prompt(&mut self, prompt: String) {
        self.record_prompt_at(prompt, Instant::now());
    }

    /// Append to the history ring and mark the layout cache stale.
    ///
    /// Every mutation must go through here — a direct `push` would leave the
    /// panel rendering rows laid out from the previous content (#254).
    fn push_prompt_history(&mut self, entry: PromptHistoryEntry) {
        append_prompt_history_with_cap(&mut self.prompt_history, entry);
        self.prompt_history_generation = self.prompt_history_generation.wrapping_add(1);
    }

    /// Monotonic counter over `prompt_history` mutations; the panel's row
    /// layout cache is keyed on it.
    pub fn prompt_history_generation(&self) -> u64 {
        self.prompt_history_generation
    }

    pub fn record_prompt_at(&mut self, prompt: String, now: Instant) {
        self.last_prompt = Some(prompt.clone());
        // The agent just wrote a turn boundary, so the transcript on disk has
        // moved: mark hydration due so the panel re-reads the authoritative
        // text rather than living on the hook's summary forever (#254).
        self.invalidate_transcript_hydration();
        self.push_prompt_history(PromptHistoryEntry {
            kind: PromptHistoryKind::Prompt,
            text: prompt,
            recorded_at: now,
            wall_clock: None,
        });
    }

    /// Append a recap entry to the history ring. Recaps render visually
    /// distinct from prompts and do NOT update `last_prompt` — the collapsed
    /// header still shows the latest user prompt verbatim.
    pub fn record_recap(&mut self, recap: String) {
        self.record_recap_at(recap, Instant::now());
    }

    pub fn record_recap_at(&mut self, recap: String, now: Instant) {
        // The agent just wrote a turn boundary, so the transcript on disk has
        // moved: mark hydration due so the panel re-reads the authoritative
        // text rather than living on the hook's summary forever (#254).
        self.invalidate_transcript_hydration();
        self.push_prompt_history(PromptHistoryEntry {
            kind: PromptHistoryKind::Recap,
            text: recap,
            recorded_at: now,
            wall_clock: None,
        });
    }

    /// Append an assistant reply to the history ring. Wired from the Stop
    /// hook with the last assistant message (capped on the wire). Like
    /// recaps, replies do NOT update `last_prompt` — the collapsed header
    /// still shows the latest user prompt verbatim.
    pub fn record_reply(&mut self, reply: String) {
        self.record_reply_at(reply, Instant::now());
    }

    pub fn record_reply_at(&mut self, reply: String, now: Instant) {
        // The agent just wrote a turn boundary, so the transcript on disk has
        // moved: mark hydration due so the panel re-reads the authoritative
        // text rather than living on the hook's summary forever (#254).
        self.invalidate_transcript_hydration();
        self.push_prompt_history(PromptHistoryEntry {
            kind: PromptHistoryKind::Reply,
            text: reply,
            recorded_at: now,
            wall_clock: None,
        });
    }
}

/// Hold a `Working -> Idle` edge for [`CLAUDE_WORKING_HOLD`] so a single
/// detector frame cannot flip the sidebar.
///
/// This used to bail out for every agent except Claude, which left codex,
/// gemini, cursor, droid and the rest flipping on the raw 300ms tick with no
/// damping at all (#309). The constant was tuned against Claude's redraw
/// cadence but the hazard is not Claude-specific: it is "one bad frame", and
/// every screen-scraped agent has those. `ATTENTION_SETTLE` already applies
/// the same 1200ms agent-agnostically one layer up.
pub(crate) fn stabilize_agent_state(
    agent: Option<Agent>,
    previous: AgentState,
    raw: AgentState,
    now: std::time::Instant,
    last_claude_working_at: &mut Option<std::time::Instant>,
) -> AgentState {
    if agent.is_none() {
        return raw;
    }

    match raw {
        AgentState::Working => {
            *last_claude_working_at = Some(now);
            AgentState::Working
        }
        AgentState::Blocked => AgentState::Blocked,
        AgentState::Idle if previous == AgentState::Working => {
            if last_claude_working_at
                .is_some_and(|last_working| now.duration_since(last_working) < CLAUDE_WORKING_HOLD)
            {
                AgentState::Working
            } else {
                AgentState::Idle
            }
        }
        _ => raw,
    }
}

pub(crate) fn stabilize_agent_detection(
    agent: Option<Agent>,
    previous: AgentState,
    detection: crate::detect::AgentDetection,
    process_exited: bool,
    now: std::time::Instant,
    last_claude_working_at: &mut Option<std::time::Instant>,
) -> AgentState {
    if process_exited {
        return detection.state;
    }

    stabilize_agent_state(
        agent,
        previous,
        detection.state,
        now,
        last_claude_working_at,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::AgentDetection;

    fn test_terminal() -> TerminalState {
        TerminalState::new(TerminalId::alloc(), "/tmp".into())
    }

    /// #246: the transcript is complete and authoritative, so it replaces the
    /// hook-fed ring rather than appending to it — otherwise every turn that
    /// hooks did report would render twice.
    #[test]
    fn hydration_replaces_hook_fed_history_and_keeps_wall_clock_ages() {
        use crate::agent_transcript::Role;

        let mut terminal = test_terminal();
        terminal.record_prompt("reported by a hook".into());
        terminal.record_reply("hook reply".into());
        assert_eq!(terminal.prompt_history.len(), 2);

        let an_hour_ago = SystemTime::now() - Duration::from_secs(3_600);
        terminal.hydrate_prompt_history(&[
            (Role::User, "first prompt".into(), Some(an_hour_ago)),
            (Role::Assistant, "first reply".into(), Some(an_hour_ago)),
            (Role::User, "second prompt".into(), None),
        ]);

        assert_eq!(
            terminal
                .prompt_history
                .iter()
                .map(|e| (e.kind, e.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (PromptHistoryKind::Prompt, "first prompt"),
                (PromptHistoryKind::Reply, "first reply"),
                (PromptHistoryKind::Prompt, "second prompt"),
            ],
            "hook entries must not survive alongside the transcript"
        );
        // Historical entries age from the wall clock, not from hydration time.
        assert_eq!(
            terminal.prompt_history[0].relative_age(Instant::now()),
            "1h ago"
        );
        // No stamp falls back to the monotonic clock (just hydrated).
        assert_eq!(
            terminal.prompt_history[2].relative_age(Instant::now()),
            "0s ago"
        );
        // The collapsed header follows the ring it summarises.
        assert_eq!(terminal.last_prompt.as_deref(), Some("second prompt"));
    }

    /// The guard used to be per-session, which meant cycling the panel's
    /// detail level could not re-arm the reader and the user would keep
    /// seeing the previous mode's turns. Assert directly that the same
    /// session at a different detail re-arms, while the same pair does not.
    #[test]
    fn hydration_guard_rearms_on_detail_change_but_not_on_repeat() {
        use crate::agent_transcript::TranscriptDetail;

        let mut terminal = test_terminal();
        assert!(terminal.take_transcript_hydration_due("s1", TranscriptDetail::Reply));
        assert!(
            !terminal.take_transcript_hydration_due("s1", TranscriptDetail::Reply),
            "same (session, detail) must not re-arm"
        );
        assert!(
            terminal.take_transcript_hydration_due("s1", TranscriptDetail::Collapsed),
            "cycling detail on the same session must re-arm the reader"
        );
        assert!(terminal.take_transcript_hydration_due("s2", TranscriptDetail::Collapsed));
    }

    /// #254: the identity guard alone is correct only while hydration happens
    /// once per session. A new turn is the same session at the same detail, so
    /// without a generation the panel would never re-read the file again and
    /// would live on hook summaries for the rest of the session.
    #[test]
    fn a_new_turn_makes_the_same_session_due_for_a_re_read() {
        use crate::agent_transcript::TranscriptDetail;

        let mut terminal = test_terminal();
        assert!(terminal.take_transcript_hydration_due("s1", TranscriptDetail::Reply));
        assert!(!terminal.take_transcript_hydration_due("s1", TranscriptDetail::Reply));

        // Simulate the read landing, then a hook reporting a new turn.
        terminal.hydrate_prompt_history(&[(
            crate::agent_transcript::Role::User,
            "old turn".into(),
            None,
        )]);
        terminal.record_prompt("a brand new turn".into());
        assert!(
            terminal.transcript_hydration_pending(),
            "a reported turn means the file moved"
        );
        assert!(
            terminal.take_transcript_hydration_due("s1", TranscriptDetail::Reply),
            "the same session at the same detail must be due again"
        );
    }

    /// A burst of hooks between reads must still collapse to one reader — the
    /// file reaches ~15 MB and this is not the UI thread's to spend twice.
    #[test]
    fn a_burst_of_turns_collapses_to_a_single_pending_read() {
        use crate::agent_transcript::TranscriptDetail;

        let mut terminal = test_terminal();
        assert!(terminal.take_transcript_hydration_due("s1", TranscriptDetail::Reply));
        assert!(terminal.transcript_read_in_flight());

        for i in 0..10 {
            terminal.record_prompt(format!("turn {i}"));
        }
        assert!(
            !terminal.take_transcript_hydration_due("s1", TranscriptDetail::Reply),
            "a second reader must not stack while one is walking the file"
        );

        // Once the in-flight read lands, the pending work is still owed.
        terminal.hydrate_prompt_history(&[(
            crate::agent_transcript::Role::User,
            "from disk".into(),
            None,
        )]);
        assert!(
            terminal.take_transcript_hydration_due("s1", TranscriptDetail::Reply),
            "the deferred re-read must not be lost"
        );
    }

    /// The turn that ARMS hydration is usually not on disk yet — the agent
    /// writes it after the hook fires, and a half-written last line is
    /// deliberately skipped by the reader. Replacing the ring wholesale would
    /// drop exactly that turn, and no later hook would bring it back.
    #[test]
    fn hydration_keeps_turns_that_arrived_after_the_read_was_armed() {
        use crate::agent_transcript::{Role, TranscriptDetail};

        let mut terminal = test_terminal();
        terminal.record_prompt("older turn already on disk".into());
        assert!(terminal.take_transcript_hydration_due("sess-1", TranscriptDetail::Reply));
        // The hook that armed the read appends AFTER the worker opened the file.
        terminal.record_prompt("live turn the worker cannot have seen".into());

        terminal.hydrate_prompt_history(&[(Role::User, "older turn already on disk".into(), None)]);

        assert_eq!(
            terminal
                .prompt_history
                .iter()
                .map(|e| e.text.as_str())
                .collect::<Vec<_>>(),
            vec![
                "older turn already on disk",
                "live turn the worker cannot have seen",
            ],
            "the turn that armed hydration must survive the replace"
        );
        assert_eq!(
            terminal.last_prompt.as_deref(),
            Some("live turn the worker cannot have seen"),
            "the collapsed header must follow the newest turn, not the file's"
        );
    }

    /// A failed read leaves the guard flipped; without re-arming, a pane whose
    /// transcript did not exist yet would never hydrate again for that session.
    #[test]
    fn a_failed_read_rearms_so_the_next_hook_retries() {
        use crate::agent_transcript::TranscriptDetail;

        let mut terminal = test_terminal();
        assert!(terminal.take_transcript_hydration_due("sess-1", TranscriptDetail::Reply));
        assert!(
            !terminal.take_transcript_hydration_due("sess-1", TranscriptDetail::Reply),
            "a burst of hooks must not spawn a reader each"
        );
        assert!(terminal.transcript_read_in_flight());

        terminal.rearm_transcript_hydration();

        assert!(!terminal.transcript_read_in_flight());
        assert!(
            terminal.take_transcript_hydration_due("sess-1", TranscriptDetail::Reply),
            "after a failed read the next hook must be able to retry"
        );
    }

    /// An unreadable or empty transcript must not blank a panel that hooks had
    /// already filled.
    #[test]
    fn hydration_with_no_turns_keeps_existing_history() {
        let mut terminal = test_terminal();
        terminal.record_prompt("kept".into());
        terminal.hydrate_prompt_history(&[]);
        assert_eq!(terminal.prompt_history.len(), 1);
        assert_eq!(terminal.prompt_history[0].text, "kept");
    }

    #[test]
    fn live_activity_survives_transient_scrape_miss_while_working() {
        let mut terminal = test_terminal();

        terminal.update_live_activity(Some("Cogitating".into()), AgentState::Working);
        assert_eq!(terminal.live_activity.as_deref(), Some("Cogitating"));

        // A frame caught mid-redraw scrapes as None, but the agent is still
        // working — hold the last activity instead of flickering to the label.
        terminal.update_live_activity(None, AgentState::Working);
        assert_eq!(terminal.live_activity.as_deref(), Some("Cogitating"));

        // A fresh scrape replaces it.
        terminal.update_live_activity(Some("Manifesting".into()), AgentState::Working);
        assert_eq!(terminal.live_activity.as_deref(), Some("Manifesting"));

        // Once the agent genuinely leaves working, a None clears it.
        terminal.update_live_activity(None, AgentState::Idle);
        assert_eq!(terminal.live_activity, None);
    }

    #[test]
    fn claude_working_is_sticky_for_short_gap() {
        let now = std::time::Instant::now();
        let mut last_working = None;

        let working = stabilize_agent_state(
            Some(Agent::Claude),
            AgentState::Idle,
            AgentState::Working,
            now,
            &mut last_working,
        );
        assert_eq!(working, AgentState::Working);

        let still_working = stabilize_agent_state(
            Some(Agent::Claude),
            AgentState::Working,
            AgentState::Idle,
            now + std::time::Duration::from_millis(400),
            &mut last_working,
        );
        assert_eq!(still_working, AgentState::Working);
    }

    #[test]
    fn claude_transitions_to_idle_after_hold_expires() {
        let now = std::time::Instant::now();
        let mut last_working = Some(now);

        let state = stabilize_agent_state(
            Some(Agent::Claude),
            AgentState::Working,
            AgentState::Idle,
            now + CLAUDE_WORKING_HOLD + std::time::Duration::from_millis(1),
            &mut last_working,
        );
        assert_eq!(state, AgentState::Idle);
    }

    #[test]
    fn process_exit_idle_bypasses_claude_working_hold() {
        let now = std::time::Instant::now();
        let mut last_working = Some(now);

        let state = stabilize_agent_detection(
            Some(Agent::Claude),
            AgentState::Working,
            AgentDetection {
                state: AgentState::Idle,
                activity: None,
                skip_state_update: false,
                visible_blocker: false,
                visible_idle: false,
                visible_working: false,
            },
            true,
            now + std::time::Duration::from_millis(100),
            &mut last_working,
        );

        assert_eq!(state, AgentState::Idle);
    }

    #[test]
    fn visible_idle_does_not_bypass_claude_working_hold() {
        let now = std::time::Instant::now();
        let mut last_working = Some(now);

        let state = stabilize_agent_detection(
            Some(Agent::Claude),
            AgentState::Working,
            AgentDetection {
                state: AgentState::Idle,
                activity: None,
                skip_state_update: false,
                visible_blocker: false,
                visible_idle: true,
                visible_working: false,
            },
            false,
            now + std::time::Duration::from_millis(100),
            &mut last_working,
        );

        assert_eq!(state, AgentState::Working);
    }

    #[test]
    fn non_claude_states_are_unchanged() {
        let now = std::time::Instant::now();
        let mut last_working = None;

        let state = stabilize_agent_state(
            Some(Agent::Codex),
            AgentState::Working,
            AgentState::Idle,
            now,
            &mut last_working,
        );
        assert_eq!(state, AgentState::Idle);
    }

    #[test]
    fn hook_authority_overrides_fallback_for_same_agent() {
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        terminal.set_hook_authority(
            "flock:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            None,
        );

        assert_eq!(terminal.detected_agent, Some(Agent::Pi));
        assert_eq!(terminal.fallback_state, AgentState::Idle);
        assert_eq!(terminal.effective_agent_label(), Some("pi"));
        assert_eq!(terminal.state, AgentState::Working);
    }

    #[test]
    fn hook_authority_can_override_with_unknown_agent_label() {
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        terminal.set_hook_authority(
            "flock:custom".into(),
            "custom-agent".into(),
            AgentState::Working,
            None,
            None,
        );

        assert_eq!(terminal.detected_agent, Some(Agent::Pi));
        assert_eq!(terminal.effective_agent_label(), Some("custom-agent"));
        assert_eq!(terminal.effective_known_agent(), None);
        assert_eq!(terminal.state, AgentState::Working);
    }

    #[test]
    fn visible_blocker_overrides_non_blocked_hook_for_same_agent() {
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Idle);
        terminal.set_hook_authority(
            "flock:codex".into(),
            "codex".into(),
            AgentState::Working,
            None,
            None,
        );

        let change = terminal.set_detected_state_with_visible_blocker(
            Some(Agent::Codex),
            AgentState::Blocked,
            true,
            false,
            false,
        );

        assert_eq!(terminal.fallback_state, AgentState::Blocked);
        assert_eq!(terminal.state, AgentState::Blocked);
        assert_eq!(change.unwrap().previous_state, AgentState::Working);
    }

    #[test]
    fn weak_blocked_fallback_does_not_override_hook_authority() {
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Idle);
        terminal.set_hook_authority(
            "flock:codex".into(),
            "codex".into(),
            AgentState::Working,
            None,
            None,
        );

        let change = terminal.set_detected_state_with_visible_blocker(
            Some(Agent::Codex),
            AgentState::Blocked,
            false,
            false,
            false,
        );

        assert_eq!(terminal.fallback_state, AgentState::Blocked);
        assert_eq!(terminal.state, AgentState::Working);
        assert!(change.is_none());
    }

    #[test]
    fn hook_blocked_wins_over_visible_blocker() {
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Working);
        terminal.set_hook_authority(
            "flock:codex".into(),
            "codex".into(),
            AgentState::Blocked,
            None,
            None,
        );

        terminal.set_detected_state_with_visible_blocker(
            Some(Agent::Codex),
            AgentState::Blocked,
            true,
            false,
            false,
        );

        assert_eq!(terminal.state, AgentState::Blocked);
        assert!(terminal.hook_authority.is_some());
    }

    #[test]
    fn visible_blocker_does_not_override_different_agent_hook() {
        let mut terminal = test_terminal();
        terminal.set_detected_state(None, AgentState::Unknown);
        terminal.set_hook_authority(
            "custom:agent".into(),
            "custom-agent".into(),
            AgentState::Working,
            None,
            None,
        );

        terminal.set_detected_state_with_visible_blocker(
            Some(Agent::Codex),
            AgentState::Blocked,
            true,
            false,
            false,
        );

        assert_eq!(terminal.effective_agent_label(), Some("custom-agent"));
        assert_eq!(terminal.state, AgentState::Working);
    }

    #[test]
    fn visible_blocker_suppresses_stale_hook_custom_status() {
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Idle);
        terminal.set_hook_authority_with_custom_status(
            "flock:codex".into(),
            "codex".into(),
            AgentState::Working,
            None,
            Some("planning".into()),
            None,
        );

        terminal.set_detected_state_with_visible_blocker(
            Some(Agent::Codex),
            AgentState::Blocked,
            true,
            false,
            false,
        );

        assert_eq!(terminal.state, AgentState::Blocked);
        assert_eq!(terminal.effective_custom_status(), None);
    }

    #[test]
    fn visible_idle_waits_before_overriding_claude_hook_working() {
        let now = Instant::now();
        let mut terminal = test_terminal();
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Claude),
            AgentState::Working,
            false,
            false,
            true,
            false,
            now,
        );
        terminal.set_hook_authority_with_custom_status_at(
            "flock:claude".into(),
            "claude".into(),
            AgentState::Working,
            None,
            Some("thinking".into()),
            None,
            None,
            now,
        );

        let waiting = terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Claude),
            AgentState::Idle,
            false,
            true,
            false,
            false,
            now + Duration::from_millis(500),
        );

        assert!(waiting.effective_state_change.is_none());
        assert_eq!(terminal.fallback_state, AgentState::Idle);
        assert_eq!(terminal.state, AgentState::Working);
        assert_eq!(
            terminal.effective_custom_status().as_deref(),
            Some("thinking")
        );

        let change = terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Claude),
            AgentState::Idle,
            false,
            true,
            false,
            false,
            now + Duration::from_millis(500) + STALE_HOOK_IDLE_GRACE + Duration::from_millis(1),
        );

        assert_eq!(terminal.state, AgentState::Idle);
        assert_eq!(terminal.effective_custom_status(), None);
        assert_eq!(
            change.effective_state_change.unwrap().previous_state,
            AgentState::Working
        );
    }

    #[test]
    fn fresh_hook_working_resets_visible_idle_stale_window() {
        let now = Instant::now();
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Working);
        terminal.set_hook_authority_with_custom_status_at(
            "flock:claude".into(),
            "claude".into(),
            AgentState::Working,
            None,
            Some("thinking".into()),
            None,
            None,
            now,
        );
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Claude),
            AgentState::Idle,
            false,
            true,
            false,
            false,
            now + Duration::from_millis(500),
        );

        terminal.set_hook_authority_with_custom_status_at(
            "flock:claude".into(),
            "claude".into(),
            AgentState::Working,
            None,
            Some("thinking".into()),
            None,
            Some(1),
            now + Duration::from_millis(800),
        );
        let change = terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Claude),
            AgentState::Idle,
            false,
            true,
            false,
            false,
            now + STALE_HOOK_IDLE_GRACE + Duration::from_millis(1),
        );

        assert!(change.effective_state_change.is_none());
        assert_eq!(terminal.state, AgentState::Working);
    }

    #[test]
    fn visible_working_overrides_hook_idle_for_same_agent() {
        let now = Instant::now();
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Idle);
        terminal.set_hook_authority_with_custom_status_at(
            "flock:claude".into(),
            "claude".into(),
            AgentState::Idle,
            None,
            None,
            None,
            None,
            now,
        );

        let change = terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Claude),
            AgentState::Working,
            false,
            false,
            true,
            false,
            now + Duration::from_millis(1),
        );

        assert_eq!(terminal.state, AgentState::Working);
        assert_eq!(
            change.effective_state_change.unwrap().previous_state,
            AgentState::Idle
        );
    }

    #[test]
    fn recent_visible_working_holds_against_newer_claude_hook_idle() {
        let now = Instant::now();
        let mut terminal = test_terminal();
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Claude),
            AgentState::Working,
            false,
            false,
            true,
            false,
            now,
        );

        let change = terminal.set_hook_authority_with_custom_status_at(
            "flock:claude".into(),
            "claude".into(),
            AgentState::Idle,
            None,
            None,
            None,
            None,
            now + Duration::from_millis(100),
        );

        assert!(change.unwrap().effective_state_change.is_none());
        assert_eq!(terminal.state, AgentState::Working);
    }

    #[test]
    fn old_visible_working_does_not_hold_against_newer_claude_hook_idle() {
        let now = Instant::now();
        let mut terminal = test_terminal();
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Claude),
            AgentState::Working,
            false,
            false,
            true,
            false,
            now,
        );

        let change = terminal.set_hook_authority_with_custom_status_at(
            "flock:claude".into(),
            "claude".into(),
            AgentState::Idle,
            None,
            None,
            None,
            None,
            now + CLAUDE_WORKING_HOLD + Duration::from_millis(1),
        );

        assert_eq!(terminal.state, AgentState::Idle);
        assert_eq!(
            change
                .unwrap()
                .effective_state_change
                .unwrap()
                .previous_state,
            AgentState::Working
        );
    }

    #[test]
    fn refreshed_visible_working_overrides_newer_hook_blocked() {
        let now = Instant::now();
        let mut terminal = test_terminal();
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Codex),
            AgentState::Working,
            false,
            false,
            true,
            false,
            now,
        );
        terminal.set_hook_authority_with_custom_status_at(
            "flock:codex".into(),
            "codex".into(),
            AgentState::Blocked,
            None,
            Some("permission".into()),
            None,
            None,
            now + CLAUDE_WORKING_HOLD + Duration::from_millis(1),
        );

        assert_eq!(terminal.state, AgentState::Blocked);

        let change = terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Codex),
            AgentState::Working,
            false,
            false,
            true,
            false,
            now + CLAUDE_WORKING_HOLD + Duration::from_millis(800),
        );

        assert_eq!(terminal.state, AgentState::Working);
        assert_eq!(terminal.effective_custom_status(), None);
        assert_eq!(
            change.effective_state_change.unwrap().previous_state,
            AgentState::Blocked
        );
    }

    #[test]
    fn visible_idle_waits_before_overriding_claude_hook_blocked() {
        let now = Instant::now();
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Working);
        terminal.set_hook_authority_with_custom_status_at(
            "flock:claude".into(),
            "claude".into(),
            AgentState::Blocked,
            None,
            Some("permission".into()),
            None,
            None,
            now,
        );

        let waiting = terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Claude),
            AgentState::Idle,
            false,
            true,
            false,
            false,
            now + Duration::from_millis(500),
        );

        assert!(waiting.effective_state_change.is_none());
        assert_eq!(terminal.fallback_state, AgentState::Idle);
        assert_eq!(terminal.state, AgentState::Blocked);
        assert_eq!(
            terminal.effective_custom_status().as_deref(),
            Some("permission")
        );

        let change = terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Claude),
            AgentState::Idle,
            false,
            true,
            false,
            false,
            now + Duration::from_millis(500) + STALE_HOOK_IDLE_GRACE + Duration::from_millis(1),
        );

        assert_eq!(terminal.state, AgentState::Idle);
        assert_eq!(terminal.effective_custom_status(), None);
        assert_eq!(
            change.effective_state_change.unwrap().previous_state,
            AgentState::Blocked
        );
    }

    #[test]
    fn visible_idle_does_not_override_other_agent_hook_working() {
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Working);
        terminal.set_hook_authority(
            "flock:codex".into(),
            "codex".into(),
            AgentState::Working,
            None,
            None,
        );

        let change = terminal.set_detected_state_with_visible_blocker(
            Some(Agent::Codex),
            AgentState::Idle,
            false,
            true,
            false,
        );

        assert_eq!(terminal.fallback_state, AgentState::Idle);
        assert_eq!(terminal.state, AgentState::Working);
        assert!(change.is_none());
    }

    #[test]
    fn known_hook_authority_does_not_override_different_detected_agent() {
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::Grok), AgentState::Working);
        let change = terminal.set_hook_authority(
            "flock:claude".into(),
            "claude".into(),
            AgentState::Blocked,
            None,
            None,
        );

        assert!(change.is_none());
        assert!(terminal.hook_authority.is_none());
        assert_eq!(terminal.detected_agent, Some(Agent::Grok));
        assert_eq!(terminal.effective_agent_label(), Some("grok"));
        assert_eq!(terminal.state, AgentState::Working);
    }

    #[test]
    fn detected_agent_clears_conflicting_known_hook_authority() {
        let mut terminal = test_terminal();
        terminal.set_hook_authority(
            "flock:claude".into(),
            "claude".into(),
            AgentState::Blocked,
            None,
            None,
        );

        terminal.set_detected_state(Some(Agent::Grok), AgentState::Working);

        assert!(terminal.hook_authority.is_none());
        assert_eq!(terminal.detected_agent, Some(Agent::Grok));
        assert_eq!(terminal.effective_agent_label(), Some("grok"));
        assert_eq!(terminal.state, AgentState::Working);
    }

    #[test]
    fn border_label_prefers_manual_label_over_agent_label() {
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Idle);

        assert_eq!(terminal.border_label(false), None);
        assert_eq!(terminal.border_label(true).as_deref(), Some("claude"));

        terminal.set_manual_label(" reviewer ".into());
        assert_eq!(terminal.border_label(false).as_deref(), Some("reviewer"));
        assert_eq!(terminal.border_label(true).as_deref(), Some("reviewer"));

        terminal.set_manual_label("   ".into());
        assert_eq!(terminal.border_label(true).as_deref(), Some("claude"));

        terminal.set_manual_label("reviewer".into());
        terminal.clear_manual_label();
        assert_eq!(terminal.border_label(true).as_deref(), Some("claude"));
    }

    #[test]
    fn hook_authority_survives_unrelated_detected_agent_clear() {
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        terminal.set_hook_authority(
            "flock:custom".into(),
            "custom-agent".into(),
            AgentState::Working,
            None,
            None,
        );

        terminal.set_detected_state(None, AgentState::Unknown);

        assert!(terminal.hook_authority.is_some());
        assert_eq!(terminal.detected_agent, None);
        assert_eq!(terminal.effective_agent_label(), Some("custom-agent"));
        assert_eq!(terminal.state, AgentState::Working);
    }

    #[test]
    fn detected_agent_clear_clears_matching_hook_authority() {
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
        terminal.set_hook_authority(
            "flock:opencode".into(),
            "opencode".into(),
            AgentState::Idle,
            None,
            None,
        );

        terminal.set_detected_state(None, AgentState::Unknown);

        assert!(terminal.hook_authority.is_none());
        assert_eq!(terminal.detected_agent, None);
        assert_eq!(terminal.fallback_state, AgentState::Unknown);
        assert_eq!(terminal.effective_agent_label(), None);
        assert_eq!(terminal.state, AgentState::Unknown);
    }

    #[test]
    fn detected_agent_clear_clears_matching_working_hook_authority() {
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Working);
        terminal.set_hook_authority(
            "flock:codex".into(),
            "codex".into(),
            AgentState::Working,
            None,
            None,
        );

        terminal.set_detected_state(None, AgentState::Unknown);

        assert!(terminal.hook_authority.is_none());
        assert_eq!(terminal.detected_agent, None);
        assert_eq!(terminal.effective_agent_label(), None);
        assert_eq!(terminal.state, AgentState::Unknown);
    }

    #[test]
    fn process_exit_clears_matching_hook_authority_before_reporting_idle() {
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Working);
        terminal.set_hook_authority(
            "flock:codex".into(),
            "codex".into(),
            AgentState::Working,
            None,
            None,
        );

        terminal.set_detected_state_with_visible_blocker(
            Some(Agent::Codex),
            AgentState::Idle,
            false,
            false,
            true,
        );

        assert!(terminal.hook_authority.is_none());
        assert_eq!(terminal.detected_agent, Some(Agent::Codex));
        assert_eq!(terminal.effective_agent_label(), Some("codex"));
        assert_eq!(terminal.state, AgentState::Idle);
    }

    #[test]
    fn stale_visible_screen_signal_does_not_override_newer_hook_authority() {
        let mut terminal = test_terminal();
        let observed = Instant::now();
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Claude),
            AgentState::Working,
            false,
            false,
            true,
            false,
            observed,
        );
        terminal.set_hook_authority_with_custom_status_at(
            "flock:claude".into(),
            "claude".into(),
            AgentState::Working,
            None,
            None,
            None,
            Some(1),
            observed + Duration::from_secs(1),
        );

        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Claude),
            AgentState::Idle,
            false,
            true,
            false,
            false,
            observed,
        );

        assert_eq!(terminal.state, AgentState::Working);
        assert!(terminal.stale_hook_idle_since.is_none());
    }

    #[test]
    fn stale_process_exit_does_not_clear_newer_same_agent_hook_authority() {
        let mut terminal = test_terminal();
        let observed = Instant::now();
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Codex),
            AgentState::Working,
            false,
            false,
            false,
            false,
            observed,
        );
        terminal.set_hook_authority_with_custom_status_at(
            "flock:codex".into(),
            "codex".into(),
            AgentState::Working,
            None,
            None,
            None,
            Some(1),
            observed,
        );
        terminal.set_hook_authority_with_custom_status_at(
            "flock:codex".into(),
            "codex".into(),
            AgentState::Working,
            None,
            Some("new turn".into()),
            None,
            Some(2),
            observed + Duration::from_secs(1),
        );

        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Codex),
            AgentState::Idle,
            false,
            false,
            false,
            true,
            observed,
        );

        let authority = terminal.hook_authority.as_ref().expect("hook authority");
        assert_eq!(authority.custom_status.as_deref(), Some("new turn"));
        assert_eq!(terminal.state, AgentState::Working);
        assert_eq!(terminal.effective_agent_label(), Some("codex"));
    }

    #[test]
    fn detected_agent_change_clears_previous_matching_hook_authority() {
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Idle);
        terminal.set_hook_authority(
            "flock:codex".into(),
            "codex".into(),
            AgentState::Idle,
            None,
            None,
        );

        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Working);

        assert!(terminal.hook_authority.is_none());
        assert_eq!(terminal.detected_agent, Some(Agent::OpenCode));
        assert_eq!(terminal.effective_agent_label(), Some("opencode"));
        assert_eq!(terminal.state, AgentState::Working);
    }

    #[test]
    fn release_agent_clears_identity_immediately() {
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        terminal.set_hook_authority(
            "flock:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            None,
        );

        terminal.release_agent("flock:pi", "pi", None);

        assert!(terminal.hook_authority.is_none());
        assert_eq!(terminal.detected_agent, None);
        assert_eq!(terminal.fallback_state, AgentState::Unknown);
        assert_eq!(terminal.state, AgentState::Unknown);
    }

    #[test]
    fn stale_hook_report_sequence_is_ignored_for_same_source() {
        let mut terminal = test_terminal();
        terminal.set_hook_authority(
            "flock:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            Some(20),
        );

        let change = terminal.set_hook_authority(
            "flock:pi".into(),
            "pi".into(),
            AgentState::Idle,
            None,
            Some(19),
        );

        assert!(change.is_none());
        assert_eq!(terminal.state, AgentState::Working);
        assert_eq!(
            terminal.hook_authority.as_ref().unwrap().state,
            AgentState::Working
        );
    }

    #[test]
    fn accepted_hook_report_stores_session_ref() {
        let mut terminal = test_terminal();
        let mutation = terminal
            .set_hook_authority_with_session_ref(
                "flock:pi".into(),
                "pi".into(),
                AgentState::Working,
                None,
                None,
                crate::agent_resume::AgentSessionRef::path("/tmp/pi.jsonl"),
                Some(20),
            )
            .expect("accepted report");

        assert!(mutation.session_ref_changed);
        assert_eq!(
            terminal
                .hook_authority
                .as_ref()
                .and_then(|authority| authority.session_ref.as_ref())
                .map(|session_ref| (&session_ref.kind, session_ref.value.as_str())),
            Some((
                &crate::agent_resume::AgentSessionRefKind::Path,
                "/tmp/pi.jsonl"
            ))
        );
    }

    #[test]
    fn stale_hook_report_cannot_overwrite_session_ref() {
        let mut terminal = test_terminal();
        terminal.set_hook_authority_with_session_ref(
            "flock:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            None,
            crate::agent_resume::AgentSessionRef::path("/tmp/pi.jsonl"),
            Some(20),
        );

        let mutation = terminal.set_hook_authority_with_session_ref(
            "flock:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            None,
            crate::agent_resume::AgentSessionRef::path("/tmp/new.jsonl"),
            Some(19),
        );

        assert!(mutation.is_none());
        assert_eq!(
            terminal
                .hook_authority
                .as_ref()
                .and_then(|authority| authority.session_ref.as_ref())
                .map(|session_ref| session_ref.value.as_str()),
            Some("/tmp/pi.jsonl")
        );
    }

    #[test]
    fn accepted_hook_report_without_session_ref_clears_previous_ref() {
        let mut terminal = test_terminal();
        terminal.set_hook_authority_with_session_ref(
            "flock:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            None,
            crate::agent_resume::AgentSessionRef::path("/tmp/pi.jsonl"),
            Some(20),
        );

        let mutation = terminal
            .set_hook_authority_with_session_ref(
                "flock:pi".into(),
                "pi".into(),
                AgentState::Working,
                None,
                None,
                None,
                Some(21),
            )
            .expect("accepted report");

        assert!(mutation.session_ref_changed);
        assert!(mutation.effective_state_change.is_none());
        assert!(terminal
            .hook_authority
            .as_ref()
            .unwrap()
            .session_ref
            .is_none());
    }

    #[test]
    fn accepted_hook_report_marks_changed_when_session_identity_changes() {
        let mut terminal = test_terminal();
        terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "flock:opencode".into(),
            agent: "opencode".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("same-session").unwrap(),
        });

        let mutation = terminal
            .set_hook_authority_with_session_ref(
                "flock:hermes".into(),
                "hermes".into(),
                AgentState::Working,
                None,
                None,
                crate::agent_resume::AgentSessionRef::id("same-session"),
                Some(20),
            )
            .expect("accepted report");

        assert!(mutation.session_ref_changed);
    }

    #[test]
    fn different_same_agent_session_ref_is_ignored_until_current_session_clears() {
        let mut terminal = test_terminal();
        terminal
            .set_agent_session_ref(
                "flock:claude".into(),
                "claude".into(),
                crate::agent_resume::AgentSessionRef::id("claude-session"),
                Some(20),
            )
            .expect("initial session should be accepted");

        let mutation = terminal.set_agent_session_ref(
            "flock:claude".into(),
            "claude".into(),
            crate::agent_resume::AgentSessionRef::id("nested-session"),
            Some(21),
        );

        assert!(mutation.is_none());
        assert_eq!(
            terminal.hook_report_sequences.get("flock:claude"),
            Some(&21)
        );
        assert_eq!(
            terminal
                .persisted_agent_session
                .as_ref()
                .map(|session| session.session_ref.value.as_str()),
            Some("claude-session")
        );
    }

    #[test]
    fn repeated_same_agent_session_ref_is_accepted_without_session_change() {
        let mut terminal = test_terminal();
        terminal
            .set_agent_session_ref(
                "flock:claude".into(),
                "claude".into(),
                crate::agent_resume::AgentSessionRef::id("claude-session"),
                Some(20),
            )
            .expect("initial session should be accepted");

        let mutation = terminal
            .set_agent_session_ref(
                "flock:claude".into(),
                "claude".into(),
                crate::agent_resume::AgentSessionRef::id("claude-session"),
                Some(21),
            )
            .expect("same session should be accepted");

        assert!(!mutation.session_ref_changed);
    }

    #[test]
    fn hook_authority_preserves_current_session_ref_when_incoming_ref_differs() {
        let mut terminal = test_terminal();
        terminal
            .set_hook_authority_with_session_ref(
                "flock:copilot".into(),
                "copilot".into(),
                AgentState::Working,
                None,
                None,
                crate::agent_resume::AgentSessionRef::id("copilot-session"),
                Some(20),
            )
            .expect("initial session should be accepted");

        let mutation = terminal
            .set_hook_authority_with_session_ref(
                "flock:copilot".into(),
                "copilot".into(),
                AgentState::Blocked,
                Some("needs approval".into()),
                None,
                crate::agent_resume::AgentSessionRef::id("nested-session"),
                Some(21),
            )
            .expect("state update should still be accepted");

        assert!(!mutation.session_ref_changed);
        assert_eq!(terminal.state, AgentState::Blocked);
        assert_eq!(
            terminal
                .hook_authority
                .as_ref()
                .and_then(|authority| authority.session_ref.as_ref())
                .map(|session_ref| session_ref.value.as_str()),
            Some("copilot-session")
        );
    }

    #[test]
    fn different_same_agent_session_ref_is_accepted_after_detection_clears_current_session() {
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Working);
        terminal
            .set_agent_session_ref(
                "flock:claude".into(),
                "claude".into(),
                crate::agent_resume::AgentSessionRef::id("claude-session"),
                Some(20),
            )
            .expect("initial session should be accepted");

        let clear = terminal.set_detected_state_with_mutation(None, AgentState::Unknown);
        assert!(clear.session_ref_changed);

        let mutation = terminal
            .set_agent_session_ref(
                "flock:claude".into(),
                "claude".into(),
                crate::agent_resume::AgentSessionRef::id("new-session"),
                Some(21),
            )
            .expect("new session should be accepted after clear");

        assert!(mutation.session_ref_changed);
        assert_eq!(
            terminal
                .persisted_agent_session
                .as_ref()
                .map(|session| session.session_ref.value.as_str()),
            Some("new-session")
        );
    }

    #[test]
    fn clearing_hook_authority_clears_session_ref() {
        let mut terminal = test_terminal();
        terminal.set_hook_authority_with_session_ref(
            "flock:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            None,
            crate::agent_resume::AgentSessionRef::path("/tmp/pi.jsonl"),
            Some(20),
        );

        let mutation = terminal
            .clear_hook_authority_with_mutation(Some("flock:pi"), Some(21))
            .expect("accepted clear");

        assert!(mutation.session_ref_changed);
        assert!(terminal.hook_authority.is_none());
    }

    #[test]
    fn release_agent_clears_session_ref() {
        let mut terminal = test_terminal();
        terminal.set_hook_authority_with_session_ref(
            "flock:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            None,
            crate::agent_resume::AgentSessionRef::path("/tmp/pi.jsonl"),
            Some(20),
        );

        let mutation = terminal
            .release_agent_with_mutation("flock:pi", "pi", Some(21))
            .expect("accepted release");

        assert!(mutation.session_ref_changed);
        assert!(terminal.hook_authority.is_none());
    }

    #[test]
    fn release_agent_clears_matching_restored_session_ref_before_detection() {
        let mut terminal = test_terminal();
        terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "flock:hermes".into(),
            agent: "hermes".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("hermes-session").unwrap(),
        });

        let mutation = terminal
            .release_agent_with_mutation("flock:hermes", "hermes", Some(21))
            .expect("accepted release");

        assert!(mutation.session_ref_changed);
        assert!(mutation.effective_state_change.is_none());
        assert!(terminal.persisted_agent_session.is_none());
    }

    #[test]
    fn respawn_cleanup_resets_restored_agent_status() {
        let mut terminal = test_terminal();
        terminal.respawn_shell_on_exit = true;
        terminal.set_agent_name("codex".into());
        terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "flock:codex".into(),
            agent: "codex".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("codex-session").unwrap(),
        });
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Idle);

        terminal.clear_agent_runtime_identity_after_respawn();

        assert_eq!(terminal.state, AgentState::Unknown);
        assert!(terminal.detected_agent.is_none());
        assert!(terminal.agent_name.is_none());
        assert!(terminal.persisted_agent_session.is_none());
        assert!(!terminal.respawn_shell_on_exit);
    }

    #[test]
    fn detected_conflict_clears_session_ref() {
        let mut terminal = test_terminal();
        terminal.set_hook_authority_with_session_ref(
            "flock:claude".into(),
            "claude".into(),
            AgentState::Working,
            None,
            None,
            crate::agent_resume::AgentSessionRef::id("claude-session"),
            Some(20),
        );

        let mutation =
            terminal.set_detected_state_with_mutation(Some(Agent::Grok), AgentState::Idle);

        assert!(mutation.session_ref_changed);
        assert!(terminal.hook_authority.is_none());
    }

    #[test]
    fn detected_agent_disappearance_clears_matching_hook_session_ref() {
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::Hermes), AgentState::Idle);
        terminal.set_hook_authority_with_session_ref(
            "flock:hermes".into(),
            "hermes".into(),
            AgentState::Working,
            None,
            None,
            crate::agent_resume::AgentSessionRef::id("hermes-session"),
            Some(20),
        );

        let mutation = terminal.set_detected_state_with_mutation(None, AgentState::Unknown);

        assert!(mutation.session_ref_changed);
        assert!(terminal.hook_authority.is_none());
        assert!(terminal.persisted_agent_session.is_none());
        assert_eq!(terminal.effective_agent_label(), None);
    }

    #[test]
    fn detected_agent_disappearance_clears_matching_persisted_session_ref() {
        let mut terminal = test_terminal();
        terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "flock:opencode".into(),
            agent: "opencode".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("opencode-session").unwrap(),
        });

        let first =
            terminal.set_detected_state_with_mutation(Some(Agent::OpenCode), AgentState::Idle);
        assert!(!first.session_ref_changed);
        assert!(terminal.persisted_agent_session.is_some());

        let second = terminal.set_detected_state_with_mutation(None, AgentState::Unknown);
        assert!(second.session_ref_changed);
        assert!(terminal.persisted_agent_session.is_none());
    }

    #[test]
    fn initial_unknown_detection_preserves_restored_session_ref() {
        let mut terminal = test_terminal();
        terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "flock:hermes".into(),
            agent: "hermes".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("hermes-session").unwrap(),
        });

        let mutation = terminal.set_detected_state_with_mutation(None, AgentState::Unknown);
        assert!(!mutation.session_ref_changed);
        assert!(terminal.persisted_agent_session.is_some());
    }

    #[test]
    fn unsequenced_hook_report_is_ignored_after_source_uses_sequence() {
        let mut terminal = test_terminal();
        terminal.set_hook_authority(
            "flock:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            Some(20),
        );

        let change = terminal.set_hook_authority(
            "flock:pi".into(),
            "pi".into(),
            AgentState::Idle,
            None,
            None,
        );

        assert!(change.is_none());
        assert_eq!(terminal.state, AgentState::Working);
    }

    #[test]
    fn stale_release_sequence_is_ignored_for_same_source() {
        let mut terminal = test_terminal();
        terminal.set_hook_authority(
            "flock:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            Some(20),
        );

        let change = terminal.release_agent("flock:pi", "pi", Some(19));

        assert!(change.is_none());
        assert_eq!(terminal.state, AgentState::Working);
        assert!(terminal.hook_authority.is_some());
    }

    #[test]
    fn stale_clear_all_sequence_is_checked_against_current_authority_source() {
        let mut terminal = test_terminal();
        terminal.set_hook_authority(
            "flock:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            Some(20),
        );

        let change = terminal.clear_hook_authority(None, Some(19));

        assert!(change.is_none());
        assert_eq!(terminal.state, AgentState::Working);
        assert!(terminal.hook_authority.is_some());
    }

    #[test]
    fn same_sequence_from_different_sources_is_independent() {
        let mut terminal = test_terminal();
        terminal.set_hook_authority(
            "flock:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            Some(20),
        );

        terminal.set_hook_authority(
            "custom:pi".into(),
            "pi".into(),
            AgentState::Idle,
            None,
            Some(19),
        );

        assert_eq!(terminal.state, AgentState::Idle);
        assert_eq!(
            terminal.hook_authority.as_ref().unwrap().source,
            "custom:pi"
        );
    }

    /// A nested `claude -p` invocation inherits the parent pane's
    /// `FLOCK_PANE_ID` and fires its own SessionStart hook with a fresh
    /// session id. We must not let it hijack the restored id for the pane.
    #[test]
    fn claude_nested_startup_session_does_not_replace_restored_session_ref() {
        let mut terminal = test_terminal();
        terminal
            .set_agent_session_ref_for_session_start(
                "flock:claude".into(),
                "claude".into(),
                crate::agent_resume::AgentSessionRef::id("claude-session"),
                Some(20),
                Some("startup".into()),
            )
            .expect("initial session should be accepted");

        // Same `source` + `agent`, fresh id, `startup` source — this is the
        // nested `claude -p` shape. It must be ignored.
        let mutation = terminal.set_agent_session_ref_for_session_start(
            "flock:claude".into(),
            "claude".into(),
            crate::agent_resume::AgentSessionRef::id("nested-startup-session"),
            Some(21),
            Some("startup".into()),
        );
        assert!(
            mutation.is_none(),
            "nested startup session must not produce a mutation"
        );
        assert_eq!(
            terminal
                .persisted_agent_session
                .as_ref()
                .map(|session| session.session_ref.value.as_str()),
            Some("claude-session"),
            "nested startup must not overwrite the restored id"
        );
    }

    /// Real lifecycle changes (/clear, /resume, compaction) genuinely rotate
    /// the session id and must be persisted so resume works after a restart.
    #[test]
    fn claude_lifecycle_session_start_replaces_existing_session_ref() {
        for session_start_source in ["clear", "resume", "compact"] {
            let mut terminal = test_terminal();
            terminal
                .set_agent_session_ref_for_session_start(
                    "flock:claude".into(),
                    "claude".into(),
                    crate::agent_resume::AgentSessionRef::id("claude-session"),
                    Some(20),
                    Some("startup".into()),
                )
                .expect("initial session should be accepted");

            let next_session = format!("{session_start_source}-session");
            let mutation = terminal
                .set_agent_session_ref_for_session_start(
                    "flock:claude".into(),
                    "claude".into(),
                    crate::agent_resume::AgentSessionRef::id(&next_session),
                    Some(21),
                    Some(session_start_source.into()),
                )
                .unwrap_or_else(|| panic!("`{session_start_source}` must replace the session id"));
            assert!(
                mutation.session_ref_changed,
                "{session_start_source} should mark the session changed"
            );
            assert_eq!(
                terminal
                    .persisted_agent_session
                    .as_ref()
                    .map(|session| session.session_ref.value.as_str()),
                Some(next_session.as_str()),
                "{session_start_source} should store the replacement session id"
            );
        }
    }

    /// Repeating the same id (idle SessionStart with no rotation) is fine —
    /// no conflict, no mutation noise.
    #[test]
    fn claude_repeated_same_session_ref_is_accepted_without_change() {
        let mut terminal = test_terminal();
        terminal
            .set_agent_session_ref_for_session_start(
                "flock:claude".into(),
                "claude".into(),
                crate::agent_resume::AgentSessionRef::id("claude-session"),
                Some(20),
                Some("startup".into()),
            )
            .expect("initial session should be accepted");

        let mutation = terminal
            .set_agent_session_ref_for_session_start(
                "flock:claude".into(),
                "claude".into(),
                crate::agent_resume::AgentSessionRef::id("claude-session"),
                Some(21),
                Some("startup".into()),
            )
            .expect("same id should still flow through");
        assert!(!mutation.session_ref_changed);
        assert_eq!(
            terminal
                .persisted_agent_session
                .as_ref()
                .map(|session| session.session_ref.value.as_str()),
            Some("claude-session"),
        );
    }

    // ------------------------------------------------------------------
    // #309: hook TTL, and what a blipping screen may no longer do.
    // ------------------------------------------------------------------

    fn hook_working(terminal: &mut TerminalState, at: Instant) {
        terminal.set_hook_authority_with_custom_status_at(
            "flock:claude".into(),
            "claude".into(),
            AgentState::Working,
            None,
            None,
            None,
            None,
            at,
        );
    }

    fn screen(
        terminal: &mut TerminalState,
        state: AgentState,
        visible_idle: bool,
        at: Instant,
    ) -> TerminalStateMutation {
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Claude),
            state,
            false,
            visible_idle,
            false,
            false,
            at,
        )
    }

    /// The #309 headline. A host that wires `UserPromptSubmit` but not `Stop`
    /// pins hook authority to `Working` forever. Once the report ages past
    /// `HOOK_AUTHORITY_TTL` the screen simply wins — no veto grace involved.
    #[test]
    fn expired_hook_authority_yields_to_the_screen() {
        let now = Instant::now();
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Working);
        hook_working(&mut terminal, now);

        // Inside the TTL the hook still owns the pane.
        screen(
            &mut terminal,
            AgentState::Idle,
            true,
            now + Duration::from_millis(300),
        );
        assert_eq!(terminal.state, AgentState::Working);
        assert!(!terminal.hook_authority_expired(now + Duration::from_millis(300)));

        // Past it, the screen decides and the change names the authority.
        let after = now + HOOK_AUTHORITY_TTL + Duration::from_secs(1);
        let change = screen(&mut terminal, AgentState::Idle, true, after);
        assert_eq!(terminal.state, AgentState::Idle);
        assert!(terminal.hook_authority_expired(after));
        assert_eq!(
            change.effective_state_change.unwrap().authority,
            StateAuthority::HookExpired
        );
    }

    /// Once the hook has expired, a screen blip can no longer hand the pane
    /// back to it. Before #309 a single `visible_idle == false` frame reset the
    /// veto window and the pane snapped to `Working` for another full grace.
    #[test]
    fn a_blip_cannot_resurrect_an_expired_hook() {
        let now = Instant::now();
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Working);
        hook_working(&mut terminal, now);

        let settled = now + HOOK_AUTHORITY_TTL + Duration::from_secs(1);
        screen(&mut terminal, AgentState::Idle, true, settled);
        assert_eq!(terminal.state, AgentState::Idle);

        // One frame where the prompt box did not parse. Still Idle, and still
        // the screen's call — the stale hook does not come back.
        let blip = settled + Duration::from_millis(300);
        screen(&mut terminal, AgentState::Idle, false, blip);
        assert_eq!(
            terminal.state,
            AgentState::Idle,
            "an expired hook must not reclaim the pane on a blip"
        );
        assert_eq!(terminal.last_state_authority, StateAuthority::HookExpired);
    }

    /// A fresh hook is still authoritative — the TTL must not quietly disable
    /// hooks on hosts where they work.
    #[test]
    fn fresh_hook_still_outranks_a_contradicting_screen() {
        let now = Instant::now();
        let mut terminal = test_terminal();
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Working);
        hook_working(&mut terminal, now);
        let change = screen(
            &mut terminal,
            AgentState::Idle,
            true,
            now + Duration::from_millis(500),
        );
        assert!(change.effective_state_change.is_none());
        assert_eq!(terminal.state, AgentState::Working);
        assert_eq!(terminal.last_state_authority, StateAuthority::Hook);
    }

    /// Never-reported and expired are different things, and the authority
    /// label distinguishes them for the log.
    #[test]
    fn absent_and_expired_hooks_are_distinguishable() {
        let now = Instant::now();
        let mut terminal = test_terminal();
        screen(&mut terminal, AgentState::Idle, true, now);
        assert!(
            !terminal.hook_authority_expired(now),
            "never reported is not expired"
        );
        assert_eq!(terminal.last_state_authority, StateAuthority::Screen);
        assert_eq!(StateAuthority::Screen.label(), "screen");
        assert_eq!(StateAuthority::HookExpired.label(), "hook_expired");
    }

    /// The Working->Idle hold is no longer Claude-only (#309 P9 sibling): every
    /// agent gets the same damping, so a single detector tick cannot flip the
    /// sidebar for codex/gemini/cursor/droid.
    #[test]
    fn working_hold_applies_to_every_agent() {
        let now = Instant::now();
        for agent in [
            Agent::Claude,
            Agent::Codex,
            Agent::Gemini,
            Agent::Cursor,
            Agent::Droid,
        ] {
            let mut last_working = Some(now);
            assert_eq!(
                stabilize_agent_state(
                    Some(agent),
                    AgentState::Working,
                    AgentState::Idle,
                    now + Duration::from_millis(300),
                    &mut last_working,
                ),
                AgentState::Working,
                "{agent:?} must hold through a single-frame Idle"
            );
            assert_eq!(
                stabilize_agent_state(
                    Some(agent),
                    AgentState::Working,
                    AgentState::Idle,
                    now + CLAUDE_WORKING_HOLD + Duration::from_millis(1),
                    &mut last_working,
                ),
                AgentState::Idle,
                "{agent:?} must still settle once the hold elapses"
            );
        }
    }
}
