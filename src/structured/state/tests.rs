use super::*;
use serde_json::{json, Value};

const STALL: Duration = Duration::from_millis(100);
const CATALOGUE: &str = include_str!("../../../tests/fixtures/claude-session/events.jsonl");
fn fixture(kind: &str, discriminator: Option<(&str, &str)>) -> Value {
    CATALOGUE
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|v| {
            v["type"] == kind
                && discriminator
                    .is_none_or(|(path, value)| v.pointer(path).is_some_and(|v| v == value))
        })
        .expect("recorded fixture")
}
fn event(value: Value) -> SessionEvent {
    SessionEvent::parse(&value.to_string(), "2.1.296")
}
fn step(state: &mut SessionState, value: Value, at: Instant) -> Update {
    state.reduce(Input::Event(&event(value)), at, 1_000, STALL)
}
fn started() -> Value {
    fixture("command_lifecycle", Some(("/state", "started")))
}
fn result() -> Value {
    fixture("result", Some(("/subtype", "success")))
}
fn permission(id: &str, tool: &str) -> Value {
    let mut v = fixture("control_request", None);
    v["request_id"] = id.into();
    v["request"]["tool_name"] = tool.into();
    v
}
fn clock(state: &mut SessionState, at: Instant) -> Update {
    state.reduce(Input::Observe, at, 1_000, STALL)
}

#[test]
fn recorded_turn_boundaries_use_session_authority() {
    for start in [
        started(),
        fixture("system", Some(("/status", "requesting"))),
        fixture("stream_event", Some(("/event/type", "message_start"))),
    ] {
        for end in [
            result(),
            fixture("result", Some(("/subtype", "error_during_execution"))),
        ] {
            let now = Instant::now();
            let mut reducer = SessionState::new("agent_fixture".into());
            let working = step(&mut reducer, start.clone(), now);
            assert_eq!(working.state.turn, Turn::Working);
            assert_eq!(working.state.phase, Phase::Working);
            assert_eq!(
                working.phase_changed,
                Some((Phase::Unknown, Phase::Working))
            );
            assert_eq!(working.state.source.turn, Some(StateSource::Session));
            assert_eq!(working.state.source.attention, Some(StateSource::Session));
            let idle = step(&mut reducer, end, now + Duration::from_millis(1));
            assert_eq!(idle.state.turn, Turn::Idle);
            assert_eq!(idle.state.phase, Phase::Settling);
            assert_eq!(idle.phase_changed, Some((Phase::Working, Phase::Settling)));
            assert_eq!(idle.state.background, Background::Unknown);
            assert_ne!(
                clock(&mut reducer, now + Duration::from_secs(30))
                    .state
                    .phase,
                Phase::Finished
            );
        }
    }
}

#[test]
fn every_open_request_requires_its_own_host_answer() {
    let now = Instant::now();
    let mut reducer = SessionState::new("agent_fixture".into());
    step(&mut reducer, started(), now);
    let first = step(&mut reducer, permission("p1", "Read"), now);
    assert_eq!(first.state.phase, Phase::NeedsHuman);
    assert_eq!(first.state.turn, Turn::Unknown);
    assert_eq!(first.needs_human, [("p1".into(), HumanReason::Permission)]);
    step(&mut reducer, permission("p2", "AskUserQuestion"), now);
    // Stdout responses acknowledge host-initiated controls, not incoming permissions.
    for kind in ["success", "error"] {
        let mut response = fixture("control_response", Some(("/response/subtype", kind)));
        response["response"]["request_id"] = "p1".into();
        assert_eq!(step(&mut reducer, response, now).needs_human.len(), 2);
    }
    assert_eq!(
        reducer
            .reduce(Input::ControlAnswered("other"), now, 1000, STALL)
            .needs_human
            .len(),
        2
    );
    let answered = reducer.reduce(Input::ControlAnswered("p1"), now, 1000, STALL);
    assert_eq!(answered.state.phase, Phase::NeedsHuman);
    assert_eq!(answered.needs_human, [("p2".into(), HumanReason::Question)]);
    let answered = reducer.reduce(Input::ControlAnswered("p2"), now, 1000, STALL);
    assert!(answered.needs_human.is_empty());
    assert_eq!(answered.state.phase, Phase::Working);
}

#[test]
fn limits_preserve_turn_scope_and_use_epoch_seconds() {
    let now = Instant::now();
    let mut reducer = SessionState::new("agent_fixture".into());
    step(&mut reducer, started(), now);
    let mut limit = fixture("rate_limit_event", None);
    limit["rate_limit_info"]["status"] = "rejected".into();
    limit["rate_limit_info"]["resetsAt"] = 3.into();
    let blocked = step(&mut reducer, limit.clone(), now);
    assert_eq!(blocked.state.phase, Phase::LimitReached);
    assert_eq!(blocked.state.turn, Turn::Unknown);
    assert_eq!(
        blocked.state.attention,
        Attention::Blocked {
            reason: BlockedReason::ProviderLimit,
            retry_after_ms: Some(2000)
        }
    );
    assert_eq!(
        step(&mut reducer, result(), now).state.phase,
        Phase::LimitReached
    );
    assert_eq!(
        step(&mut reducer, started(), now).state.phase,
        Phase::Working
    );
    for resets in [Value::Null, json!("future-format"), json!(u64::MAX)] {
        limit["rate_limit_info"]["resetsAt"] = resets;
        assert_eq!(
            step(&mut reducer, limit.clone(), now).state.attention,
            Attention::Blocked {
                reason: BlockedReason::ProviderLimit,
                retry_after_ms: None
            }
        );
    }
    assert_eq!(
        step(&mut reducer, fixture("rate_limit_event", None), now)
            .state
            .phase,
        Phase::Working
    );
}

#[test]
fn unknown_and_non_boundary_events_do_not_change_axes() {
    let now = Instant::now();
    let mut reducer = SessionState::new("agent_fixture".into());
    let working = step(&mut reducer, started(), now).state;
    assert_eq!(working.phase, Phase::Working);
    for value in [
        json!({"type":"future", "state":"idle"}),
        json!({"type":"result", "subtype":"future", "is_error":false}),
        fixture("command_lifecycle", Some(("/state", "completed"))),
        fixture("system", Some(("/status", "compacting"))),
        fixture("stream_event", Some(("/event/type", "message_stop"))),
    ] {
        let update = step(&mut reducer, value, now);
        assert_eq!(update.state, working);
        assert!(update.phase_changed.is_none());
    }
    let unknown = SessionEvent::parse("non-json diagnostic", "2.1.296");
    assert_eq!(
        reducer
            .reduce(Input::Event(&unknown), now, 1000, STALL)
            .state,
        working
    );
}

#[test]
fn nested_result_does_not_end_the_parent_turn() {
    let now = Instant::now();
    let mut reducer = SessionState::new("agent_fixture".into());
    step(&mut reducer, started(), now);
    let mut nested = result();
    nested["parent_tool_use_id"] = "subagent-tool".into();
    assert_eq!(step(&mut reducer, nested, now).state.turn, Turn::Working);
}

#[test]
fn silence_observation_is_bounded_and_late_result_recovers() {
    let now = Instant::now();
    let mut reducer = SessionState::new("agent_fixture".into());
    assert!(clock(&mut reducer, now + STALL).stalled_since_ms.is_none());
    assert_eq!(
        step(&mut reducer, started(), now).observe_at,
        Some(now + STALL)
    );
    assert!(clock(&mut reducer, now + STALL - Duration::from_millis(1))
        .stalled_since_ms
        .is_none());
    let stalled = clock(&mut reducer, now + STALL);
    assert_eq!(stalled.stalled_since_ms, Some(1000));
    assert_eq!(stalled.state.phase, Phase::Working);
    assert!(stalled.phase_changed.is_none());
    assert!(clock(&mut reducer, now + STALL * 2)
        .stalled_since_ms
        .is_none());
    // Even unknown stdout proves progress, without changing axes.
    step(&mut reducer, json!({"type":"future"}), now + STALL * 2);
    assert!(clock(&mut reducer, now + STALL * 2)
        .stalled_since_ms
        .is_none());
    assert_eq!(
        clock(&mut reducer, now + STALL * 3).stalled_since_ms,
        Some(1000)
    );
    assert_eq!(
        step(&mut reducer, result(), now + STALL * 4).state.turn,
        Turn::Idle
    );
    assert!(clock(&mut reducer, now + STALL * 5)
        .stalled_since_ms
        .is_none());
}

#[test]
fn open_permission_and_executing_tool_exclude_stalls() {
    let now = Instant::now();
    let mut reducer = SessionState::new("agent_fixture".into());
    step(&mut reducer, started(), now);
    step(&mut reducer, permission("p1", "Read"), now);
    assert!(clock(&mut reducer, now + STALL).stalled_since_ms.is_none());
    reducer.reduce(Input::ControlAnswered("p1"), now, 1000, STALL);
    let mut tool = fixture("stream_event", Some(("/event/type", "content_block_start")));
    tool["event"]["content_block"] = json!({"type":"tool_use","id":"tool-example","name":"Read"});
    step(&mut reducer, tool, now);
    // Finishing the argument block does not finish the running tool.
    step(
        &mut reducer,
        fixture("stream_event", Some(("/event/type", "content_block_stop"))),
        now,
    );
    assert!(clock(&mut reducer, now + STALL * 10)
        .stalled_since_ms
        .is_none());
    step(&mut reducer, fixture("user", None), now + STALL * 10);
    assert_eq!(
        clock(&mut reducer, now + STALL * 11).stalled_since_ms,
        Some(1000)
    );
}

#[test]
fn complete_assistant_tool_calls_also_exclude_stalls() {
    let now = Instant::now();
    let mut reducer = SessionState::new("agent_fixture".into());
    step(&mut reducer, started(), now);
    let mut assistant = fixture("assistant", None);
    assistant["message"]["content"] =
        json!([{"type":"tool_use","id":"tool-example","name":"Read"}]);
    step(&mut reducer, assistant, now);
    assert!(clock(&mut reducer, now + STALL).stalled_since_ms.is_none());
    step(&mut reducer, fixture("user", None), now + STALL);
    assert!(clock(&mut reducer, now + STALL * 2)
        .stalled_since_ms
        .is_some());
}

#[test]
fn crash_and_registry_lifecycle_precedence_are_independent_of_turn_outcome() {
    for lifecycle in [Lifecycle::Offline, Lifecycle::Hibernated, Lifecycle::Gone] {
        let now = Instant::now();
        let mut reducer = SessionState::new("agent_fixture".into());
        step(&mut reducer, started(), now);
        let crashed = reducer.reduce(Input::ProcessExited, now, 1000, STALL);
        assert_eq!(crashed.state.lifecycle, Lifecycle::Live);
        assert_eq!(crashed.state.phase, Phase::Blocked);
        assert_eq!(
            crashed.state.attention,
            Attention::Blocked {
                reason: BlockedReason::Crashed,
                retry_after_ms: None
            }
        );
        let projected = reducer.reduce(Input::Lifecycle(lifecycle), now, 1000, STALL);
        assert_eq!(
            projected.state.phase,
            match lifecycle {
                Lifecycle::Offline => Phase::Offline,
                Lifecycle::Hibernated => Phase::Hibernated,
                _ => Phase::Gone,
            }
        );
        assert!(clock(&mut reducer, now + STALL).stalled_since_ms.is_none());
    }
    let now = Instant::now();
    let mut reducer = SessionState::new("agent_fixture".into());
    step(&mut reducer, started(), now);
    step(&mut reducer, result(), now);
    assert_eq!(
        reducer
            .reduce(Input::ProcessExited, now, 1000, STALL)
            .state
            .attention,
        Attention::None
    );
}

#[derive(Default)]
struct Sink(Vec<Update>);
impl StateSink for Sink {
    fn publish(&mut self, update: Update) {
        self.0.push(update);
    }
}
#[test]
fn keyed_sink_never_changes_another_agent_or_publishes_eventhub_events() {
    let now = Instant::now();
    let mut a = SessionState::new("agent_a".into());
    let mut b = SessionState::new("agent_b".into());
    let mut sink = Sink::default();
    a.publish(Input::Event(&event(started())), now, 1000, STALL, &mut sink);
    b.publish(
        Input::Event(&event(permission("p1", "Read"))),
        now,
        1000,
        STALL,
        &mut sink,
    );
    assert_eq!(sink.0[0].agent_id, "agent_a");
    assert_eq!(sink.0[0].state.phase, Phase::Working);
    assert_eq!(sink.0[1].agent_id, "agent_b");
    assert_eq!(sink.0[1].state.phase, Phase::NeedsHuman);
    assert_eq!(clock(&mut a, now).state.phase, Phase::Working);
    assert_eq!(clock(&mut b, now).needs_human.len(), 1);
}
