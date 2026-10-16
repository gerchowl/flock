//! Contract self-tests use deliberately wrong traces, not a client implementation.
use super::*;
use fixture::{FixtureLine, FixtureStep};
use serde_json::{json, Value};

fn parse(value: Value) -> FixtureLine {
    FixtureLine::parse(&value.to_string()).unwrap()
}
fn script(name: &str) -> Vec<Value> {
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/client/test/fixtures/wire");
    std::fs::read_to_string(root.join(format!("{name}.jsonl")))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn round3_caller_stimuli_parse_without_becoming_expectations() {
    for action in [
        json!({"kind":"disconnect"}),
        json!({"kind":"set_token","principal":"other-fixture"}),
        json!({"kind":"network_change"}),
        json!({"kind":"foreground"}),
    ] {
        let line = parse(json!({"v":1,"step":"client","action":action}));
        assert!(matches!(line.step, FixtureStep::Client { .. }));
    }
}
#[test]
fn round3_given_restores_resync_and_command_status() {
    parse(json!({"v":1,"step":"given","cursors":{"fleet":2},
        "resync":{"fleet":{"snapshot_to":5,"unconfirmed":["node-a"]}},
        "now_ms":99,"idempotency_window_ms":100,
        "outbox":[{"cmd_id":"c","verb":"queue.add","args":{},"principal":"fixture-principal","created_ms":0,"first_sent_ms":0,"status":"sent"}]}));
}
#[test]
fn round3_six_outbox_states_and_no_other_spelling() {
    for state in [
        "pending",
        "sent",
        "uncertain",
        "unknown_outcome",
        "acked",
        "failed",
    ] {
        parse(json!({"v":1,"step":"observe","outbox":{"c":state}}));
    }
    assert!(
        FixtureLine::parse(&json!({"v":1,"step":"observe","outbox":{"c":"typo"}}).to_string())
            .is_err()
    );
}
#[test]
fn round3_observation_absence_and_transition_trace_parse() {
    parse(
        json!({"v":1,"step":"observe","absent_streams":["s1"],"absent_outbox":["c1"],
        "connection_transitions":["Stale","Reconnecting"]}),
    );
}
#[test]
fn round3_connect_lower_bound_and_client_close_parse() {
    parse(json!({"v":1,"step":"expect_connect","not_before_ms":250,"within_ms":500}));
    parse(json!({"v":1,"step":"expect_close","code":1002}));
    assert!(FixtureLine::parse(
        &json!({"v":1,"step":"expect_connect","not_before_ms":501,"within_ms":500}).to_string()
    )
    .is_err());
}

// A synthetic trace matcher pins the runner contract: advancing a deadline
// checks every intermediate effect, not only the final state at the ceiling.
fn timing(step: &Value, effect_ms: u64) -> bool {
    match step["step"].as_str().unwrap() {
        "expect_connect" => {
            effect_ms >= step["not_before_ms"].as_u64().unwrap_or(0)
                && effect_ms <= step["within_ms"].as_u64().unwrap()
        }
        "deadline" => {
            effect_ms
                == match step["stage"]["kind"].as_str().unwrap() {
                    "connect" => CONNECT_DEADLINE_MS,
                    "handshake" => HANDSHAKE_DEADLINE_MS,
                    "subscription" => SUBSCRIPTION_DEADLINE_MS,
                    "command_ack" => ACK_DEADLINE_MS,
                    other => panic!("unknown stage {other}"),
                }
        }
        _ => panic!("not a timing assertion"),
    }
}
fn frame_matches(expected: &Value, actual: &Value) -> bool {
    expected["frame"]["t"] == actual["t"]
        && expected["fields"].as_array().unwrap().iter().all(|key| {
            let key = key.as_str().unwrap();
            expected["frame"].get(key) == actual.get(key)
        })
        && expected
            .get("absent")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .all(|key| actual.get(key.as_str().unwrap()).is_none())
}
fn observation(expected: &Value, actual: &Value) -> bool {
    for key in ["connection", "connection_transitions"] {
        if let Some(value) = expected.get(key) {
            if actual.get(key) != Some(value) {
                return false;
            }
        }
    }
    for (key, absent) in [("streams", "absent_streams"), ("outbox", "absent_outbox")] {
        if let Some(entries) = expected[key].as_object() {
            if entries
                .iter()
                .any(|(id, value)| actual[key].get(id) != Some(value))
            {
                return false;
            }
        }
        if let Some(ids) = expected[absent].as_array() {
            if ids
                .iter()
                .any(|id| actual[key].get(id.as_str().unwrap()).is_some())
            {
                return false;
            }
        }
    }
    true
}
#[test]
fn round3_early_deadline_effect_fails_even_with_correct_final_state() {
    let steps = script("uncertain-command");
    let deadline = steps
        .iter()
        .find(|step| step["step"] == "deadline")
        .unwrap();
    assert!(timing(deadline, 30_000));
    assert!(
        !timing(deadline, 5_000),
        "a 5s client must fail, even if uncertain at 30s"
    );
    for kind in ["connect", "handshake", "subscription"] {
        let step = json!({"step":"deadline","stage":{"kind":kind}});
        let ceiling = match kind {
            "connect" => 10_000,
            "handshake" => 5_000,
            _ => 30_000,
        };
        assert!(!timing(&step, ceiling - 1));
        assert!(timing(&step, ceiling));
        assert!(!timing(&step, ceiling + 1));
    }
}
#[test]
fn round3_backoff_rejects_early_and_late_attempt_traces() {
    let step = json!({"step":"expect_connect","not_before_ms":15_000,"within_ms":30_000});
    parse(json!({"v":1,"step":"expect_connect","not_before_ms":15_000,"within_ms":30_000}));
    assert!(!timing(&step, 14_999));
    assert!(timing(&step, 15_000));
    assert!(timing(&step, 30_000));
    assert!(!timing(&step, 30_001));
}
#[test]
fn round3_absent_entries_and_transient_states_reject_wrong_traces() {
    let step = json!({"v":1,"step":"observe","absent_streams":["s1"],"absent_outbox":["c"],
        "connection_transitions":["Stale","Reconnecting"]});
    parse(step.clone());
    let good = json!({"streams":{},"outbox":{},"connection_transitions":["Stale","Reconnecting"]});
    assert!(observation(&step, &good));
    for bad in [
        json!({"streams":{"s1":null},"outbox":{},"connection_transitions":["Stale","Reconnecting"]}),
        json!({"streams":{},"outbox":{"c":"pending"},"connection_transitions":["Stale","Reconnecting"]}),
        json!({"streams":{},"outbox":{},"connection_transitions":["Reconnecting"]}),
    ] {
        assert!(!observation(&step, &bad));
    }
}

#[test]
fn round3_new_action_scripts_reject_wrong_observable_client_traces() {
    for (name, connection) in [
        ("client-disconnect", "Disconnected"),
        ("set-token-changed-principal", "Live"),
        ("set-token-same-principal", "Live"),
        ("network-change", "Live"),
        ("foreground", "Live"),
    ] {
        let steps = script(name);
        let expected = steps
            .iter()
            .rev()
            .find(|step| step["step"] == "observe")
            .unwrap();
        assert_eq!(expected["connection"], connection);
        let mut good = expected.clone();
        good["streams"] = json!({});
        good["outbox"] = expected.get("outbox").cloned().unwrap_or(json!({}));
        assert!(observation(expected, &good));
        let mut bad = good.clone();
        bad["connection"] = json!("NeedsToken");
        assert!(!observation(expected, &bad), "{name}: wrong action outcome");
        if name == "set-token-changed-principal" {
            bad = good;
            bad["outbox"] = json!({"c1":"pending"});
            assert!(
                !observation(expected, &bad),
                "changed principal retained a command"
            );
        }
        if name == "network-change" || name == "foreground" {
            let connect = steps
                .iter()
                .rev()
                .find(|step| step["step"] == "expect_connect")
                .unwrap();
            assert!(timing(connect, 0));
            assert!(!timing(connect, 250));
        }
    }
}
#[test]
fn round3_restored_status_resync_and_close_reject_wrong_client_traces() {
    let steps = script("restored-outbox-status");
    assert_eq!(steps[0]["outbox"][0]["status"], "pending");
    assert_eq!(steps[0]["outbox"][1]["status"], "sent");
    for (id, resume) in [("c1", false), ("c2", true)] {
        let expected = steps
            .iter()
            .find(|step| step["frame"]["cmd_id"] == id && step["step"] == "expect")
            .unwrap();
        assert_eq!(expected["frame"]["resume_only"], resume);
        let mut wrong = expected["frame"].clone();
        wrong["resume_only"] = json!(!resume);
        assert!(frame_matches(expected, &expected["frame"]));
        assert!(
            !frame_matches(expected, &wrong),
            "wrong resume_only must fail the script"
        );
    }
    let steps = script("restored-resync");
    assert_eq!(steps[0]["resync"]["fleet"]["snapshot_to"], 5);
    assert_eq!(
        steps[0]["resync"]["fleet"]["unconfirmed"],
        json!(["node-a", "node-b"])
    );
    let expected = steps
        .iter()
        .find(|step| step["streams"]["s1"]["state"] == "Resyncing")
        .unwrap();
    let mut wrong = expected.clone();
    wrong["streams"]["s1"]["state"] = json!("Current");
    assert!(!observation(expected, &wrong));
    let steps = script("protocol-refusal");
    let expected = steps
        .iter()
        .find(|step| step["step"] == "expect_close")
        .unwrap();
    parse(expected.clone());
    let close_matches = |trace: &[Value]| {
        trace.first().is_some_and(|value| {
            value["kind"] == "client_close" && value["code"] == expected["code"]
        })
    };
    assert!(close_matches(&[json!({"kind":"client_close","code":1002})]));
    assert!(!close_matches(&[
        json!({"kind":"client_close","code":1000})
    ]));
    assert!(!close_matches(&[
        json!({"kind":"server_close","code":1002})
    ]));
    assert!(!close_matches(&[]));
}
#[test]
fn round3_required_recovery_scenarios_pin_their_key_steps() {
    let has = |name: &str, predicate: fn(&Value) -> bool| {
        assert!(
            script(name).iter().any(predicate),
            "{name}: key step missing"
        )
    };
    has("subfail-cursor_ahead", |s| {
        s["step"] == "expect" && s["frame"]["t"] == "sub" && s["absent"] == json!(["after"])
    });
    has("old-socket", |s| {
        s["step"] == "send" && s["socket"] == "previous"
    });
    has("bootstrap-snapshot", |s| {
        s["frame"]["t"] == "gap" && s["frame"].get("snapshot_to").is_some()
    });
    has("stream-independence", |s| {
        s["streams"]["s1"]["state"] == "Resyncing" && s["streams"]["s2"]["state"] == "Resyncing"
    });
    has("stream-independence", |s| {
        s["streams"]["s1"]["state"] == "Current" && s["streams"]["s2"]["state"] == "Resyncing"
    });
    has("lagged", |s| s["streams"]["s1"]["state"] == "Replaying");
    has("heartbeat-missed", |s| {
        s["connection_transitions"] == json!(["Stale", "Reconnecting"])
    });
    let rows = script("heartbeat-missed");
    let ping = rows.iter().position(|s| s["frame"]["t"] == "ping").unwrap();
    assert_eq!(rows[ping - 1]["ceiling_ms"], HEARTBEAT_INTERVAL_MS);
    for name in [
        "subfail-scope",
        "subfail-operator_only",
        "subfail-unknown_stream",
    ] {
        has(name, |s| {
            s["streams"]["s1"]["state"] == "Refused" && s["streams"]["s1"]["cursor"] == 9
        });
    }
    let rows = script("subscription-deadline");
    let ping = rows
        .iter()
        .position(|s| s["frame"]["nonce"] == "unrelated")
        .unwrap();
    assert_eq!(rows[ping - 1]["step"], "quiet");
    assert!(rows[ping - 1]["ceiling_ms"].as_u64().unwrap() > 0);
    let deadline = rows
        .iter()
        .position(|s| s["stage"]["kind"] == "subscription")
        .unwrap();
    assert_eq!(rows[deadline]["stage"]["sub_id"], "s1");
    assert_eq!(rows[deadline + 1]["frame"]["t"], "unsub");
    assert_eq!(rows[deadline + 1]["frame"]["sub_id"], "s1");
    assert_eq!(rows[deadline + 2]["frame"]["t"], "sub");
    assert_eq!(rows[deadline + 2]["frame"]["sub_id"], "s1");
    assert_eq!(rows[deadline + 3]["streams"]["s2"]["state"], "Current");
}
#[test]
fn round3_cap_reset_and_expiry_scenarios_assert_real_timing_and_states() {
    let cap = script("backoff-cap");
    let ceilings: Vec<_> = cap
        .iter()
        .filter(|s| s["step"] == "expect_connect")
        .map(|s| s["within_ms"].as_u64().unwrap())
        .collect();
    assert_eq!(
        ceilings,
        [1, 500, 1000, 2000, 4000, 8000, 16000, 30000, 30000]
    );
    let reset = script("backoff-reset");
    let retry = reset
        .iter()
        .rposition(|s| s["step"] == "expect_connect")
        .unwrap();
    assert_eq!(reset[retry]["not_before_ms"], 250);
    assert_eq!(reset[retry]["within_ms"], 500);
    let healthy = reset[..retry]
        .iter()
        .rposition(|s| s["connection"] == "Live")
        .unwrap();
    let elapsed: u64 = reset[healthy + 1..retry]
        .iter()
        .filter(|s| s["step"] == "quiet")
        .map(|s| s["ceiling_ms"].as_u64().unwrap())
        .sum();
    assert_eq!(elapsed, BACKOFF_RESET_MS);
    for state in [
        "Disconnected",
        "Connecting",
        "Hello",
        "Syncing",
        "Live",
        "ReadOnly",
        "NeedsToken",
        "Reconnecting",
    ] {
        let rows = script(&format!("outbox-window-expiry-{}", state.to_lowercase()));
        assert_eq!(rows[0]["now_ms"], 2099);
        assert_eq!(rows[0]["outbox"][0]["created_ms"], 0);
        assert_eq!(rows[0]["outbox"][0]["first_sent_ms"], 100);
        assert_eq!(rows[0]["idempotency_window_ms"], 2000);
        let before = rows.iter().find(|s| s["outbox"]["c1"] == "sent").unwrap();
        assert_eq!(before["connection"], state);
        let index = rows
            .iter()
            .position(|s| s["outbox"]["c1"] == "unknown_outcome")
            .unwrap();
        let expected = &rows[index];
        assert_eq!(expected["connection"], state);
        assert_eq!(rows[index - 1]["step"], "quiet");
        assert_eq!(rows[index - 1]["ceiling_ms"], 2);
        let mut bad = expected.clone();
        bad["outbox"]["c1"] = json!("sent");
        assert!(
            !observation(expected, &bad),
            "{state}: expiry cannot depend on connectivity"
        );
    }
}
#[test]
fn round3_absence_and_restored_status_typos_are_explicit_errors() {
    for raw in [
        json!({"v":1,"step":"observe","streams":{"s": {"state":"Current","cursor":0}},"absent_streams":["s"]}),
        json!({"v":1,"step":"observe","absent_outbox":["c","c"]}),
        json!({"v":1,"step":"given","cursors":{},"outbox":[{"cmd_id":"c","verb":"x","args":{},"principal":"fixture","created_ms":0,"status":"bad"}]}),
        json!({"v":1,"step":"client","action":{"kind":"set_token","principal":"fixture","typo":true}}),
    ] {
        assert!(FixtureLine::parse(&raw.to_string()).is_err(), "{raw}");
    }
}
