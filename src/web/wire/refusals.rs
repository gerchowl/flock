//! Best-effort evidence for authenticated commands refused before dispatch.
use super::*;
use crate::api::{
    error_code::ErrorCode,
    schema::{EventData, EventEnvelope, EventKind},
    EventHub,
};
use std::collections::BTreeMap;

const WINDOW: Duration = Duration::from_secs(60);
const KEYS: usize = 64;
const PER_KEY: u64 = 8;
const GLOBAL: u64 = 128;

type Key = (String, &'static str, &'static str);
struct Entry {
    emitted: u64,
    suppressed: u64,
}
struct State {
    hub: EventHub,
    start: Instant,
    entries: BTreeMap<Key, Entry>,
    emitted: u64,
    overflow: u64,
}
/// One server-wide fixed 60-second window, shared by ingress and queue races.
/// Emit at most eight refusals per (authenticated principal, registered verb,
/// typed reason), 128 individual events globally, and retain at most 64 keys.
/// Unknown verbs share `unknown`. Keys never evict before window expiry.
/// Excess requests for resident keys count toward one per-key summary, unless
/// the global budget is exhausted, when they join the exact overflow total.
/// Nonresident keys beyond either cap also join that single aggregate summary.
/// Summaries are outside the individual-event budget (at most 65 per window).
/// Tick or the next request closes the window, including idle-key summaries.
/// Restart/handoff resets budgets and loses pending suppression counts.
/// No refusal has an effect to fence: use best-effort `EventHub::push`, whose
/// storage failures may degrade to memory-only, never required-audit admission.
/// Unauthenticated traffic has no attributable principal and emits no events,
/// avoiding an unauthenticated journal-amplification path.
pub(super) struct Refusals(Mutex<State>);
impl Refusals {
    pub(super) fn new(now: Instant) -> Self {
        Self(Mutex::new(State {
            hub: EventHub::default(),
            start: now,
            entries: BTreeMap::new(),
            emitted: 0,
            overflow: 0,
        }))
    }
    pub(super) fn attach(&self, hub: EventHub) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).hub = hub;
    }
    pub(super) fn tick(&self, now: Instant) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).roll(now);
    }
    pub(super) fn record(
        &self,
        now: Instant,
        session: &crate::api::client_tokens::TokenInfo,
        verb: &str,
        args: &serde_json::Value,
        reason: ErrorCode,
    ) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.roll(now);
        let verb = commands::known_verb(verb).unwrap_or("unknown");
        let key = (session.principal.clone(), verb, reason.as_str());
        if state.emitted >= GLOBAL
            || (!state.entries.contains_key(&key) && state.entries.len() == KEYS)
        {
            state.overflow += 1;
            return;
        }
        let entry = state.entries.entry(key.clone()).or_insert_with(|| Entry {
            emitted: 0,
            suppressed: 0,
        });
        if entry.emitted == PER_KEY {
            entry.suppressed += 1;
            return;
        }
        entry.emitted += 1;
        state.emitted += 1;
        state.hub.push(EventEnvelope {
            event: EventKind::WireRefused,
            data: EventData::WireRefused {
                principal: Some(key.0),
                token_id: Some(session.name.clone()),
                verb: Some(verb.into()),
                target: args
                    .get("target")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| args.get("agent").and_then(serde_json::Value::as_str))
                    .map(|s| s.chars().take(256).collect()),
                reason: Some(reason.as_str().into()),
                suppressed: 0,
            },
        });
    }
}
impl State {
    fn roll(&mut self, now: Instant) {
        if now.saturating_duration_since(self.start) < WINDOW {
            return;
        }
        for ((principal, verb, reason), entry) in std::mem::take(&mut self.entries) {
            if entry.suppressed > 0 {
                self.hub.push(EventEnvelope {
                    event: EventKind::WireRefused,
                    data: EventData::WireRefused {
                        principal: Some(principal),
                        token_id: None,
                        verb: Some(verb.into()),
                        target: None,
                        reason: Some(reason.into()),
                        suppressed: entry.suppressed,
                    },
                });
            }
        }
        if self.overflow > 0 {
            self.hub.push(EventEnvelope {
                event: EventKind::WireRefused,
                data: EventData::WireRefused {
                    principal: None,
                    token_id: None,
                    verb: None,
                    target: None,
                    reason: None,
                    suppressed: self.overflow,
                },
            });
        }
        self.start = now;
        self.emitted = 0;
        self.overflow = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::client_tokens::TokenInfo;
    use serde_json::{json, Value};

    fn info(principal: &str) -> TokenInfo {
        TokenInfo {
            name: "fixture-token".into(),
            principal: principal.into(),
            scopes: Default::default(),
            expires_in_ms: None,
        }
    }
    fn events(audit: &Refusals) -> Vec<Value> {
        audit
            .0
            .lock()
            .unwrap()
            .hub
            .events_after(0)
            .unwrap()
            .into_iter()
            .map(|(_, event)| serde_json::to_value(event.data).unwrap())
            .collect()
    }
    fn record(audit: &Refusals, now: Instant, principal: &str, verb: &str) {
        audit.record(
            now,
            &info(principal),
            verb,
            &json!({}),
            ErrorCode::SessionBusy,
        );
    }
    #[test]
    fn unknown_flood_has_one_key_and_one_exact_summary_even_when_idle() {
        let now = Instant::now();
        let audit = Refusals::new(now);
        for n in 0..1000 {
            record(&audit, now, "fixture-principal", &format!("unknown-{n}"));
        }
        assert_eq!(audit.0.lock().unwrap().entries.len(), 1);
        assert_eq!(events(&audit).len(), PER_KEY as usize);
        audit.tick(now + WINDOW);
        audit.tick(now + WINDOW);
        let events = events(&audit);
        assert_eq!(events.len(), PER_KEY as usize + 1);
        assert_eq!(events.last().unwrap()["suppressed"], 1000 - PER_KEY);
        assert_eq!(events.last().unwrap()["verb"], "unknown");
        assert!(events.last().unwrap()["token_id"].is_null());
    }
    #[test]
    fn key_capacity_routes_exact_total_to_one_overflow_summary_without_eviction() {
        let now = Instant::now();
        let audit = Refusals::new(now);
        for n in 0..KEYS {
            record(&audit, now, &format!("fixture-{n}"), "agent.send");
        }
        for n in 0..100 {
            record(&audit, now, &format!("overflow-{n}"), "agent.send");
        }
        assert_eq!(audit.0.lock().unwrap().entries.len(), KEYS);
        // New keys cannot evict and renew the first key's budget.
        for _ in 0..20 {
            record(&audit, now, "fixture-0", "agent.send");
        }
        audit.tick(now + WINDOW);
        audit.tick(now + WINDOW);
        let events = events(&audit);
        assert_eq!(
            events
                .iter()
                .filter(|e| e["principal"] == "fixture-0" && e["suppressed"] == 0)
                .count(),
            PER_KEY as usize
        );
        let summaries: Vec<_> = events.iter().filter(|e| e["suppressed"] != 0).collect();
        assert_eq!(summaries.len(), 2);
        assert_eq!(
            summaries.iter().find(|e| e["principal"].is_null()).unwrap()["suppressed"],
            100
        );
        assert_eq!(
            summaries
                .iter()
                .find(|e| e["principal"] == "fixture-0")
                .unwrap()["suppressed"],
            21 - PER_KEY
        );
        assert!(audit.0.lock().unwrap().entries.is_empty());
        record(&audit, now + WINDOW, "new-window", "agent.send");
        assert_eq!(audit.0.lock().unwrap().entries.len(), 1);
    }
    #[test]
    fn global_budget_counts_resident_and_nonresident_overflow_exactly() {
        let now = Instant::now();
        let audit = Refusals::new(now);
        for n in 0..(GLOBAL / PER_KEY) {
            for _ in 0..PER_KEY {
                record(&audit, now, &format!("fixture-{n}"), "agent.stop");
            }
        }
        record(&audit, now, "fixture-0", "agent.stop");
        record(&audit, now, "overflow", "agent.stop");
        assert_eq!(events(&audit).len(), GLOBAL as usize);
        audit.tick(now + WINDOW);
        let events = events(&audit);
        assert_eq!(events.len(), GLOBAL as usize + 1);
        assert_eq!(events.last().unwrap()["suppressed"], 2);
        assert!(events.last().unwrap()["principal"].is_null());
    }
    #[test]
    fn budgets_share_principal_but_separate_registered_verbs_and_typed_reasons() {
        let now = Instant::now();
        let audit = Refusals::new(now);
        for verb in [
            "agent.send",
            "agent.decide",
            "agent.start",
            "agent.restart",
            "agent.stop",
        ] {
            for _ in 0..PER_KEY {
                record(&audit, now, "fixture-principal", verb);
            }
            let mut other = info("fixture-principal");
            other.name = "other-token".into();
            audit.record(now, &other, verb, &json!({}), ErrorCode::SessionBusy);
            audit.record(now, &other, verb, &json!({}), ErrorCode::InvalidParams);
        }
        assert_eq!(audit.0.lock().unwrap().entries.len(), 10);
        audit.tick(now + WINDOW);
        let events = events(&audit);
        assert_eq!(events.iter().filter(|e| e["suppressed"] == 1).count(), 5);
    }
    #[test]
    fn refusal_and_summary_survive_journal_reopen_without_secret_or_body() {
        let now = Instant::now();
        let root = crate::test_support::TempDir::socket();
        let path = root.path().join("events.jsonl");
        let audit = Refusals::new(now);
        audit.attach(EventHub::with_persistence(path.clone()));
        for _ in 0..10 {
            audit.record(
                now,
                &info("fixture-principal"),
                "agent.send",
                &json!({"target":"fixture-agent","text":"secret-command-body"}),
                ErrorCode::SessionBusy,
            );
        }
        audit.tick(now + WINDOW);
        let journal = std::fs::read_to_string(&path).unwrap();
        assert!(!journal.contains("secret-command-body"));
        let reopened = EventHub::with_persistence(path);
        assert_eq!(
            reopened.persisted_events_after(0).len(),
            PER_KEY as usize + 1
        );
        assert!(EventKind::WireRefused.is_persisted());
    }
}
