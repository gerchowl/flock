use std::collections::{HashMap, HashSet, VecDeque};

use crate::api::schema::{EventData, EventEnvelope};

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
