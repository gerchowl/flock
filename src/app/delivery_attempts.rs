//! Bounded local submission evidence. The accessor isolates future custody-store migration.
use std::collections::BTreeMap;

use crate::api::schema::DeliveryAttempt;

pub(super) struct DeliveryAttempts {
    by_id: BTreeMap<String, DeliveryAttempt>,
    next_id: u64,
}

impl DeliveryAttempts {
    pub(super) fn new(next_id: u64) -> Self {
        Self {
            by_id: BTreeMap::new(),
            next_id,
        }
    }

    fn evict_oldest_terminal(&mut self) -> bool {
        let oldest = self
            .by_id
            .values()
            .filter(|a| a.finished_at_ms.is_some() && a.state != "unconfirmed")
            .min_by_key(|a| (a.queued_at_ms, &a.attempt_id))
            .map(|a| a.attempt_id.clone());
        oldest.is_some_and(|id| self.by_id.remove(&id).is_some())
    }

    pub(super) fn reserve_id(&mut self) -> Result<String, &'static str> {
        while self.by_id.len() >= super::mailboxes::MAX_SEEN {
            if !self.evict_oldest_terminal() {
                return Err("delivery_attempt_capacity");
            }
        }
        self.next_id = self.next_id.checked_add(1).ok_or("attempt_id_exhausted")?;
        Ok(format!("attempt:{:020}", self.next_id))
    }

    pub(super) fn record(&mut self, attempt: DeliveryAttempt) {
        if let Some(id) = attempt
            .attempt_id
            .strip_prefix("attempt:")
            .and_then(|id| id.parse::<u64>().ok())
        {
            self.next_id = self.next_id.max(id);
        }
        self.by_id.insert(attempt.attempt_id.clone(), attempt);
        while self.by_id.len() > super::mailboxes::MAX_SEEN {
            if !self.evict_oldest_terminal() {
                break;
            }
        }
    }

    pub(super) fn clear(&mut self) {
        self.by_id.clear();
    }

    pub(super) fn snapshot(&self) -> Vec<DeliveryAttempt> {
        self.by_id.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(id: u64, state: &str) -> DeliveryAttempt {
        DeliveryAttempt {
            attempt_id: format!("attempt:{id:020}"),
            pane: "fixture-pane".into(),
            correlation_ids: Vec::new(),
            wake: false,
            state: state.into(),
            reason: None,
            queued_at_ms: id,
            typed_at_ms: None,
            submit_sent_at_ms: None,
            finished_at_ms: (state != "typed").then_some(id),
            retried: false,
        }
    }

    #[test]
    fn retention_evicts_oldest_terminal_and_protects_unconfirmed_and_pending() {
        let mut registry = DeliveryAttempts::new(0);
        registry.record(attempt(0, "typed"));
        registry.record(attempt(1, "unconfirmed"));
        for id in 2..super::super::mailboxes::MAX_SEEN as u64 {
            registry.record(attempt(id, "accepted"));
        }
        let id = registry.reserve_id().unwrap();
        let mut new = attempt(5000, "typed");
        new.attempt_id = id;
        registry.record(new);
        let remaining = registry.snapshot();
        assert_eq!(remaining.len(), super::super::mailboxes::MAX_SEEN);
        assert!(remaining.iter().any(|a| a.queued_at_ms == 0));
        assert!(remaining.iter().any(|a| a.queued_at_ms == 1));
        assert!(!remaining.iter().any(|a| a.queued_at_ms == 2));
    }

    #[test]
    fn protected_capacity_refuses_new_attempts_instead_of_losing_evidence() {
        let mut registry = DeliveryAttempts::new(0);
        for id in 0..super::super::mailboxes::MAX_SEEN as u64 {
            registry.record(attempt(id, "unconfirmed"));
        }
        assert_eq!(registry.reserve_id(), Err("delivery_attempt_capacity"));
        assert_eq!(registry.snapshot().len(), super::super::mailboxes::MAX_SEEN);
    }

    #[test]
    fn ids_advance_without_event_publication_and_past_restored_ids() {
        let mut registry = DeliveryAttempts::new(0);
        let first = registry.reserve_id().unwrap();
        let second = registry.reserve_id().unwrap();
        assert_ne!(first, second);
        registry.record(attempt(99, "accepted"));
        registry.clear();
        assert_eq!(
            registry.reserve_id().unwrap(),
            "attempt:00000000000000000100"
        );
    }
    #[test]
    fn older_submit_responses_and_missing_optional_evidence_fields_deserialize() {
        let result: crate::api::schema::ResponseResult =
            serde_json::from_value(serde_json::json!({
                "type": "guarded_submit", "outcome": "accepted", "reason": null, "retried": false
            }))
            .unwrap();
        assert!(matches!(
            result,
            crate::api::schema::ResponseResult::GuardedSubmit { attempt: None, .. }
        ));
        let evidence: DeliveryAttempt = serde_json::from_value(serde_json::json!({
            "attempt_id": "older", "pane": "fixture-pane", "correlation_ids": [],
            "wake": false, "state": "queued", "queued_at_ms": 1, "retried": false
        }))
        .unwrap();
        assert_eq!(evidence.typed_at_ms, None);
        assert_eq!(evidence.submit_sent_at_ms, None);
        assert_eq!(evidence.finished_at_ms, None);
        assert_eq!(evidence.reason, None);
    }
}
