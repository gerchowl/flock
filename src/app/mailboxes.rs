use std::collections::{HashMap, HashSet, VecDeque};

use crate::api::schema::{EventData, EventEnvelope, MsgIntent};

/// Per-pane message queues (#175 M1). The durable event log (ADR-0005) is
/// the source of truth: every admit emits `MessageQueued`, every settle
/// emits `MessageDelivered`, and [`MailboxRegistry::seed_from_events`]
/// reconstructs undelivered queues plus the dedupe set at boot — that is
/// the at-least-once + dedupe-across-restarts contract (§8.4, §8.6).
/// Queues are keyed by the recipient's *public pane id*, the identity that
/// survives restarts; panes are re-resolved at drain time.
///
/// Dedupe is BOUNDED, not eternal: the seen-set holds the newest
/// `MAX_SEEN` correlation ids (and the boot seed only sees what log
/// rotation kept), so a duplicate older than both windows can be accepted
/// again. Evicting a seen id also drops its reply-routing history — replies
/// to sufficiently old messages return `message_not_found`.
#[derive(Default)]
pub(crate) struct MailboxRegistry {
    queues: HashMap<String, VecDeque<PendingMessage>>,
    seen: HashSet<String>,
    seen_order: VecDeque<String>,
    /// Delivered-message metadata for reply routing + round-trip telemetry.
    history: HashMap<String, DeliveredMeta>,
    /// Sender pane → recent send timestamps (ms), for rate limiting.
    rate: HashMap<String, VecDeque<u64>>,
    /// Recipient pane → ms-since-epoch its self-declared mute expires (#316).
    ///
    /// Deliberately NOT seeded from the durable log and not persisted. A mute
    /// is a receiver saying "not for the next few minutes", and the honest
    /// thing to do with one across a restart is forget it — which fails OPEN,
    /// the only direction that cannot strand a message. Queues are the
    /// durable half; this is a live preference about interruptions.
    mutes: HashMap<String, Mute>,
    /// Correlation ids a muted recipient has already answered with a
    /// deferral (ADR-0018 §3). "Exactly one" is enforced here, not by the
    /// caller remembering: re-muting, extending a mute, or a restart
    /// followed by a fresh mute must not tell the same sender twice.
    ///
    /// Unlike `mutes` this IS seeded from the durable log (`MessageDeferred`),
    /// because forgetting it fails the wrong way — a duplicate answer, not a
    /// missing one.
    ///
    /// A mark lives exactly as long as its message is QUEUED — dropped when
    /// it is read or expires, and never by the `seen` window. That window is
    /// the newest `MAX_SEEN` ids server-wide, while a question can wait up to
    /// `UNDELIVERED_TTL_MS`; tying the mark to the window let a busy fleet
    /// evict the mark of a question still sitting in an inbox, and the next
    /// mute answered it again. Queue lifetime bounds the set just as well:
    /// it can never hold more than the queues do.
    deferred: HashSet<String>,
    /// Cross-host deferral hops waiting for a slot, and how many are running.
    /// See `App::pump_deferral_hops`.
    deferral_hops: VecDeque<DeferralHop>,
    deferral_hops_running: usize,
    /// Sender → recent `blocking` send timestamps (ms), for the tier's own
    /// hourly budget (ADR-0018 §1). Separate from `rate` so a sender's
    /// ordinary traffic cannot spend its escalation budget, or vice versa.
    blocking_rate: HashMap<String, VecDeque<u64>>,
    /// Correlation ids of `blocking` messages already escalated to the
    /// operator (ADR-0018 §4) — once per message, however many times the
    /// recipient re-mutes while it waits.
    ///
    /// In memory, like `mutes`, and forgotten by a restart for the same
    /// reason: a restart also forgets the mute, so nothing escalates again
    /// until the recipient mutes afresh — and a message still waiting on a
    /// recipient that has just re-muted IS still the disagreement, so the
    /// operator hearing about it once more after a restart is the honest
    /// outcome, not a duplicate.
    escalated: HashSet<String>,
    /// Correlation ids of waking messages this server RELAYED to another host,
    /// oldest first (ADR-0018 §1's reply rule). A question that left the
    /// machine is in neither the queue nor the delivery history here, so
    /// without this its answer, arriving back, could not be told apart from a
    /// notice. Bounded like `seen`: an answer to a question older than the
    /// newest `MAX_SEEN` no longer wakes, exactly as a reply to a delivered
    /// message that aged out of `history` does not.
    relayed_questions: HashSet<String>,
    /// Insertion order of `relayed_questions`, for eviction.
    relayed_questions_order: VecDeque<String>,
}

/// One cross-host deferral waiting to be sent (ADR-0018 §3).
pub(crate) struct DeferralHop {
    pub peer: crate::config::PeerConfig,
    pub body: String,
    pub relay: crate::events::MsgDeferralRelay,
}

/// A live receiver-side mute: when it lifts, and why, in the muter's words.
#[derive(Debug, Clone)]
struct Mute {
    until_ms: u64,
    reason: Option<String>,
}

/// What the attention surface may know about a pane's waiting `blocking`
/// mail: a count and a sender identity, never a body (#316 pitfall 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlockingMail {
    pub count: usize,
    /// The oldest waiting sender — the one blocked longest.
    pub sender: String,
    /// How many OTHER senders are also waiting on this pane.
    pub other_senders: usize,
}

impl BlockingMail {
    /// The agents-panel label: `✉2 from <sender>`, `+N` when others are
    /// waiting too. A count and an identity — the whole of what #316 pitfall 3
    /// allows a wake-adjacent surface to say.
    pub(crate) fn label(&self) -> String {
        let others = if self.other_senders == 0 {
            String::new()
        } else {
            format!(" +{}", self.other_senders)
        };
        format!("✉{} from {}{others}", self.count, self.sender)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingMessage {
    pub correlation_id: String,
    pub body: String,
    pub from_pane: Option<String>,
    /// Sender's fleet-global identity. Unlike `from_pane` this survives a
    /// restart, a pane move, and the trip across a machine boundary — it is
    /// what makes a cross-host reply addressable.
    pub from_agent: Option<String>,
    /// Host the sender was on when it sent.
    pub from_host: Option<String>,
    pub from_repo: Option<String>,
    pub to_pane: String,
    pub to_repo: Option<String>,
    pub in_reply_to: Option<String>,
    pub enqueued_at_ms: u64,
    pub delivery_attempts: u32,
    /// Whether the sender said it is owed an answer (#280). Carried on the
    /// queued message so it reaches the reader on the envelope rather than
    /// buried in the body.
    pub intent: MsgIntent,
}

#[derive(Debug, Clone)]
pub(crate) struct DeliveredMeta {
    pub from_pane: Option<String>,
    /// Fleet-global sender, so a reply can be routed after the original has
    /// left the queue.
    pub from_agent: Option<String>,
    pub enqueued_at_ms: u64,
    /// Correlation id of the thread root (self for a fresh message).
    pub root: String,
    pub round_trips: u32,
    /// The delivered message's own tier, so a later reply to it knows whether
    /// it answers a question (ADR-0018 §1).
    pub intent: MsgIntent,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EnqueueOutcome {
    Queued,
    /// Correlation id already seen — at-least-once dedupe (§8.6).
    Duplicate,
    MailboxFull,
}

pub(crate) const MAX_QUEUED_PER_PANE: usize = 32;
pub(crate) const RATE_LIMIT_PER_MINUTE: usize = 20;
const MAX_SEEN: usize = 4096;
/// Undelivered messages older than this are dropped as undeliverable
/// (hibernated-forever panes must not grow the queue without bound).
pub(crate) const UNDELIVERED_TTL_MS: u64 = 24 * 60 * 60 * 1000;
/// The window `[msg] blocking_per_hour` counts over.
const BLOCKING_WINDOW_MS: u64 = 60 * 60 * 1000;
/// Ceiling on a receiver-side mute (#316). An unbounded mute is a black hole
/// wearing a politeness hat; a bounded one has to be renewed, which is what
/// makes it impossible to set and forget. Half an hour is long enough to
/// finish a refactor and short enough that the worst case is bounded latency.
pub(crate) const MAX_MUTE_SECONDS: u64 = 30 * 60;

impl MailboxRegistry {
    /// Rebuild queues, dedupe set, and reply history from the durable event
    /// stream: a `MessageQueued` with no matching `MessageDelivered` is
    /// still pending.
    pub(crate) fn seed_from_events<'a>(&mut self, events: impl Iterator<Item = &'a EventEnvelope>) {
        let mut queued: HashMap<String, PendingMessage> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        for envelope in events {
            match &envelope.data {
                EventData::MessageQueued {
                    correlation_id,
                    from_pane,
                    from_repo,
                    to_pane,
                    to_repo,
                    in_reply_to,
                    enqueued_at_ms,
                    body,
                    from_agent,
                    from_host,
                    intent,
                    ..
                } => {
                    self.mark_seen(correlation_id.clone());
                    queued.insert(
                        correlation_id.clone(),
                        PendingMessage {
                            correlation_id: correlation_id.clone(),
                            body: body.clone(),
                            from_pane: from_pane.clone(),
                            from_agent: from_agent.clone(),
                            from_host: from_host.clone(),
                            from_repo: from_repo.clone(),
                            to_pane: to_pane.clone(),
                            to_repo: to_repo.clone(),
                            in_reply_to: in_reply_to.clone(),
                            enqueued_at_ms: *enqueued_at_ms,
                            delivery_attempts: 0,
                            intent: *intent,
                        },
                    );
                    order.push(correlation_id.clone());
                }
                EventData::MessageDelivered { correlation_id, .. } => {
                    self.deferred.remove(correlation_id);
                    if let Some(message) = queued.remove(correlation_id) {
                        let root = message
                            .in_reply_to
                            .as_ref()
                            .and_then(|parent| self.history.get(parent))
                            .map(|meta| meta.root.clone())
                            .unwrap_or_else(|| correlation_id.clone());
                        self.history.insert(
                            correlation_id.clone(),
                            DeliveredMeta {
                                from_pane: message.from_pane.clone(),
                                from_agent: message.from_agent.clone(),
                                enqueued_at_ms: message.enqueued_at_ms,
                                root,
                                round_trips: 0,
                                intent: message.intent,
                            },
                        );
                    }
                }
                // A held deferral (#576) is recorded as a `MessageReplied`
                // so its body survives, but a mute's automatic answer is not a
                // round trip: live, it never bumps the count, so replay must
                // not either.
                EventData::MessageReplied {
                    correlation_id,
                    reply_correlation_id,
                    ..
                } if !is_deferral(reply_correlation_id) => {
                    if let Some(meta) = self.history.get_mut(correlation_id) {
                        meta.round_trips += 1;
                    }
                }
                // Only while the message is still pending: a mark outlives
                // nothing but its queue entry, and the `MessageDelivered`
                // arm above drops it when that entry goes.
                EventData::MessageDeferred { correlation_id, .. }
                    if queued.contains_key(correlation_id) =>
                {
                    self.deferred.insert(correlation_id.clone());
                }
                EventData::MessageRelayed {
                    correlation_id,
                    intent,
                    ..
                } if intent.wakes() => self.record_relayed_question(correlation_id.clone()),
                _ => {}
            }
        }
        for correlation_id in order {
            if let Some(message) = queued.remove(&correlation_id) {
                self.queues
                    .entry(message.to_pane.clone())
                    .or_default()
                    .push_back(message);
            }
        }
    }

    fn mark_seen(&mut self, correlation_id: String) {
        if self.seen.insert(correlation_id.clone()) {
            self.seen_order.push_back(correlation_id);
            while self.seen_order.len() > MAX_SEEN {
                if let Some(oldest) = self.seen_order.pop_front() {
                    self.seen.remove(&oldest);
                    self.history.remove(&oldest);
                }
            }
        }
    }

    /// Per-sender token bucket (P: mechanical gates). Returns the wait in
    /// milliseconds when the sender is over budget.
    pub(crate) fn admit_rate(&mut self, sender_key: &str, now_ms: u64) -> Result<(), u64> {
        let window = self.rate.entry(sender_key.to_string()).or_default();
        while window
            .front()
            .is_some_and(|sent| now_ms.saturating_sub(*sent) > 60_000)
        {
            window.pop_front();
        }
        if window.len() >= RATE_LIMIT_PER_MINUTE {
            let retry_after = window
                .front()
                .map(|oldest| 60_000u64.saturating_sub(now_ms.saturating_sub(*oldest)))
                .unwrap_or(60_000);
            return Err(retry_after.max(1));
        }
        window.push_back(now_ms);
        Ok(())
    }

    /// The `blocking` tier's own budget (ADR-0018 §1): `Err` carries the wait
    /// in ms, like [`Self::admit_rate`]. Peek-only — [`Self::record_blocking`]
    /// spends the slot once every other gate has admitted the message, so a
    /// send refused for another reason does not cost the sender escalation.
    pub(crate) fn blocking_retry_after(
        &mut self,
        sender_key: &str,
        now_ms: u64,
        per_hour: usize,
    ) -> Result<(), u64> {
        // Every key is a sender that spent budget within the window, and no
        // other: a sender whose window has lapsed leaves no entry behind, so
        // the map is bounded by who was actually active this hour.
        self.blocking_rate.retain(|_, window| {
            while window
                .front()
                .is_some_and(|sent| now_ms.saturating_sub(*sent) > BLOCKING_WINDOW_MS)
            {
                window.pop_front();
            }
            !window.is_empty()
        });
        let Some(window) = self.blocking_rate.get(sender_key) else {
            return Ok(());
        };
        if window.len() >= per_hour {
            let retry_after = window
                .front()
                .map(|oldest| BLOCKING_WINDOW_MS.saturating_sub(now_ms.saturating_sub(*oldest)))
                .unwrap_or(BLOCKING_WINDOW_MS);
            return Err(retry_after.max(1));
        }
        Ok(())
    }

    pub(crate) fn record_blocking(&mut self, sender_key: &str, now_ms: u64) {
        self.blocking_rate
            .entry(sender_key.to_string())
            .or_default()
            .push_back(now_ms);
    }

    pub(crate) fn enqueue(&mut self, message: PendingMessage) -> EnqueueOutcome {
        if self.seen.contains(&message.correlation_id) {
            return EnqueueOutcome::Duplicate;
        }
        if self
            .queues
            .get(&message.to_pane)
            .is_some_and(|queue| queue.len() >= MAX_QUEUED_PER_PANE)
        {
            return EnqueueOutcome::MailboxFull;
        }
        self.mark_seen(message.correlation_id.clone());
        self.queues
            .entry(message.to_pane.clone())
            .or_default()
            .push_back(message);
        EnqueueOutcome::Queued
    }

    /// Take the next message for a pane. The recipient's `msg.read` drains
    /// this until empty.
    ///
    /// There is no re-queue counterpart any more: under pane injection a
    /// delivery could fail halfway (pane mid-turn, no agent label) and the
    /// message had to go back on the front. A pull read cannot half-fail —
    /// the recipient either took the message or never asked (ADR-0008).
    pub(crate) fn pop_next(&mut self, pane_id: &str) -> Option<PendingMessage> {
        let message = self.queues.get_mut(pane_id)?.pop_front()?;
        self.deferred.remove(&message.correlation_id);
        // A message that left the queue can never be escalated again, so its
        // marker is dead weight.
        self.escalated.remove(&message.correlation_id);
        Some(message)
    }

    /// Take one specific queued message out of `pane_id`'s inbox (#438).
    ///
    /// For the channel push, where an agent can answer a message it was shown
    /// but never pulled: the reply is the only acknowledgement a push gets,
    /// so it settles the message the way a read would. Scoped to the
    /// recipient's own queue — a pane cannot settle someone else's mail by
    /// knowing its correlation id.
    pub(crate) fn take_queued(
        &mut self,
        pane_id: &str,
        correlation_id: &str,
    ) -> Option<PendingMessage> {
        let queue = self.queues.get_mut(pane_id)?;
        let index = queue
            .iter()
            .position(|message| message.correlation_id == correlation_id)?;
        let message = queue.remove(index)?;
        self.deferred.remove(&message.correlation_id);
        self.escalated.remove(&message.correlation_id);
        Some(message)
    }

    pub(crate) fn record_delivered(&mut self, message: &PendingMessage) {
        let root = message
            .in_reply_to
            .as_ref()
            .and_then(|parent| self.history.get(parent))
            .map(|meta| meta.root.clone())
            .unwrap_or_else(|| message.correlation_id.clone());
        self.history.insert(
            message.correlation_id.clone(),
            DeliveredMeta {
                from_agent: message.from_agent.clone(),
                from_pane: message.from_pane.clone(),
                enqueued_at_ms: message.enqueued_at_ms,
                root,
                round_trips: 0,
                intent: message.intent,
            },
        );
    }

    pub(crate) fn reply_meta(&self, correlation_id: &str) -> Option<&DeliveredMeta> {
        self.history.get(correlation_id)
    }

    /// Metadata for a message that is still queued (reply-before-delivery).
    pub(crate) fn queued_message(&self, correlation_id: &str) -> Option<&PendingMessage> {
        self.queues
            .values()
            .flat_map(|queue| queue.iter())
            .find(|message| message.correlation_id == correlation_id)
    }

    pub(crate) fn bump_round_trips(&mut self, root: &str) -> u32 {
        match self.history.get_mut(root) {
            Some(meta) => {
                meta.round_trips += 1;
                meta.round_trips
            }
            None => 1,
        }
    }

    /// Drop messages past the undeliverable TTL; returns them so the caller
    /// can emit terminal `MessageDelivered { delivered: false }` events.
    /// Backdate every queued message so a TTL sweep can be exercised without
    /// sleeping.
    #[cfg(test)]
    pub(crate) fn test_age_all(&mut self, by_ms: u64) {
        for queue in self.queues.values_mut() {
            for message in queue.iter_mut() {
                message.enqueued_at_ms = message.enqueued_at_ms.saturating_sub(by_ms);
            }
        }
    }

    pub(crate) fn expire(&mut self, now_ms: u64) -> Vec<PendingMessage> {
        let mut expired = Vec::new();
        for queue in self.queues.values_mut() {
            while queue.front().is_some_and(|message| {
                now_ms.saturating_sub(message.enqueued_at_ms) > UNDELIVERED_TTL_MS
            }) {
                if let Some(message) = queue.pop_front() {
                    self.deferred.remove(&message.correlation_id);
                    self.escalated.remove(&message.correlation_id);
                    expired.push(message);
                }
            }
        }
        self.queues.retain(|_, queue| !queue.is_empty());
        expired
    }

    /// Set (or, with `seconds == 0`, clear) a pane's wake mute. Returns the
    /// expiry in ms since epoch, or 0 when cleared.
    ///
    /// `seconds` is CLAMPED to [`MAX_MUTE_SECONDS`] rather than refused. A
    /// receiver asking for two hours has still said "not now"; answering with
    /// an error would leave it un-muted, which is the opposite of what it
    /// asked for and the kind of refusal an agent retries in a loop.
    ///
    /// `reason` replaces the previous one rather than accumulating: a renewed
    /// mute says why NOW.
    pub(crate) fn set_mute(
        &mut self,
        pane: &str,
        seconds: u64,
        now_ms: u64,
        reason: Option<String>,
    ) -> u64 {
        if seconds == 0 {
            self.mutes.remove(pane);
            return 0;
        }
        let until = now_ms.saturating_add(seconds.min(MAX_MUTE_SECONDS).saturating_mul(1000));
        self.mutes.insert(
            pane.to_string(),
            Mute {
                until_ms: until,
                reason,
            },
        );
        until
    }

    /// The live mute's reason, if the pane is muted and gave one.
    pub(crate) fn mute_reason(&mut self, pane: &str, now_ms: u64) -> Option<String> {
        self.muted_until(pane, now_ms)?;
        self.mutes.get(pane)?.reason.clone()
    }

    /// Messages queued for `pane` that a mute owes an answer (ADR-0018 §3):
    /// every waking message not already answered. An `fyi` is never owed
    /// one — nothing was asked, so there is nothing to defer.
    pub(crate) fn owed_deferrals(&self, pane: &str) -> Vec<PendingMessage> {
        self.queues
            .get(pane)
            .into_iter()
            .flat_map(|queue| queue.iter())
            .filter(|message| self.owes_deferral(message))
            .cloned()
            .collect()
    }

    /// Whether `message` still needs a deferral. The one predicate both
    /// triggers — a mute being set, and a message arriving into one — ask,
    /// so they cannot disagree about what is owed.
    ///
    /// Keyed on the message's OWN tier ([`MsgIntent::wakes`]), deliberately
    /// not on [`Self::message_wakes`]: an `fyi` answer to the muted agent's
    /// own question wakes it, but its sender asked nothing, so nothing is
    /// owed. A deferral is `fyi` by construction and so is never owed one.
    pub(crate) fn owes_deferral(&self, message: &PendingMessage) -> bool {
        message.intent.wakes() && !self.deferred.contains(&message.correlation_id)
    }

    /// Record that `correlation_id` has been answered. Returns false when it
    /// already was — the caller must then send nothing.
    pub(crate) fn mark_deferred(&mut self, correlation_id: &str) -> bool {
        self.deferred.insert(correlation_id.to_string())
    }

    pub(crate) fn push_deferral_hop(&mut self, hop: DeferralHop) {
        self.deferral_hops.push_back(hop);
    }

    /// Hand out as many queued hops as fit under `cap` running at once, and
    /// count them as running.
    pub(crate) fn start_deferral_hops(&mut self, cap: usize) -> Vec<DeferralHop> {
        let free = cap.saturating_sub(self.deferral_hops_running);
        let take = free.min(self.deferral_hops.len());
        self.deferral_hops_running += take;
        self.deferral_hops.drain(..take).collect()
    }

    pub(crate) fn finish_deferral_hop(&mut self) {
        self.deferral_hops_running = self.deferral_hops_running.saturating_sub(1);
    }

    /// Withdraw a claim whose answer never left: a cross-host deferral the
    /// peer could not be reached for. Unclaimed, the next mute retries it.
    pub(crate) fn unmark_deferred(&mut self, correlation_id: &str) {
        self.deferred.remove(correlation_id);
    }

    /// The pane's live mute expiry, or `None` when it is not muted. Expired
    /// entries are dropped as they are read, so a pane that muted once does
    /// not hold an entry for the life of the server.
    pub(crate) fn muted_until(&mut self, pane: &str, now_ms: u64) -> Option<u64> {
        match self.mutes.get(pane).map(|mute| mute.until_ms) {
            Some(until) if until > now_ms => Some(until),
            Some(_) => {
                self.mutes.remove(pane);
                None
            }
            None => None,
        }
    }

    /// How many messages are queued for one pane — the number the wake path
    /// is allowed to know. No bodies, no previews; see [`MsgWakeParams`].
    ///
    /// [`MsgWakeParams`]: crate::api::schema::MsgWakeParams
    pub(crate) fn queued_len(&self, pane: &str) -> usize {
        self.queues.get(pane).map_or(0, VecDeque::len)
    }

    /// The count a wake may name (ADR-0018 §1): zero unless at least one
    /// queued message [`wakes`](MsgIntent::wakes), and otherwise EVERY queued
    /// message — the `fyi` ones included, so the read the nudge prompts takes
    /// them too. An inbox of nothing but `fyi` never costs a turn.
    pub(crate) fn wake_count(&self, pane: &str) -> usize {
        let Some(queue) = self.queues.get(pane) else {
            return 0;
        };
        if queue.iter().any(|message| self.message_wakes(message)) {
            queue.len()
        } else {
            0
        }
    }

    /// Whether a queued message may start a wake (ADR-0018 §1): its own tier
    /// wakes, or it ANSWERS a waking message. A reply's own stamp says whether
    /// it asks something back and defaults to `fyi`, so without the second
    /// half the answer to an agent's own question would never nudge it.
    /// A deferral is excluded by name: it replies to a waking message by
    /// construction, but carries "later", not an answer.
    pub(crate) fn message_wakes(&self, message: &PendingMessage) -> bool {
        message.intent.wakes()
            || (!is_deferral(&message.correlation_id)
                && message
                    .in_reply_to
                    .as_deref()
                    .is_some_and(|parent| self.asked_a_question(parent)))
    }

    /// Whether `correlation_id` names a waking message this server has seen:
    /// delivered here, still queued here, or relayed from here to another host.
    /// Bounded memory, so bounded reach: a question that has aged out of both
    /// `history` and `relayed_questions` is forgotten, and an answer to it
    /// arriving later is read at its own stamp.
    fn asked_a_question(&self, correlation_id: &str) -> bool {
        self.history
            .get(correlation_id)
            .map(|meta| meta.intent)
            .or_else(|| self.queued_message(correlation_id).map(|m| m.intent))
            .is_some_and(MsgIntent::wakes)
            || self.relayed_questions.contains(correlation_id)
    }

    /// Remember a waking message that left for another host, so its answer
    /// wakes the sender when it comes back.
    pub(crate) fn record_relayed_question(&mut self, correlation_id: String) {
        if !self.relayed_questions.insert(correlation_id.clone()) {
            return;
        }
        self.relayed_questions_order.push_back(correlation_id);
        while self.relayed_questions_order.len() > MAX_SEEN {
            if let Some(oldest) = self.relayed_questions_order.pop_front() {
                self.relayed_questions.remove(&oldest);
            }
        }
    }

    /// The `blocking` mail waiting on each pane, for the attention surface.
    pub(crate) fn blocking_mail(&self) -> HashMap<String, BlockingMail> {
        self.queues
            .iter()
            .filter_map(|(pane, queue)| {
                let mut waiting = queue
                    .iter()
                    .filter(|message| message.intent == MsgIntent::Blocking);
                let oldest = waiting.next()?;
                let sender = sender_identity(oldest);
                let mut count = 1;
                let mut others: HashSet<String> = HashSet::new();
                for message in waiting {
                    count += 1;
                    let other = sender_identity(message);
                    if other != sender {
                        others.insert(other);
                    }
                }
                Some((
                    pane.clone(),
                    BlockingMail {
                        count,
                        sender,
                        other_senders: others.len(),
                    },
                ))
            })
            .collect()
    }

    /// Queued `blocking` messages for `pane` not yet escalated, marked
    /// escalated as they are returned (ADR-0018 §4: once per message). Grouped
    /// by sender, as `(sender, count)`, oldest sender first.
    pub(crate) fn take_unescalated_blocking(&mut self, pane: &str) -> Vec<(String, usize)> {
        let Some(queue) = self.queues.get(pane) else {
            return Vec::new();
        };
        let mut by_sender: Vec<(String, usize)> = Vec::new();
        for message in queue {
            if message.intent != MsgIntent::Blocking
                || !self.escalated.insert(message.correlation_id.clone())
            {
                continue;
            }
            let sender = sender_identity(message);
            match by_sender.iter_mut().find(|(known, _)| *known == sender) {
                Some((_, count)) => *count += 1,
                None => by_sender.push((sender, 1)),
            }
        }
        by_sender
    }

    /// Panes holding at least one message worth waking an agent for
    /// (ADR-0018 §1). The idle wake's per-tick question, answered from memory
    /// and without allocating.
    pub(crate) fn wakeable_panes(&self) -> impl Iterator<Item = &str> {
        self.queues
            .iter()
            .filter(|(_, queue)| queue.iter().any(|message| self.message_wakes(message)))
            .map(|(pane, _)| pane.as_str())
    }

    /// Correlation ids of one pane's queued messages that may wake it.
    pub(crate) fn wakeable_ids(&self, pane: &str) -> Vec<String> {
        self.queues.get(pane).map_or_else(Vec::new, |queue| {
            queue
                .iter()
                .filter(|message| self.message_wakes(message))
                .map(|message| message.correlation_id.clone())
                .collect()
        })
    }

    pub(crate) fn queued_infos(
        &self,
        pane: Option<&str>,
    ) -> Vec<crate::api::schema::QueuedMessageInfo> {
        let mut messages: Vec<_> = self
            .queues
            .iter()
            .filter(|(to_pane, _)| pane.is_none_or(|filter| filter == to_pane.as_str()))
            .flat_map(|(_, queue)| queue.iter())
            .map(|message| crate::api::schema::QueuedMessageInfo {
                correlation_id: message.correlation_id.clone(),
                to_pane: message.to_pane.clone(),
                from_pane: message.from_pane.clone(),
                in_reply_to: message.in_reply_to.clone(),
                enqueued_at_ms: message.enqueued_at_ms,
                delivery_attempts: message.delivery_attempts,
                intent: message.intent,
                preview: message.body.chars().take(120).collect(),
            })
            .collect();
        messages.sort_by_key(|message| message.enqueued_at_ms);
        messages
    }
}

/// The name an operator surface may show for a message's sender: a
/// well-formed agent id, else the server-minted pane, else `unknown@<host>`
/// for a validly named host, else an honest unknown. Never caller text that
/// failed validation — ingress refuses it, and a message restored from an
/// older log is checked again here rather than trusted (ADR-0018 §1).
fn sender_identity(message: &PendingMessage) -> String {
    message
        .from_agent
        .clone()
        .filter(|agent| crate::terminal::AgentId::is_well_formed(agent))
        .or_else(|| message.from_pane.clone())
        .or_else(|| {
            message
                .from_host
                .as_deref()
                .filter(|host| is_host_name(host))
                .map(|host| format!("unknown@{host}"))
        })
        .unwrap_or_else(|| "unknown sender".to_string())
}

/// Suffix that marks a mute's automatic deferral reply (ADR-0018 §3): the
/// deferral's correlation id is the deferred message's id plus this, so it is
/// stable across a relay and a restart and [`is_deferral`] can recognise one
/// on whichever host it lands.
const DEFERRAL_SUFFIX: &str = ":deferred";

/// The correlation id of the automatic deferral answering `correlation_id`.
/// The only place one is minted, so it always satisfies [`is_deferral`].
pub(crate) fn deferral_id(correlation_id: &str) -> String {
    format!("{correlation_id}{DEFERRAL_SUFFIX}")
}

/// Whether a correlation id names an automatic deferral.
pub(crate) fn is_deferral(correlation_id: &str) -> bool {
    correlation_id.ends_with(DEFERRAL_SUFFIX)
}

/// Longest host name [`is_host_name`] accepts.
const MAX_HOST_NAME_LEN: usize = 64;

/// Whether `raw` is shaped like a host name: ASCII alphanumerics, `.`, `-`
/// and `_`, bounded. A relayed `from_host` is asserted by the caller and ends
/// up in labels and notifications, so it is held to the same rule as an id.
pub(crate) fn is_host_name(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= MAX_HOST_NAME_LEN
        && raw
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::EventKind;

    fn message(correlation: &str, to: &str) -> PendingMessage {
        PendingMessage {
            from_agent: None,
            from_host: None,
            correlation_id: correlation.into(),
            body: "hello".into(),
            from_pane: Some("w1:p1".into()),
            from_repo: Some("flock".into()),
            to_pane: to.into(),
            to_repo: Some("flock".into()),
            in_reply_to: None,
            enqueued_at_ms: 1,
            delivery_attempts: 0,
            intent: MsgIntent::Fyi,
        }
    }

    fn queued_event(message: &PendingMessage) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::MessageQueued,
            data: EventData::MessageQueued {
                from_agent: None,
                from_host: None,
                correlation_id: message.correlation_id.clone(),
                from_pane: message.from_pane.clone(),
                from_repo: message.from_repo.clone(),
                to_pane: message.to_pane.clone(),
                to_repo: message.to_repo.clone(),
                cross_repo: false,
                in_reply_to: message.in_reply_to.clone(),
                enqueued_at_ms: message.enqueued_at_ms,
                intent: message.intent,
                body: message.body.clone(),
            },
        }
    }

    fn delivered_event(correlation: &str) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::MessageDelivered,
            data: EventData::MessageDelivered {
                correlation_id: correlation.into(),
                delivered: true,
                outcome: "delivered".into(),
                delivery_attempts: 1,
                latency_ms: 5,
            },
        }
    }

    #[test]
    fn duplicate_correlation_ids_are_deduped_even_across_seed() {
        // §8.6: for any interleaving of duplicate deliveries, the receiver
        // observes each correlation id at most once.
        let mut registry = MailboxRegistry::default();
        assert_eq!(
            registry.enqueue(message("c1", "w1:p2")),
            EnqueueOutcome::Queued
        );
        assert_eq!(
            registry.enqueue(message("c1", "w1:p2")),
            EnqueueOutcome::Duplicate
        );

        // Restart: seed from the durable events. The id stays seen even
        // though the message was already delivered.
        let mut restarted = MailboxRegistry::default();
        let events = [queued_event(&message("c1", "w1:p2")), delivered_event("c1")];
        restarted.seed_from_events(events.iter());
        assert_eq!(
            restarted.enqueue(message("c1", "w1:p2")),
            EnqueueOutcome::Duplicate,
            "dedupe must survive a restart"
        );
        assert!(restarted.pop_next("w1:p2").is_none(), "nothing re-queued");
    }

    #[test]
    fn undelivered_messages_survive_a_seed_in_order() {
        // §8.4: kill mid-delivery ⇒ on restart the message is still queued.
        let mut registry = MailboxRegistry::default();
        let first = message("c1", "w1:p2");
        let mut second = message("c2", "w1:p2");
        second.enqueued_at_ms = 2;
        let events = [
            queued_event(&first),
            queued_event(&second),
            delivered_event("c1"),
        ];
        registry.seed_from_events(events.iter());
        let next = registry.pop_next("w1:p2").expect("undelivered survives");
        assert_eq!(next.correlation_id, "c2");
        assert!(registry.pop_next("w1:p2").is_none());
        assert!(
            registry.reply_meta("c1").is_some(),
            "delivered message keeps reply routing metadata"
        );
    }

    #[test]
    fn rate_limit_admits_twenty_per_minute_then_refuses_with_retry() {
        let mut registry = MailboxRegistry::default();
        for send in 0..RATE_LIMIT_PER_MINUTE {
            assert!(
                registry.admit_rate("w1:p1", 1_000 + send as u64).is_ok(),
                "send {send} within budget"
            );
        }
        let retry = registry
            .admit_rate("w1:p1", 2_000)
            .expect_err("21st send refused");
        assert!(retry > 0 && retry <= 60_000);
        // Window slides: a minute later the budget refills.
        assert!(registry.admit_rate("w1:p1", 62_001).is_ok());
    }

    #[test]
    fn mailbox_depth_is_capped() {
        let mut registry = MailboxRegistry::default();
        for index in 0..MAX_QUEUED_PER_PANE {
            assert_eq!(
                registry.enqueue(message(&format!("c{index}"), "w1:p2")),
                EnqueueOutcome::Queued
            );
        }
        assert_eq!(
            registry.enqueue(message("c-overflow", "w1:p2")),
            EnqueueOutcome::MailboxFull
        );
    }

    /// #316: a mute is bounded by construction. Asking for longer is
    /// clamped, not refused — a receiver that said "not now" must not end up
    /// un-muted because it asked for too much.
    #[test]
    fn a_mute_is_clamped_to_the_cap_and_zero_clears_it() {
        let mut registry = MailboxRegistry::default();
        let now = 1_000_000;

        let until = registry.set_mute("pane-1", MAX_MUTE_SECONDS * 4, now, None);
        assert_eq!(
            until,
            now + MAX_MUTE_SECONDS * 1000,
            "an over-long mute is clamped to the cap",
        );
        assert_eq!(registry.muted_until("pane-1", now), Some(until));

        assert_eq!(registry.set_mute("pane-1", 0, now, None), 0, "zero clears");
        assert_eq!(registry.muted_until("pane-1", now), None);
    }

    /// An expired mute is not just ignored, it is dropped — otherwise a pane
    /// that muted itself once holds an entry for the life of the server.
    #[test]
    fn an_expired_mute_is_swept_as_it_is_read() {
        let mut registry = MailboxRegistry::default();
        let now = 1_000_000;
        let until = registry.set_mute("pane-1", 60, now, None);

        assert_eq!(registry.muted_until("pane-1", until - 1), Some(until));
        assert_eq!(
            registry.muted_until("pane-1", until),
            None,
            "the mute lifts at its expiry, not after it",
        );
        assert!(
            registry.mutes.is_empty(),
            "reading an expired mute drops it"
        );
    }

    /// The wake path is allowed to know a number and nothing else.
    #[test]
    fn queued_len_counts_only_the_named_pane() {
        let mut registry = MailboxRegistry::default();
        registry.enqueue(message("c-1", "pane-1"));
        registry.enqueue(message("c-2", "pane-1"));
        registry.enqueue(message("c-3", "pane-2"));

        assert_eq!(registry.queued_len("pane-1"), 2);
        assert_eq!(registry.queued_len("pane-2"), 1);
        assert_eq!(registry.queued_len("pane-3"), 0);
    }

    fn question(correlation: &str, to: &str) -> PendingMessage {
        PendingMessage {
            intent: MsgIntent::NeedsReply,
            ..message(correlation, to)
        }
    }

    /// Review of #411: the dedupe window is the newest `MAX_SEEN` ids
    /// server-wide, and a question can wait far longer than it takes a busy
    /// fleet to push its id out. Its deferral mark must survive that — it
    /// belongs to the queue entry, not to the window.
    #[test]
    fn a_deferral_mark_outlives_the_dedupe_window_while_its_message_waits() {
        let mut registry = MailboxRegistry::default();
        let waiting = question("c-waiting", "pane-muted");
        registry.enqueue(waiting.clone());
        assert!(registry.mark_deferred("c-waiting"));

        // Push c-waiting out of the seen window, spreading the traffic over
        // enough panes that no mailbox fills.
        for index in 0..=MAX_SEEN {
            let pane = format!("pane-{}", index / MAX_QUEUED_PER_PANE);
            assert_eq!(
                registry.enqueue(message(&format!("c-noise-{index}"), &pane)),
                EnqueueOutcome::Queued
            );
        }
        assert!(
            !registry.seen.contains("c-waiting"),
            "precondition: the id has left the dedupe window"
        );
        assert_eq!(registry.queued_len("pane-muted"), 1, "but is still queued");

        assert!(
            !registry.owes_deferral(&waiting),
            "a question still in an inbox keeps its mark, or a re-mute answers it twice"
        );
        assert!(registry.owed_deferrals("pane-muted").is_empty());

        // Once read, the entry — and with it the mark — is gone.
        registry.pop_next("pane-muted").expect("still queued");
        assert!(!registry.deferred.contains("c-waiting"));
    }

    /// The seed keeps a mark exactly while its message is still pending.
    #[test]
    fn a_seeded_deferral_mark_follows_its_message_out_of_the_queue() {
        let deferred_event = |correlation: &str| EventEnvelope {
            event: EventKind::MessageDeferred,
            data: EventData::MessageDeferred {
                correlation_id: correlation.into(),
                deferral_correlation_id: format!("{correlation}:deferred"),
                pane: "w1:p2".into(),
                muted_until_ms: 10,
                reason: None,
                route: None,
                deferred_at_ms: 2,
            },
        };
        let waiting = question("c-waiting", "w1:p2");
        let read = question("c-read", "w1:p2");
        let events = [
            queued_event(&waiting),
            queued_event(&read),
            deferred_event("c-waiting"),
            deferred_event("c-read"),
            delivered_event("c-read"),
        ];
        let mut registry = MailboxRegistry::default();
        registry.seed_from_events(events.iter());

        assert!(
            !registry.owes_deferral(&waiting),
            "still queued, still answered"
        );
        assert!(
            !registry.deferred.contains("c-read"),
            "a read message holds no mark"
        );
    }

    /// Review of #411: one mute can owe a whole inbox of cross-host
    /// deferrals. They go out at most `cap` at a time, in order, and a
    /// finished hop frees exactly one slot.
    #[test]
    fn deferral_hops_are_capped_and_start_in_order() {
        let hop = |correlation: &str| DeferralHop {
            peer: crate::config::PeerConfig::default(),
            body: String::new(),
            relay: crate::events::MsgDeferralRelay {
                correlation_id: correlation.into(),
                deferral_correlation_id: format!("{correlation}:deferred"),
                pane: "w1:p2".into(),
                muted_until_ms: 0,
                reason: None,
                from_agent: "agent_a".into(),
                to_agent: "agent_b".into(),
                to_host: "far".into(),
                route: "far".into(),
                result: Ok(()),
            },
        };
        let ids = |hops: Vec<DeferralHop>| -> Vec<String> {
            hops.into_iter()
                .map(|hop| hop.relay.correlation_id)
                .collect()
        };
        let mut registry = MailboxRegistry::default();
        for index in 0..5 {
            registry.push_deferral_hop(hop(&format!("c-{index}")));
        }

        assert_eq!(ids(registry.start_deferral_hops(2)), ["c-0", "c-1"]);
        assert!(
            registry.start_deferral_hops(2).is_empty(),
            "no slot until one finishes"
        );
        registry.finish_deferral_hop();
        assert_eq!(ids(registry.start_deferral_hops(2)), ["c-2"]);
        registry.finish_deferral_hop();
        registry.finish_deferral_hop();
        assert_eq!(ids(registry.start_deferral_hops(2)), ["c-3", "c-4"]);
        assert!(registry.start_deferral_hops(2).is_empty(), "queue drained");
    }

    #[test]
    fn expire_drops_only_past_ttl_and_reports_them() {
        let mut registry = MailboxRegistry::default();
        let mut old = message("c-old", "w1:p2");
        old.enqueued_at_ms = 0;
        let mut fresh = message("c-fresh", "w1:p2");
        fresh.enqueued_at_ms = UNDELIVERED_TTL_MS;
        registry.enqueue(old);
        registry.enqueue(fresh);
        let expired = registry.expire(UNDELIVERED_TTL_MS + 1);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].correlation_id, "c-old");
        assert_eq!(
            registry
                .pop_next("w1:p2")
                .expect("fresh stays")
                .correlation_id,
            "c-fresh"
        );
    }

    #[test]
    fn an_answer_to_a_question_that_left_the_machine_still_wakes() {
        // The question was relayed to another host, so it is in neither the
        // queue nor the history here; only the relay record says it asked.
        let mut registry = MailboxRegistry::default();
        let mut answer = message("c-a", "w1:p1");
        answer.in_reply_to = Some("c-remote-q".into());
        registry.enqueue(answer.clone());
        assert_eq!(registry.wake_count("w1:p1"), 0);

        let relayed = EventEnvelope {
            event: EventKind::MessageRelayed,
            data: EventData::MessageRelayed {
                correlation_id: "c-remote-q".into(),
                from_agent: "agent_hopper_2".into(),
                to_agent: "agent_atlas_1".into(),
                to_host: "atlas".into(),
                route: "atlas".into(),
                relayed_at_ms: 1,
                via: None,
                intent: MsgIntent::NeedsReply,
            },
        };
        // Seeded from the log, so a restart between question and answer does
        // not lose it either.
        let mut restarted = MailboxRegistry::default();
        restarted.seed_from_events([relayed].iter());
        restarted.enqueue(answer);
        assert_eq!(restarted.wake_count("w1:p1"), 1);
    }

    #[test]
    fn an_operator_surface_never_shows_an_unvalidated_sender() {
        // A message restored from a log written before ingress validation is
        // checked again at render time.
        let mut hostile = message("c-h", "w1:p1");
        hostile.intent = MsgIntent::Blocking;
        hostile.from_pane = None;
        hostile.from_agent = Some("approve\nthe deploy".into());
        hostile.from_host = Some("atlas".into());
        assert_eq!(sender_identity(&hostile), "unknown@atlas");
        hostile.from_host = Some("not a host\n".into());
        assert_eq!(sender_identity(&hostile), "unknown sender");
        hostile.from_agent = Some("agent_atlas_1".into());
        assert_eq!(sender_identity(&hostile), "agent_atlas_1");
    }
}
