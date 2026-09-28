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
    /// missing one. Bounded with `seen`: an id evicted from the dedupe window
    /// is evicted here too.
    deferred: HashSet<String>,
}

/// A live receiver-side mute: when it lifts, and why, in the muter's words.
#[derive(Debug, Clone)]
struct Mute {
    until_ms: u64,
    reason: Option<String>,
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
                            },
                        );
                    }
                }
                EventData::MessageReplied { correlation_id, .. } => {
                    if let Some(meta) = self.history.get_mut(correlation_id) {
                        meta.round_trips += 1;
                    }
                }
                // Only for an id still in the dedupe window: one that rotated
                // out cannot be queued any more, so there is nothing left to
                // answer twice.
                EventData::MessageDeferred { correlation_id, .. }
                    if self.seen.contains(correlation_id) =>
                {
                    self.deferred.insert(correlation_id.clone());
                }
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
                    self.deferred.remove(&oldest);
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
        self.queues.get_mut(pane_id)?.pop_front()
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
    pub(crate) fn owes_deferral(&self, message: &PendingMessage) -> bool {
        !matches!(message.intent, MsgIntent::Fyi)
            && !self.deferred.contains(&message.correlation_id)
    }

    /// Record that `correlation_id` has been answered. Returns false when it
    /// already was — the caller must then send nothing.
    pub(crate) fn mark_deferred(&mut self, correlation_id: &str) -> bool {
        self.deferred.insert(correlation_id.to_string())
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
}
