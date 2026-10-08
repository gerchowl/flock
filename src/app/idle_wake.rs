//! The idle wake (ADR-0018 §2): an agent that has already stopped is reached
//! by flock typing a fixed sentence into its pane.
//!
//! This is the one place flock types into an agent's pane on its own
//! initiative, and everything here exists to keep that narrow:
//!
//! - **The text is a constant.** [`idle_wake_text`] takes a count and nothing
//!   else — no sender, no subject, no body. Widening it reopens the injection
//!   hole ADR-0008 closed and needs its own ADR.
//! - **Every gate is re-checked immediately before the keystroke** (#316
//!   pitfall 1), and again before the Enter.
//! - **Flock does not type over a human** (pitfall 2): a pane that received
//!   operator input inside the quiet window is left alone.
//! - **Once per queued set** (pitfall 9): a wake marks the messages it
//!   announced and goes in flight; nothing is re-typed on a later tick. The
//!   in-flight marker clears when the agent reads its inbox or leaves `Idle`,
//!   and even then only a message not yet announced can earn another wake —
//!   an agent that ignored one is re-told at its next turn boundary by the
//!   stop hook, not by more typing.
//! - **Claude only** (pitfall 8): the sentence names a tool, so only an
//!   integration that has it is woken; every other agent stays pull-only.
//!
//! The per-tick cost is a scan of the in-memory mailbox, and nothing at all
//! when no queued message can wake anyone — no filesystem work (#262/#293).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::app::App;
use crate::detect::{Agent, AgentState};

/// The sentence typed into an idle agent's pane. The count is the only
/// variable, and it counts every queued message — an `fyi` a `needs_reply`
/// woke for is reported too (ADR-0018 §1).
pub(crate) fn idle_wake_text(count: usize) -> String {
    format!(
        "You have {count} unread message(s) from other agents. Read them with the \
         `flock_msg_read` tool."
    )
}

/// Whether an agent's integration exposes the inbox tool the sentence names.
/// Claude today; any other runtime would be told to call a tool it does not
/// have, so it stays pull-only until its integration says otherwise.
fn has_inbox_tool(agent: Option<Agent>) -> bool {
    matches!(agent, Some(Agent::Claude))
}

#[derive(Default)]
pub(crate) struct IdleWakeTracker {
    panes: HashMap<String, PaneWake>,
    /// The earliest moment a time-based suppression lifts or a pending Enter
    /// comes due, so an otherwise quiet loop wakes for it.
    next_deadline: Option<Instant>,
}

#[derive(Default)]
struct PaneWake {
    /// Correlation ids a typed wake has already told this agent about.
    announced: HashSet<String>,
    in_flight: Option<InFlight>,
    /// The last decision logged, so a suppression that holds for an hour is
    /// logged once, not once per tick.
    last_decision: Option<&'static str>,
}

struct InFlight {
    /// When the Enter is due; `None` once it was sent or abandoned.
    submit_at: Option<Instant>,
    count: usize,
    attempt: Option<super::guarded_submit::Attempt>,
}

/// Test-only: mark `pane` as having a typed sentence whose Enter has not gone
/// out. Used by `app::self_compact`'s tests, which must prove the two features
/// never submit into the same gap — a case that cannot be produced by sending
/// real mail, because the mailbox gates would keep both out of it.
#[cfg(test)]
pub(crate) fn test_arm_in_flight(tracker: &mut IdleWakeTracker, pane: &str) {
    let entry = tracker.panes.entry(pane.to_string()).or_default();
    entry.in_flight = Some(InFlight {
        submit_at: Some(Instant::now() + Duration::from_secs(30)),
        count: 1,
        attempt: None,
    });
}

impl IdleWakeTracker {
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.next_deadline
    }

    /// Whether a typed sentence is in flight for `pane`, Enter due or not.
    ///
    /// Read by `app::self_compact` before it types anything. Both features type
    /// flock-authored text and then press Enter one `pane run` gap later, and a
    /// gap is the same length for both — so two writes in the same window are
    /// two submissions the pane's reader cannot tell apart, and they go in as
    /// one line. One of them has to wait.
    pub(crate) fn in_flight(&self, pane: &str) -> bool {
        self.panes.get(pane).is_some_and(|entry| {
            entry
                .in_flight
                .as_ref()
                .is_some_and(|flight| flight.submit_at.is_some())
        })
    }

    fn note_deadline(&mut self, at: Instant) {
        self.next_deadline = Some(self.next_deadline.map_or(at, |current| current.min(at)));
    }
}

/// What one evaluation decided for one pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Suppressed(&'static str),
    Typed,
    Submitted,
    Abandoned(&'static str),
}

impl App {
    /// A message was just queued for `pane` — locally or relayed in from
    /// another host, both of which arrive through the same enqueue. Evaluate
    /// now rather than waiting for the loop to come round.
    pub(crate) fn idle_wake_on_enqueue(&mut self, pane: &str) {
        if !self.state.config.msg.idle_wake {
            return;
        }
        let ids = self.mailboxes.wakeable_ids(pane);
        if !ids.is_empty() {
            self.evaluate_idle_wake(pane, &ids, Instant::now());
        }
    }

    /// The agent read its inbox: whatever was in flight has landed. A wake
    /// whose Enter was still pending is dropped here, and that is logged like
    /// every other withheld Enter — its sentence is still in the prompt.
    pub(crate) fn idle_wake_on_read(&mut self, pane: &str) {
        let flight = self
            .idle_wake
            .panes
            .remove(pane)
            .and_then(|entry| entry.in_flight);
        if let Some(mut attempt) = flight.and_then(|flight| flight.attempt) {
            if attempt.evidence.submit_sent_at_ms.is_some() {
                attempt.evidence.state = "read".into();
                attempt.evidence.reason = Some("inbox_read".into());
                attempt.evidence.finished_at_ms = Some(super::api::messages::now_ms());
                self.record_delivery_attempt(&attempt.evidence);
            } else {
                self.finish_delivery_attempt(
                    &mut attempt,
                    &super::guarded_submit::Outcome::Abandoned("inbox_read"),
                );
                crate::logging::idle_wake_abandoned(pane, "inbox_read");
            }
        }
        for mut attempt in self.delivery_attempts() {
            if attempt.wake && attempt.pane == pane && attempt.state == "unconfirmed" {
                attempt.state = "read".into();
                attempt.reason = Some("inbox_read".into());
                attempt.finished_at_ms = Some(super::api::messages::now_ms());
                self.record_delivery_attempt(&attempt);
            }
        }
    }

    fn abandon_idle_wake(&mut self, pane: &str, reason: &'static str) {
        let attempt = self
            .idle_wake
            .panes
            .get_mut(pane)
            .and_then(|entry| entry.in_flight.take())
            .and_then(|flight| flight.attempt);
        if let Some(mut attempt) = attempt {
            self.finish_delivery_attempt(
                &mut attempt,
                &super::guarded_submit::Outcome::Abandoned(reason),
            );
            crate::logging::idle_wake_abandoned(pane, reason);
        }
    }

    /// The loop tick — messages that became wakeable later (a mute expired,
    /// the agent went idle, the operator stopped typing), and the Enter of a
    /// wake typed on an earlier tick. Mirrored in both loops (#25).
    pub(crate) fn tick_idle_wakes(&mut self, now: Instant) {
        if !self.state.config.msg.idle_wake {
            let panes: Vec<_> = self.idle_wake.panes.keys().cloned().collect();
            for pane in panes {
                self.abandon_idle_wake(&pane, "idle_wake_disabled");
            }
            self.idle_wake = IdleWakeTracker::default();
            return;
        }
        self.idle_wake.next_deadline = None;
        // Only panes this could ever type into. Mail for an agent without the
        // inbox tool can sit for hours, and must not cost a scan of its ids
        // on every tick of that time.
        let candidates: Vec<String> = self
            .mailboxes
            .wakeable_panes()
            .filter(|pane| self.pane_has_inbox_tool(pane))
            .map(str::to_owned)
            .collect();
        if candidates.is_empty() && self.idle_wake.panes.is_empty() {
            return;
        }
        let removed: Vec<_> = self
            .idle_wake
            .panes
            .keys()
            .filter(|pane| !candidates.contains(pane))
            .cloned()
            .collect();
        for pane in removed {
            self.abandon_idle_wake(&pane, "inbox_unavailable");
            // Detection can be unknown briefly after restart. Keep the durable
            // announcement while mail waits, so rediscovery cannot replay keys.
            if self.mailboxes.queued_len(&pane) == 0 {
                self.idle_wake.panes.remove(&pane);
            }
        }
        for pane in candidates {
            let ids = self.mailboxes.wakeable_ids(&pane);
            self.evaluate_idle_wake(&pane, &ids, now);
        }
    }

    fn pane_has_inbox_tool(&self, pane: &str) -> bool {
        self.parse_pane_id(pane)
            .and_then(|(ws_idx, pane_id)| self.terminal_for_pane(ws_idx, pane_id))
            .is_some_and(|terminal| has_inbox_tool(terminal.effective_known_agent()))
    }

    fn evaluate_idle_wake(&mut self, pane: &str, wakeable_ids: &[String], now: Instant) {
        let decision = self.decide_idle_wake(pane, wakeable_ids, now);
        let entry = self.idle_wake.panes.entry(pane.to_string()).or_default();
        match decision {
            Decision::Suppressed(reason) => {
                if entry.last_decision != Some(reason) {
                    crate::logging::idle_wake_suppressed(pane, reason);
                }
                entry.last_decision = Some(reason);
            }
            Decision::Typed => {
                let count = entry.in_flight.as_ref().map_or(0, |flight| flight.count);
                crate::logging::idle_wake_typed(pane, count);
                entry.last_decision = None;
            }
            Decision::Submitted => {
                let count = entry.in_flight.as_ref().map_or(0, |flight| flight.count);
                crate::logging::idle_wake_fired(pane, count);
                entry.last_decision = None;
            }
            Decision::Abandoned(reason) => {
                crate::logging::idle_wake_abandoned(pane, reason);
                entry.last_decision = None;
            }
        }
    }

    fn decide_idle_wake(&mut self, pane: &str, wakeable_ids: &[String], now: Instant) -> Decision {
        let config = &self.state.config.msg;
        let settle = Duration::from_millis(config.idle_wake_settle_ms);
        let quiet = Duration::from_millis(config.idle_wake_operator_quiet_ms);
        let fresh = Duration::from_millis(config.idle_wake_fresh_ms);

        {
            let entry = self.idle_wake.panes.entry(pane.to_string()).or_default();
            entry
                .announced
                .retain(|id| wakeable_ids.iter().any(|queued| queued == id));
        }

        let Some((ws_idx, pane_id)) = self.parse_pane_id(pane) else {
            return Decision::Suppressed("pane_gone");
        };

        // A wake already typed: finish it, or wait for it to land.
        if let Some(submit_at) = self
            .idle_wake
            .panes
            .get(pane)
            .and_then(|entry| entry.in_flight.as_ref())
            .and_then(|flight| flight.submit_at)
        {
            if now < submit_at {
                self.idle_wake.note_deadline(submit_at);
                return Decision::Suppressed("submit_pending");
            }
            return self.submit_idle_wake(pane, now);
        }
        if self
            .idle_wake
            .panes
            .get(pane)
            .is_some_and(|entry| entry.in_flight.is_some())
        {
            let still_idle = self
                .terminal_for_pane(ws_idx, pane_id)
                .is_some_and(|terminal| terminal.state == AgentState::Idle);
            if still_idle {
                return Decision::Suppressed("in_flight");
            }
            // It left Idle — the wake (or something else) took. Messages it
            // was told about stay announced; only new ones can wake it again.
            if let Some(entry) = self.idle_wake.panes.get_mut(pane) {
                entry.in_flight = None;
            }
        }

        let unannounced = self
            .idle_wake
            .panes
            .get(pane)
            .is_none_or(|entry| wakeable_ids.iter().any(|id| !entry.announced.contains(id)));
        if !unannounced {
            return Decision::Suppressed("already_announced");
        }

        // #438: under channel push the push is the first knock. Hold off
        // until it has had time to start a turn — if it did, the idle gate
        // below finds the agent working and nothing is typed.
        if config.channel_push {
            let grace = config.channel_push_idle_wake_grace_ms;
            let announced = self.idle_wake.panes.get(pane).map(|entry| &entry.announced);
            let newest = wakeable_ids
                .iter()
                .filter(|id| announced.is_none_or(|announced| !announced.contains(*id)))
                .filter_map(|id| self.mailboxes.queued_message(id))
                .map(|message| message.enqueued_at_ms)
                .max();
            if let Some(enqueued_ms) = newest {
                let waited = super::api::messages::now_ms().saturating_sub(enqueued_ms);
                if waited < grace {
                    self.idle_wake
                        .note_deadline(now + Duration::from_millis(grace - waited));
                    return Decision::Suppressed("channel_push_grace");
                }
            }
        }

        // Every gate from here on is evaluated in the same call that types,
        // with nothing between the last check and the write.
        if let Some(suppression) = self.wake_suppression(pane, super::api::messages::now_ms()) {
            if let Some(until_ms) = suppression.muted_until_ms {
                let wait = until_ms.saturating_sub(super::api::messages::now_ms());
                self.idle_wake
                    .note_deadline(now + Duration::from_millis(wait));
            }
            return Decision::Suppressed(suppression.reason);
        }

        let Some(terminal) = self.terminal_for_pane(ws_idx, pane_id) else {
            return Decision::Suppressed("pane_gone");
        };
        let agent = terminal.effective_known_agent();
        if !has_inbox_tool(agent) {
            return Decision::Suppressed("no_inbox_tool");
        }
        if terminal.restart_in_progress {
            return Decision::Suppressed("restart_pending");
        }
        if let Some(blocker) = terminal.idle_wake_blocker(now, settle, fresh) {
            if blocker == "not_settled" {
                if let Some(settles_at) = terminal.state_settles_at(settle) {
                    self.idle_wake.note_deadline(settles_at);
                }
            }
            return Decision::Suppressed(blocker);
        }

        let Some(runtime) = self.lookup_runtime_sender(ws_idx, pane_id) else {
            return Decision::Suppressed("pane_gone");
        };
        if let Some(last_input) = runtime.last_operator_input_at() {
            let quiet_until = last_input + quiet;
            if now < quiet_until {
                self.idle_wake.note_deadline(quiet_until);
                return Decision::Suppressed("operator_active");
            }
        }
        // Quiet is not the same as empty: a draft typed and left an hour ago
        // is still in the box, and the Enter would submit it together with
        // the sentence. Read off the screen here, at the keystroke, never on
        // the tick.
        match agent.and_then(|agent| {
            crate::detect::agent_prompt_is_empty(agent, &runtime.detection_text())
        }) {
            Some(true) => {}
            Some(false) => return Decision::Suppressed("prompt_not_empty"),
            None => return Decision::Suppressed("no_prompt_box"),
        }

        let count = self.mailboxes.queued_len(pane);
        let text = idle_wake_text(count);
        let attempt = match self.begin_guarded_submit(pane, &text, None, Duration::ZERO, now, true)
        {
            Ok(attempt) => attempt,
            Err(reason) => return Decision::Suppressed(reason),
        };

        // The Enter is its own write, after the pane's reader has come back
        // round — the #362 lesson: text and a carriage return in one read are
        // a paste with a newline in it, not a submitted prompt.
        let submit_at = now + crate::cli::pane::PANE_RUN_SUBMIT_GAP;
        self.idle_wake.note_deadline(submit_at);
        let entry = self.idle_wake.panes.entry(pane.to_string()).or_default();
        entry.announced.extend(wakeable_ids.iter().cloned());
        entry.in_flight = Some(InFlight {
            submit_at: Some(submit_at),
            count,
            attempt: Some(attempt),
        });
        Decision::Typed
    }

    /// Press Enter on a wake typed one gap ago — after checking, once more,
    /// that nothing changed underneath it. A human who typed into the pane
    /// since, or an agent that is no longer fresh-and-idle, gets no Enter:
    /// the sentence is left in the prompt for whoever is there to keep or
    /// delete, which is recoverable. Submitting a prompt a human was editing
    /// is not.
    ///
    /// Flock does NOT try to erase an abandoned sentence. It knows what it
    /// wrote, but not what the agent made of it — a TUI may collapse a paste
    /// into a single chip — so a counted run of backspaces could eat into
    /// whatever sits before it. A finished attempt releases its reservation;
    /// the empty-prompt gate still refuses to type beside a stranded sentence.
    fn submit_idle_wake(&mut self, pane: &str, now: Instant) -> Decision {
        let Some(mut attempt) = self
            .idle_wake
            .panes
            .get_mut(pane)
            .and_then(|entry| entry.in_flight.as_mut())
            .and_then(|flight| flight.attempt.take())
        else {
            return Decision::Abandoned("missing_attempt");
        };
        let enter_due = now >= attempt.due;
        let outcome = if let Some(suppression) =
            self.wake_suppression(pane, super::api::messages::now_ms())
        {
            Some(super::guarded_submit::Outcome::Abandoned(
                suppression.reason,
            ))
        } else {
            self.advance_guarded_submit(pane, &mut attempt, now)
        };
        if let Some(ref outcome) = outcome {
            self.finish_delivery_attempt(&mut attempt, outcome);
        }
        let next = now + Duration::from_millis(50);
        if outcome.is_none() {
            self.idle_wake.note_deadline(next);
        }
        if let Some(flight) = self
            .idle_wake
            .panes
            .get_mut(pane)
            .and_then(|entry| entry.in_flight.as_mut())
        {
            flight.submit_at = outcome.is_none().then_some(next);
            flight.attempt = outcome.is_none().then_some(attempt);
        }
        match outcome {
            None if enter_due => Decision::Submitted,
            None => Decision::Suppressed("confirm_pending"),
            Some(
                super::guarded_submit::Outcome::Accepted
                | super::guarded_submit::Outcome::ObservedAccepted,
            ) => {
                if let Some(entry) = self.idle_wake.panes.get_mut(pane) {
                    entry.in_flight = None;
                }
                Decision::Suppressed("accepted")
            }
            Some(
                super::guarded_submit::Outcome::Abandoned(reason)
                | super::guarded_submit::Outcome::Unconfirmed(reason),
            ) => {
                if let Some(entry) = self.idle_wake.panes.get_mut(pane) {
                    entry.in_flight = None;
                }
                Decision::Abandoned(reason)
            }
        }
    }

    /// `pub(crate)` rather than private because `app::self_compact` walks the
    /// same pane tree to the same terminal, and a second copy of this lookup
    /// would be a second thing to keep correct.
    pub(crate) fn terminal_for_pane(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<&crate::terminal::TerminalState> {
        let terminal_id = self.state.terminal_id_for_pane(ws_idx, pane_id)?;
        self.state.terminals.get(&terminal_id)
    }
}

impl App {
    pub(crate) fn restore_delivery_attempts(&mut self) {
        let mut events = std::collections::BTreeMap::new();
        for (seq, _, event) in self.event_hub.persisted_events_after(0) {
            events.insert(seq, event);
        }
        events.extend(self.event_hub.events_after(0));
        let mut restored = std::collections::BTreeMap::new();
        for event in events.into_values() {
            if let crate::api::schema::EventData::DeliveryAttemptUpdated { attempt } = event.data {
                restored.insert(attempt.attempt_id.clone(), attempt);
            }
        }
        self.delivery_attempt_registry.borrow_mut().clear();
        for attempt in restored.into_values() {
            if attempt.wake {
                self.idle_wake
                    .panes
                    .entry(attempt.pane.clone())
                    .or_default()
                    .announced
                    .extend(attempt.correlation_ids.iter().cloned());
            }
            self.delivery_attempt_registry.borrow_mut().record(attempt);
        }
        for mut attempt in self.delivery_attempts() {
            if attempt.finished_at_ms.is_none() {
                attempt.state = "unconfirmed".into();
                attempt.reason = Some("server_restarted".into());
                attempt.finished_at_ms = Some(super::api::messages::now_ms());
                self.record_delivery_attempt(&attempt);
            }
        }
    }
}

#[cfg(test)]
#[path = "idle_wake_tests.rs"]
mod tests;
