//! Best-effort evidence for every authenticated wire refusal.
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
/// One server-wide fixed 60-second window for authenticated wire refusals.
/// Eight individual events per (principal, closed verb, typed reason), at most
/// 64 resident keys and 128 individual events globally. Unknown commands share
/// `unknown`; control categories are sub, unsub, ping, pong and protocol.
/// Keys never evict within a window. Resident suppression gets one exact
/// per-key summary unless the global budget is exhausted; new keys beyond
/// either cap and globally suppressed requests join one exact overflow total.
/// At most 65 summaries are reserved outside the individual-event budget.
/// Tick/next refusal closes expired windows; graceful shutdown flushes the
/// current partial window after draining connections. Crash loses pending
/// counts; restart/handoff starts fresh budgets.
/// Decisions and event construction hold the limiter mutex; publication never
/// does. Best-effort `EventHub::push` may block its caller on journal I/O or
/// degrade to memory-only, but cannot hold up concurrent limiter decisions.
/// Refusals have no effect to fence, so required-audit admission is inappropriate.
/// Pre-authentication failures have no verified identity and stay unaudited to
/// avoid an unauthenticated journal-amplification path.
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
    fn publish(&self, decide: impl FnOnce(&mut State) -> Vec<EventEnvelope>) {
        let (hub, events) = {
            let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
            (state.hub.clone(), decide(&mut state))
        };
        for event in events {
            hub.push(event);
        }
    }
    pub(super) fn tick(&self, now: Instant) {
        self.publish(|s| s.roll(now, false));
    }
    pub(super) fn flush(&self, now: Instant) {
        self.publish(|s| s.roll(now, true));
    }
    pub(super) fn record(
        &self,
        now: Instant,
        info: &crate::api::client_tokens::TokenInfo,
        verb: &str,
        args: &serde_json::Value,
        reason: ErrorCode,
    ) {
        let verb = commands::known_verb(verb).unwrap_or("unknown");
        self.reason(now, info, verb, args, reason.as_str());
    }
    pub(super) fn control(
        &self,
        now: Instant,
        info: &crate::api::client_tokens::TokenInfo,
        verb: &'static str,
        reason: ErrorCode,
    ) {
        self.reason(now, info, verb, &serde_json::Value::Null, reason.as_str());
    }
    fn reason(
        &self,
        now: Instant,
        info: &crate::api::client_tokens::TokenInfo,
        verb: &'static str,
        args: &serde_json::Value,
        reason: &'static str,
    ) {
        self.publish(|s| s.decide(now, info, verb, args, reason));
    }
    pub(super) fn subscription_revoked(
        &self,
        now: Instant,
        info: &crate::api::client_tokens::TokenInfo,
        stream: &str,
    ) {
        self.reason(
            now,
            info,
            "sub",
            &serde_json::json!({"target":stream}),
            ErrorCode::AuthRevoked.as_str(),
        );
    }
    pub(super) fn subscription(
        &self,
        now: Instant,
        info: &crate::api::client_tokens::TokenInfo,
        stream: &str,
        code: Option<&SubfailCode>,
    ) {
        let reason = match code {
            Some(SubfailCode::InvalidAgent) => "invalid_agent",
            Some(SubfailCode::CursorAhead) => "cursor_ahead",
            Some(SubfailCode::OperatorOnly) => "operator_only",
            Some(SubfailCode::UnknownStream) => "unknown_stream",
            Some(SubfailCode::Scope) => "scope",
            Some(SubfailCode::SessionBusy) => "session_busy",
            Some(SubfailCode::SubIdReused) => "sub_id_reused",
            None => "internal_error",
        };
        self.reason(
            now,
            info,
            "sub",
            &serde_json::json!({"target":stream}),
            reason,
        );
    }
    pub(super) fn malformed(
        &self,
        now: Instant,
        info: &crate::api::client_tokens::TokenInfo,
        frame: &serde_json::Value,
    ) {
        let verb = match frame["t"].as_str() {
            Some("cmd") => commands::known_verb(frame["verb"].as_str().unwrap_or("unknown"))
                .unwrap_or("unknown"),
            Some("sub") => "sub",
            Some("unsub") => "unsub",
            Some("ping") => "ping",
            Some("pong") => "pong",
            _ => "protocol",
        };
        self.reason(
            now,
            info,
            verb,
            frame.get("args").unwrap_or(frame),
            ErrorCode::InvalidParams.as_str(),
        );
    }
}
impl State {
    fn decide(
        &mut self,
        now: Instant,
        info: &crate::api::client_tokens::TokenInfo,
        verb: &'static str,
        args: &serde_json::Value,
        reason: &'static str,
    ) -> Vec<EventEnvelope> {
        let mut events = self.roll(now, false);
        let key = (info.principal.clone(), verb, reason);
        if self.emitted >= GLOBAL
            || (!self.entries.contains_key(&key) && self.entries.len() == KEYS)
        {
            self.overflow += 1;
            return events;
        }
        let entry = self.entries.entry(key.clone()).or_insert_with(|| Entry {
            emitted: 0,
            suppressed: 0,
        });
        if entry.emitted == PER_KEY {
            entry.suppressed += 1;
            return events;
        }
        entry.emitted += 1;
        self.emitted += 1;
        events.push(EventEnvelope {
            event: EventKind::WireRefused,
            data: EventData::WireRefused {
                principal: Some(key.0),
                token_id: Some(info.name.clone()),
                verb: Some(verb.into()),
                target: ["target", "agent", "stream"]
                    .iter()
                    .find_map(|key| args.get(key).and_then(serde_json::Value::as_str))
                    .map(|s| s.chars().take(256).collect()),
                reason: Some(reason.into()),
                suppressed: 0,
            },
        });
        events
    }
    fn roll(&mut self, now: Instant, force: bool) -> Vec<EventEnvelope> {
        let mut events = Vec::new();
        if !force && now.saturating_duration_since(self.start) < WINDOW {
            return events;
        }
        for ((principal, verb, reason), entry) in std::mem::take(&mut self.entries) {
            if entry.suppressed > 0 {
                events.push(EventEnvelope {
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
            events.push(EventEnvelope {
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
        events
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
        for verb in ["sub", "unsub", "ping", "pong", "protocol"] {
            record(&audit, now, "fixture-principal", verb);
        }
        assert_eq!(audit.0.lock().unwrap().entries.len(), 1);
        assert_eq!(events(&audit).len(), PER_KEY as usize);
        audit.tick(now + WINDOW);
        audit.tick(now + WINDOW);
        let events = events(&audit);
        assert_eq!(events.len(), PER_KEY as usize + 1);
        assert_eq!(events.last().unwrap()["suppressed"], 1005 - PER_KEY);
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
    #[test]
    fn blocked_push_does_not_block_concurrent_refusal_decision() {
        let audit = Arc::new(Refusals::new(Instant::now()));
        let now = Instant::now();
        for _ in 0..PER_KEY {
            record(&audit, now, "saturated", "agent.send");
        }
        let hub = EventHub::default();
        let (entered, started) = std::sync::mpsc::channel();
        let (release, resume) = std::sync::mpsc::channel();
        hub.before_push_for_test(move || {
            entered.send(()).unwrap();
            resume
                .recv_timeout(Duration::from_secs(5))
                .expect("blocked writer was not released");
        });
        audit.attach(hub);
        let first = audit.clone();
        let writer = std::thread::spawn(move || record(&first, now, "writer", "agent.send"));
        started
            .recv_timeout(Duration::from_secs(5))
            .expect("writer never reached push");
        let second = audit.clone();
        let (finished, decision) = std::sync::mpsc::channel();
        let decider = std::thread::spawn(move || {
            record(&second, now, "saturated", "agent.send");
            finished.send(()).unwrap();
        });
        let result = decision.recv_timeout(Duration::from_secs(1));
        release.send(()).unwrap();
        writer.join().unwrap();
        decider.join().unwrap();
        assert!(
            result.is_ok(),
            "journal push held the refusal decision lock"
        );
    }
}
