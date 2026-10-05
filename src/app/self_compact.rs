//! Driving an armed self-compaction to completion (#540).
//!
//! A sibling of [`crate::app::idle_wake`] rather than a parameter of it, and
//! the reason is worth stating because the two look alike from outside. Both
//! are flock typing into a pane on its own initiative, and both need the same
//! four protections: a gate re-checked immediately before each keystroke, no
//! typing over a human, a submit gap between text and Enter, and `Enter` only
//! if nothing changed underneath it. Everything after that diverges. The idle
//! wake types one flock-authored constant, is gated by the mailbox, and runs
//! at most once per queued set. A self-compaction types a slash command and
//! then the agent's *own* words, is gated by nothing but the pane being
//! genuinely idle, and runs exactly once per arming — a second time being a
//! bug, not a retry. Sharing the tracker would mean either the message path
//! growing a "what to type" parameter (ADR-0008's injection hole, one argument
//! wide) or the compaction inheriting mute semantics that have nothing to say
//! to it.
//!
//! So this module owns three things and nothing else:
//!
//! - [`App::arm_self_compact`] / [`App::clear_self_compact`] — the state
//!   changes, driven by an event rather than by the tick.
//! - [`App::tick_self_compacts`] — the mirror of `tick_idle_wakes`, called from
//!   both loops on the #25 dual-loop rule.
//! - [`App::self_compact_on_session_start`] — the harness reporting that the
//!   compaction happened, which is the only proof that it did.
//!
//! The state itself, and every question asked about it, is pure and lives in
//! [`crate::agent_self_compact`].

use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::agent_self_compact::{
    ArmedSelfCompact, SelfCompactAbort, SelfCompactPhase, COMPACT_COMMAND, MAX_CONTINUATION_BYTES,
};
use crate::app::App;

impl App {
    /// The agent asked to compact its own context and carry on.
    ///
    /// Writes nothing. The caller is mid-turn when it asks, and the harness it
    /// is running inside cannot compact inside a turn — so this only records
    /// the handoff prompt, and the tick does the rest once the turn ends.
    ///
    /// The caller is responsible for having already refused what this cannot:
    /// a disabled config, and an agent whose harness has no compaction to ask
    /// for. Both are refusals at the verb rather than armings that go nowhere,
    /// because an agent that armed and then waited forever would have no way
    /// to tell that from an agent waiting for a very long turn.
    ///
    /// Returns whether the arming was stored. `false` means one was already
    /// armed and this prompt was **not** stored: an agent that arms twice has
    /// two handoff prompts written, and silently keeping the second would
    /// discard work it believed it had saved.
    pub(crate) fn arm_self_compact(
        &mut self,
        pane: &str,
        continuation: String,
        now: Instant,
    ) -> bool {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(pane) else {
            return false;
        };
        let floor = Duration::from_secs(self.state.config.session.self_compact_min_interval_secs);
        let Some(terminal) = self.terminal_mut_for_pane(ws_idx, pane_id) else {
            return false;
        };
        if terminal.armed_self_compact.is_some() {
            return false;
        }
        if terminal
            .last_self_compact_completed
            .is_some_and(|last| now.saturating_duration_since(last) < floor)
        {
            return false;
        }
        terminal.armed_self_compact = Some(ArmedSelfCompact::new(continuation));
        crate::logging::self_compact_armed(pane, terminal_continuation_len(terminal));
        true
    }

    /// Drop an armed self-compaction. Answers whether there was one, so an
    /// agent that disarms is told whether it disarmed anything.
    pub(crate) fn clear_self_compact(&mut self, pane: &str) -> bool {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(pane) else {
            return false;
        };
        let Some(terminal) = self.terminal_mut_for_pane(ws_idx, pane_id) else {
            return false;
        };
        terminal.armed_self_compact.take().is_some()
    }

    /// The harness reported a session start. If it reported a *compaction* and
    /// one was armed and waiting, this is the signal the whole sequence exists
    /// to wait for: deliver the agent's handoff prompt as its next turn.
    ///
    /// The `compact` check is not decoration. A session start that is not a
    /// compaction means the `/compact` flock typed did not compact — the
    /// operator cancelled it, or the harness started a fresh session instead —
    /// and delivering a continuation into an uncompacted session would be a lie
    /// about what happened to the context. The arming is dropped instead, and
    /// the abort reason says which half was lost.
    pub(crate) fn self_compact_on_session_start(
        &mut self,
        pane: &str,
        session_start_source: Option<&str>,
        now: Instant,
    ) {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(pane) else {
            return;
        };
        let Some(armed) = self
            .terminal_for_pane(ws_idx, pane_id)
            .and_then(|terminal| terminal.armed_self_compact.as_ref())
        else {
            return;
        };
        if session_start_source != Some("compact") {
            // Only a start we can explain drops the arming. A `resume` or
            // `startup` while merely armed (nothing typed yet) is a restart,
            // not a failure, and the arming outlives it.
            if armed.phase.has_written() {
                self.abort_self_compact(pane, SelfCompactAbort::UnexpectedSessionStart);
            }
            return;
        }
        if armed.phase != SelfCompactPhase::CompactRequested {
            // A compaction while nothing has been typed is the harness doing it
            // on its own — and Claude Code compacts by itself exactly when the
            // context fills up, which is the moment an agent reaches for this
            // verb. That is the signal we were waiting for, just earlier and
            // unrequested, so take it: record the phase so the tick does not
            // type a `/compact` into a pane that has already compacted, and
            // deliver.
            //
            // The remaining case is `ContinueTyped`, where the `/compact` Enter
            // has not landed and the harness therefore cannot have finished —
            // or the continuation is already typed and this is a duplicate
            // report. Either way, leave it to the tick.
            if armed.phase != SelfCompactPhase::Armed {
                return;
            }
            if let Some(terminal) = self.terminal_mut_for_pane(ws_idx, pane_id) {
                if let Some(armed) = terminal.armed_self_compact.as_mut() {
                    // No `submit_at`: nothing is waiting on an Enter.
                    armed.phase = SelfCompactPhase::CompactRequested;
                    armed.submit_at = None;
                }
            }
        }
        // Advance the phase only. Typing happens on a later tick, behind the
        // gates: Claude Code auto-compacts MID-TURN, so a confirmation can
        // arrive with the agent still working, and the prompt box may hold a
        // draft. Typing from here would append a handoff prompt to whatever
        // the agent is part-way through typing.
        if let Some(terminal) = self.terminal_mut_for_pane(ws_idx, pane_id) {
            if let Some(armed) = terminal.armed_self_compact.as_mut() {
                armed.compaction_confirmed(now);
            }
        }
    }

    /// The loop tick — mirrored in both loops (#25). An arming waiting for its
    /// turn to end, a settle or quiet window that lifts, the Enter of a typed
    /// write, or a timeout coming due.
    pub(crate) fn tick_self_compacts(&mut self, now: Instant) {
        self.self_compact_deadline = None;
        if !self.state.config.session.self_compact {
            return;
        }
        for (pane, ws_idx, pane_id) in self.armed_self_compact_panes() {
            match self.decide_self_compact(&pane, ws_idx, pane_id, now) {
                SelfCompactDecision::Suppressed(reason) => {
                    crate::logging::self_compact_suppressed(&pane, reason);
                }
                SelfCompactDecision::CompactTyped => {
                    crate::logging::self_compact_typed(&pane);
                }
                SelfCompactDecision::Submitted | SelfCompactDecision::ContinuationTyped => {}
                SelfCompactDecision::Abandoned(reason) => self.abort_self_compact(&pane, reason),
            }
        }
    }

    /// Panes with an armed compaction, as `(public pane id, workspace index,
    /// pane)`. The walk is the same one `commit_settled_completions` does, and
    /// it is the pane tree rather than the terminal map because the answer needs
    /// a public pane id — the one shape every other per-pane sweep in the app
    /// is written in.
    fn armed_self_compact_panes(&self) -> Vec<(String, usize, crate::layout::PaneId)> {
        let mut armed = Vec::new();
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            for tab in &ws.tabs {
                for (pane_id, pane) in &tab.panes {
                    let armed_here = self
                        .state
                        .terminals
                        .get(&pane.attached_terminal_id)
                        .is_some_and(|terminal| terminal.armed_self_compact.is_some());
                    if !armed_here {
                        continue;
                    }
                    if let Some(public) = self.public_pane_id(ws_idx, *pane_id) {
                        armed.push((public, ws_idx, *pane_id));
                    }
                }
            }
        }
        armed
    }

    fn self_compact_settle(&self) -> Duration {
        Duration::from_millis(self.state.config.session.self_compact_settle_ms)
    }

    fn self_compact_quiet(&self) -> Duration {
        Duration::from_millis(self.state.config.session.self_compact_operator_quiet_ms)
    }

    /// How stale the screen may be before flock refuses to type into it.
    /// Deliberately a separate knob from [`Self::self_compact_settle`] — see
    /// `SessionConfig::self_compact_fresh_ms` for why tying them puts the
    /// loop's wake exactly on the freshness boundary.
    fn self_compact_fresh(&self) -> Duration {
        Duration::from_millis(self.state.config.session.self_compact_fresh_ms)
    }

    fn self_compact_timeout(&self) -> Duration {
        Duration::from_millis(self.state.config.session.self_compact_timeout_ms)
    }

    /// Wake the loop for the next thing an arming is waiting on, so a settle
    /// that lifts, a quiet window that expires, or a pending Enter is not left
    /// for unrelated traffic to stumble into.
    pub(crate) fn note_self_compact_deadline(&mut self, at: Instant) {
        self.self_compact_deadline = Some(self.self_compact_deadline.map_or(at, |c| c.min(at)));
    }

    fn decide_self_compact(
        &mut self,
        pane: &str,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        now: Instant,
    ) -> SelfCompactDecision {
        let Some(terminal) = self.terminal_for_pane(ws_idx, pane_id) else {
            return SelfCompactDecision::Abandoned(SelfCompactAbort::PaneGone);
        };
        let Some(armed) = terminal.armed_self_compact.clone() else {
            return SelfCompactDecision::Suppressed("not_armed");
        };

        // The timeout covers only the half flock cannot see into. Before
        // anything is typed there is nothing to bound — an agent may be
        // mid-turn for minutes, and an arming that expired in that window
        // would silently discard a handoff prompt the agent still believes it
        // saved.
        if let Some(reason) = armed.timeout_reason(now, self.self_compact_timeout()) {
            return SelfCompactDecision::Abandoned(SelfCompactAbort::Timeout(reason));
        }

        match armed.phase {
            SelfCompactPhase::CompactRequested | SelfCompactPhase::ContinueTyped => {
                if !armed.submit_due(now) {
                    if let Some(due) = armed.submit_at {
                        self.note_self_compact_deadline(due);
                    }
                    return SelfCompactDecision::Suppressed("submit_pending");
                }
                self.submit_self_compact(pane, ws_idx, pane_id, &armed, now)
            }
            // Both of these WRITE into the pane, so both go through the same
            // gates: idle, settled, operator-quiet, empty prompt box. That is
            // what makes the auto-compact path safe — Claude Code compacts
            // itself mid-turn, so a confirmation can arrive with the agent
            // working, and typing then would be typing at a working agent.
            SelfCompactPhase::Armed => {
                self.type_when_ready(pane, ws_idx, pane_id, now, Write::CompactCommand)
            }
            SelfCompactPhase::CompactDone => {
                self.type_when_ready(pane, ws_idx, pane_id, now, Write::Continuation)
            }
        }
    }

    /// Type one of the sequence's two writes into a pane that has genuinely
    /// stopped, behind every gate, and nothing if any of them says no.
    ///
    /// The gates are the idle wake's, deliberately. Both writes land in a
    /// prompt box a human may be sitting at, and both are text nobody can take
    /// back once submitted:
    ///
    /// - the agent has been idle for a settle window, and the screen positively
    ///   shows its idle prompt, read recently — an unreadable screen or a hook
    ///   that went quiet (#309) is not idle;
    /// - no operator keystroke reached the pane inside a quiet window;
    /// - the prompt box on screen is empty. A draft left sitting there would be
    ///   submitted together with what flock types, which is the one outcome
    ///   that cannot be undone. Claude Code auto-compacting MID-TURN is exactly
    ///   how this gate earns its keep: the confirmation arrives while the agent
    ///   is working, and without the box check flock would append a handoff
    ///   prompt to whatever the agent is part-way through typing.
    /// - the idle wake (ADR-0018 §2) has nothing in flight for this pane, so two
    ///   flock-authored submissions cannot land in the same gap and be
    ///   submitted as one line.
    ///
    /// Every gate is re-checked in the same call that types, with nothing
    /// between the last check and the write.
    fn type_when_ready(
        &mut self,
        pane: &str,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        now: Instant,
        write: Write,
    ) -> SelfCompactDecision {
        let settle = self.self_compact_settle();
        let Some(terminal) = self.terminal_for_pane(ws_idx, pane_id) else {
            return SelfCompactDecision::Abandoned(SelfCompactAbort::PaneGone);
        };
        // The verb refuses an agent that cannot compact itself, so this should
        // be unreachable. It is a safety net rather than a gate because
        // dropping a whole arming — an agent's written handoff prompt — is not a
        // consequence a detection change should be able to cause.
        if !crate::agent_self_compact::agent_can_self_compact(terminal.effective_known_agent()) {
            return SelfCompactDecision::Suppressed("cannot_self_compact");
        }
        if self.idle_wake.in_flight(pane) {
            return SelfCompactDecision::Suppressed("idle_wake_in_flight");
        }
        if let Some(blocker) = terminal.idle_wake_blocker(now, settle, self.self_compact_fresh()) {
            if blocker == "not_settled" {
                if let Some(settles_at) = terminal.state_settles_at(settle) {
                    self.note_self_compact_deadline(settles_at);
                }
            }
            return SelfCompactDecision::Suppressed(blocker);
        }
        let Some(runtime) = self.lookup_runtime_sender(ws_idx, pane_id) else {
            return SelfCompactDecision::Abandoned(SelfCompactAbort::PaneGone);
        };
        if let Some(last_input) = runtime.last_operator_input_at() {
            let quiet_until = last_input + self.self_compact_quiet();
            if now < quiet_until {
                self.note_self_compact_deadline(quiet_until);
                return SelfCompactDecision::Suppressed("operator_active");
            }
        }
        // Quiet is not empty: a draft left in the box would be submitted with
        // this. Read the screen here, at the keystroke, never on an earlier tick.
        let agent = terminal.effective_known_agent();
        match agent
            .and_then(|agent| crate::detect::agent_prompt_is_empty(agent, &runtime.visible_text()))
        {
            Some(true) => {}
            Some(false) => return SelfCompactDecision::Suppressed("prompt_not_empty"),
            None => return SelfCompactDecision::Suppressed("no_prompt_box"),
        }

        let Some(terminal) = self.terminal_for_pane(ws_idx, pane_id) else {
            return SelfCompactDecision::Abandoned(SelfCompactAbort::PaneGone);
        };
        let text = match write {
            Write::CompactCommand => COMPACT_COMMAND.to_string(),
            Write::Continuation => {
                let Some(continuation) = terminal
                    .armed_self_compact
                    .as_ref()
                    .map(|armed| armed.continuation.clone())
                else {
                    return SelfCompactDecision::Suppressed("not_armed");
                };
                continuation
            }
        };
        let bytes = super::api_helpers::encode_api_text(runtime, &text);
        if runtime.try_send_flock_authored(Bytes::from(bytes)).is_err() {
            return SelfCompactDecision::Abandoned(SelfCompactAbort::WriteFailed);
        }
        let gap = crate::cli::pane::PANE_RUN_SUBMIT_GAP;
        let Some(terminal) = self.terminal_mut_for_pane(ws_idx, pane_id) else {
            return SelfCompactDecision::Abandoned(SelfCompactAbort::PaneGone);
        };
        let Some(armed) = terminal.armed_self_compact.as_mut() else {
            return SelfCompactDecision::Suppressed("not_armed");
        };
        let typed_bytes = text.len();
        match write {
            Write::CompactCommand => armed.compact_requested(now, gap),
            Write::Continuation => armed.continuation_typed(now, gap),
        }
        if write == Write::Continuation {
            // The agent's prompt is now a prompt in its own session, so it
            // belongs in the pane's prompt history: that panel is where an
            // operator looks to answer "what did it tell itself to do next",
            // which is the whole reason this feature exists.
            terminal.record_prompt_at(text, now);
        }
        self.note_self_compact_deadline(now + gap);
        match write {
            Write::CompactCommand => {
                crate::logging::self_compact_typed(pane);
                SelfCompactDecision::CompactTyped
            }
            Write::Continuation => {
                crate::logging::self_compact_continued(pane, typed_bytes);
                SelfCompactDecision::ContinuationTyped
            }
        }
    }

    /// Press Enter on a write typed one gap ago — after checking, once more,
    /// that nothing changed underneath it.
    fn submit_self_compact(
        &mut self,
        pane: &str,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        armed: &ArmedSelfCompact,
        now: Instant,
    ) -> SelfCompactDecision {
        let typed_at = armed.typed_at.unwrap_or(now);
        let Some(terminal) = self.terminal_for_pane(ws_idx, pane_id) else {
            return SelfCompactDecision::Abandoned(SelfCompactAbort::PaneGone);
        };
        // The full freshness check, not just `state == Idle`: a screen that went
        // stale inside the gap is not evidence either. The settle is already
        // proven for the command itself, so it is not asked again.
        if terminal
            .idle_wake_blocker(now, Duration::ZERO, self.self_compact_fresh())
            .is_some()
        {
            return SelfCompactDecision::Abandoned(SelfCompactAbort::LeftIdle);
        }
        let Some(runtime) = self.lookup_runtime_sender(ws_idx, pane_id) else {
            return SelfCompactDecision::Abandoned(SelfCompactAbort::PaneGone);
        };
        if runtime
            .last_operator_input_at()
            .is_some_and(|input| input >= typed_at)
        {
            return SelfCompactDecision::Abandoned(SelfCompactAbort::OperatorActive);
        }
        if !press_enter(runtime) {
            return SelfCompactDecision::Abandoned(SelfCompactAbort::WriteFailed);
        }
        let Some(terminal) = self.terminal_mut_for_pane(ws_idx, pane_id) else {
            return SelfCompactDecision::Abandoned(SelfCompactAbort::PaneGone);
        };
        let completing = terminal
            .armed_self_compact
            .as_ref()
            .is_some_and(|armed| armed.phase == SelfCompactPhase::ContinueTyped);
        if completing {
            // The last thing the sequence does, and the arming is DROPPED rather
            // than reset. Left in place it logs every SUCCESS as a
            // `continuation_timeout` once its clock runs out, and refuses the
            // next arming as `pending` until then — so a feature meant to let an
            // agent keep working would wedge its own pane after one compaction.
            let bytes = terminal
                .armed_self_compact
                .take()
                .map_or(0, |armed| armed.continuation.len());
            terminal.last_self_compact_completed = Some(now);
            crate::logging::self_compact_completed(pane, bytes);
        } else if let Some(armed) = terminal.armed_self_compact.as_mut() {
            armed.compact_submitted(now);
        }
        SelfCompactDecision::Submitted
    }

    /// Drop an arming, and say why.
    ///
    /// Every abort really does drop it, which is the difference from the idle
    /// wake: that one deliberately leaves an abandoned wake in flight, so
    /// nothing more is typed into that pane until the agent moves. Here the
    /// arming is a claim on the *next* arming as well — `arm_self_compact`
    /// refuses while one exists — so leaving one behind after it has failed
    /// would wedge the pane against self-compaction permanently, which is
    /// exactly what the timeout exists to prevent.
    fn abort_self_compact(&mut self, pane: &str, reason: SelfCompactAbort) {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(pane) else {
            return;
        };
        let dropped = self
            .terminal_mut_for_pane(ws_idx, pane_id)
            .and_then(|terminal| terminal.armed_self_compact.take())
            .is_some();
        if dropped {
            crate::logging::self_compact_abandoned(pane, reason.as_str());
        }
    }

    /// Why this pane's agent could not be asked to compact itself, or `None` if
    /// it could. Asked at the verb, where a refusal can name a reason the
    /// agent can act on instead of leaving it waiting for a continuation that
    /// was never coming.
    pub(crate) fn self_compact_refusal(&self, pane: &str) -> Option<&'static str> {
        if !self.state.config.session.self_compact {
            return Some("disabled");
        }
        let (ws_idx, pane_id) = self.parse_pane_id(pane)?;
        let agent = self
            .terminal_for_pane(ws_idx, pane_id)
            .and_then(|terminal| terminal.effective_known_agent());
        if crate::agent_self_compact::agent_can_self_compact(agent) {
            None
        } else {
            Some("agent_cannot_self_compact")
        }
    }

    /// The terminal a pane's state lives on. Reached directly rather than
    /// through `update_terminal_state`, whose return value means "this change
    /// was broadcast to the UI" and not "this mutation was applied" — an arming
    /// is invisible state, and reading that `Option` as success would report
    /// every arming as refused.
    fn terminal_mut_for_pane(
        &mut self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<&mut crate::terminal::TerminalState> {
        let terminal_id = self.state.terminal_id_for_pane(ws_idx, pane_id)?;
        self.state.terminals.get_mut(&terminal_id)
    }
}

impl App {
    /// Test-only: reach a pane's terminal for assertions the App-level tests
    /// need to make directly. Kept beside the production lookups rather than
    /// reimplemented per test, so a test cannot drift onto a different path
    /// than the one it is testing.
    #[cfg(test)]
    pub(crate) fn terminal_mut_for_test(
        &mut self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> &mut crate::terminal::TerminalState {
        self.terminal_mut_for_pane(ws_idx, pane_id)
            .expect("test pane has a terminal")
    }

    /// Test-only: pretend the idle wake has a sentence typed and waiting for
    /// its Enter, so the mutual exclusion can be exercised without sending a
    /// real message through the mailbox.
    #[cfg(test)]
    pub(crate) fn test_arm_idle_wake_in_flight(&mut self, pane: &str) {
        crate::app::idle_wake::test_arm_in_flight(&mut self.idle_wake, pane);
    }
}

fn terminal_continuation_len(terminal: &crate::terminal::TerminalState) -> usize {
    terminal
        .armed_self_compact
        .as_ref()
        .map_or(0, |armed| armed.continuation.len())
}

fn press_enter(runtime: &crate::terminal::TerminalRuntime) -> bool {
    let Ok(mut keys) = super::api_helpers::encode_api_keys(runtime, &["Enter".to_string()]) else {
        return false;
    };
    let enter = keys.pop().unwrap_or_else(|| b"\r".to_vec());
    runtime.try_send_flock_authored(Bytes::from(enter)).is_ok()
}

/// What one evaluation decided for one armed compaction. Deliberately the same
/// four outcomes as the idle wake's `Decision`, so a reader who knows that one
/// knows this one. The continuation write is absent because it is not the
/// tick's to make: it belongs to the session hook that reports the compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelfCompactDecision {
    Suppressed(&'static str),
    CompactTyped,
    ContinuationTyped,
    Submitted,
    Abandoned(SelfCompactAbort),
}

/// Which of the sequence's two writes the gated writer is being asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Write {
    CompactCommand,
    Continuation,
}

/// Re-exported so the verb's handler and its tests agree on the cap.
pub(crate) const CONTINUATION_CAP: usize = MAX_CONTINUATION_BYTES;

#[cfg(test)]
#[path = "self_compact_tests.rs"]
mod tests;
