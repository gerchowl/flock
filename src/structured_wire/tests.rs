use super::*;
use crate::structured::claude::SessionEvent;
use fixture::{FixtureLine, FixtureStep};
use serde_json::json;
use std::collections::BTreeSet;

fn fixtures() -> Vec<(String, String)> {
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/client/test/fixtures/wire");
    let mut files: Vec<_> = std::fs::read_dir(root)
        .unwrap()
        .map(Result::unwrap)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .map(|path| {
            (
                path.file_stem().unwrap().to_str().unwrap().to_owned(),
                std::fs::read_to_string(path).unwrap(),
            )
        })
        .collect();
    files.sort();
    files
}

#[test]
fn every_script_parses_and_round_trips_all_frame_kinds() {
    let mut server = BTreeSet::new();
    let mut client = BTreeSet::new();
    let files = fixtures();
    for (name, content) in &files {
        FixtureLine::parse_script(content).unwrap_or_else(|err| panic!("{name}: {err}"));
        for (i, line) in content.lines().enumerate() {
            let parsed =
                FixtureLine::parse(line).unwrap_or_else(|err| panic!("{name}:{}: {err}", i + 1));
            let value = serde_json::to_value(&parsed).unwrap();
            assert_eq!(FixtureLine::parse(&value.to_string()).unwrap(), parsed);
            match parsed.step {
                FixtureStep::Send { frame, .. } => {
                    server.insert(value["frame"]["t"].as_str().unwrap().to_owned());
                    let encoded = serde_json::to_string(&frame).unwrap();
                    assert_eq!(
                        serde_json::from_str::<ServerFrame>(&encoded).unwrap(),
                        frame
                    );
                }
                FixtureStep::Expect { frame, .. } => {
                    client.insert(value["frame"]["t"].as_str().unwrap().to_owned());
                    let encoded = serde_json::to_string(&frame).unwrap();
                    assert_eq!(
                        serde_json::from_str::<ClientFrame>(&encoded).unwrap(),
                        frame
                    );
                }
                _ => {}
            }
        }
    }
    assert_eq!(
        server,
        ["welcome", "ping", "ev", "synced", "subfail", "gap", "lagged", "ack"]
            .map(str::to_owned)
            .into()
    );
    assert_eq!(
        client,
        ["hello", "sub", "pong", "unsub", "cmd"]
            .map(str::to_owned)
            .into()
    );
    let names: BTreeSet<_> = files.iter().map(|(name, _)| name.as_str()).collect();
    for required in [
        "clean",
        "handshake-deadline",
        "connect-deadline",
        "gap-snapshot",
        "empty-snapshot",
        "empty-bootstrap",
        "lagged",
        "heartbeat-missed",
        "protocol-refusal",
        "token-refused",
        "token-revoked",
        "version-gap",
        "uncertain-command",
        "subfail-cursor_ahead",
        "subfail-operator_only",
        "subfail-unknown_stream",
        "subfail-scope",
    ] {
        assert!(names.contains(required), "missing scenario {required}");
    }
}

#[test]
fn additive_fields_are_ignored_but_unknown_transport_tags_fail() {
    let client: ClientFrame =
        serde_json::from_value(json!({"t":"pong","nonce":"n","future":true})).unwrap();
    assert_eq!(
        serde_json::to_value(client).unwrap(),
        json!({"t":"pong","nonce":"n"})
    );
    let server: ServerFrame =
        serde_json::from_value(json!({"t":"ping","nonce":"n","future":true})).unwrap();
    assert_eq!(
        serde_json::to_value(server).unwrap(),
        json!({"t":"ping","nonce":"n"})
    );
    for value in [
        json!({"t":"future","ev":{}}),
        json!({"t":"unknown","raw":"x","shape":"key"}),
    ] {
        assert!(serde_json::from_value::<ClientFrame>(value.clone()).is_err());
        assert!(serde_json::from_value::<ServerFrame>(value).is_err());
    }
}

#[test]
fn unknown_agent_wrapper_preserves_raw_and_sender_shape() {
    let raw = " { \"type\": \"future\", \"counter\": 1e2 } ";
    let event = SessionEvent::parse(raw, "fixture-version");
    let SessionEvent::Unknown { raw, shape, .. } = event else {
        panic!("unknown fixture");
    };
    let payload = WireEvent::Unknown(UnknownEvent {
        r#type: UnknownTag::Unknown,
        raw: raw.clone(),
        shape: shape.0.clone(),
    });
    let json = serde_json::to_value(&payload).unwrap();
    assert_eq!(json, json!({"type":"unknown","raw":raw,"shape":shape.0}));
    assert_eq!(serde_json::from_value::<WireEvent>(json).unwrap(), payload);
}

#[test]
fn missing_fields_invalid_controls_and_bare_unknown_agent_payload_fail() {
    for value in [
        json!({"v":2,"step":"drop"}),
        json!({"v":1,"step":"sleep","ms":1}),
        json!({"v":1,"step":"drop","typo":true}),
        json!({"v":1,"step":"send","frame":{"t":"gap","sub_id":"s","from":1}}),
        json!({"v":1,"step":"token_refused","code":1000}),
        json!({"v":1,"step":"expect","frame":{"t":"pong","nonce":"n"},"fields":["typo"]}),
        json!({"v":1,"step":"send","frame":{"t":"ev","sub_id":"s","stream":"agent/a","seq":1,"ts_ms":0,"ev":{"type":"future"}}}),
        json!({"v":1,"step":"send","frame":{"t":"ev","sub_id":"s","stream":"agent/a/live","seq":1,"ts_ms":0,"ev":{"type":"assistant","message":{}}}}),
        json!({"v":1,"step":"send","frame":{"t":"ack","cmd_id":"c","ok":false,"result":{}}}),
        json!({"v":1,"step":"send","frame":{"t":"ack","cmd_id":"c","ok":true,"error":{"code":"scope","message":"x","exit":1,"retry":"never"}}}),
    ] {
        assert!(FixtureLine::parse(&value.to_string()).is_err(), "{value}");
    }
}

#[test]
fn seq_snapshot_and_live_forms_remain_distinct() {
    for (from, to, end) in [
        (1, 0, Some(0)),
        (3, 5, Some(5)),
        (3, 5, Some(7)),
        (3, 5, None),
    ] {
        let value = ServerFrame::Gap {
            sub_id: "s".into(),
            from,
            to,
            snapshot_to: end,
        };
        let json = serde_json::to_value(&value).unwrap();
        assert_eq!(json.get("snapshot_to").is_some(), end.is_some());
        assert_eq!(serde_json::from_value::<ServerFrame>(json).unwrap(), value);
    }
    let live = json!({"v":1,"step":"send","frame":{"t":"ev","sub_id":"s","stream":"agent/a/live","ts_ms":1,"ev":{"type":"assistant","message":{}}}});
    assert!(FixtureLine::parse(&live.to_string()).is_ok());
    let empty_ack: ServerFrame =
        serde_json::from_value(json!({"t":"ack","cmd_id":"c","ok":true})).unwrap();
    assert_eq!(
        serde_json::to_value(empty_ack).unwrap(),
        json!({"t":"ack","cmd_id":"c","ok":true})
    );
}

#[test]
fn settled_parameters_and_uncertain_ack_use_existing_exit_constant() {
    assert_eq!(WIRE_JSON_VERSION, 1);
    assert_eq!(
        (
            CONNECT_DEADLINE_MS,
            HANDSHAKE_DEADLINE_MS,
            HEARTBEAT_INTERVAL_MS,
            LIVENESS_WINDOW_MS
        ),
        (10_000, 5_000, 15_000, 45_000)
    );
    let files = fixtures();
    let (_, script) = files
        .iter()
        .find(|(name, _)| name == "uncertain-command")
        .unwrap();
    let line = script
        .lines()
        .find(|line| line.contains("session_unconfirmed"))
        .unwrap();
    let FixtureStep::Send {
        frame:
            ServerFrame::Ack {
                error: Some(error),
                ok: false,
                ..
            },
        ..
    } = FixtureLine::parse(line).unwrap().step
    else {
        panic!("uncertain ack");
    };
    assert_eq!(error.exit, crate::cli::exit::UNCONFIRMED);
    assert_eq!(error.code, SESSION_UNCONFIRMED);
    assert_eq!(error.retry, WireRetry::Later);
}

#[test]
fn explicit_null_results_and_details_round_trip_without_becoming_absent() {
    for value in [
        json!({"t":"ack","cmd_id":"c","ok":true,"result":null}),
        json!({"t":"ack","cmd_id":"c","ok":false,"error":{"code":"scope","message":"fixture","exit":crate::cli::exit::FAILURE,"retry":"never","data":null}}),
    ] {
        let frame: ServerFrame = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(frame).unwrap(), value);
    }
    for stage in [
        json!({"kind":"subscription","sub_id":"s"}),
        json!({"kind":"command_ack","cmd_id":"c"}),
    ] {
        let value = json!({"v":1,"step":"deadline","stage":stage});
        let parsed = FixtureLine::parse(&value.to_string()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), value);
    }
}

#[test]
fn every_omitted_script_field_is_optional_in_generated_types() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/client/src/gen");
    let fixture_ts = std::fs::read_to_string(root.join("FixtureLine.ts")).unwrap();
    let command_ts = std::fs::read_to_string(root.join("QueuedCommand.ts")).unwrap();
    fn check(raw: &Value, canonical: &Value, declaration: &str, location: &str) {
        for key in canonical.as_object().unwrap().keys() {
            if raw.get(key).is_none() {
                assert!(
                    optional_field(declaration, key),
                    "{location}: omitted {key} must be optional in TS"
                );
            }
        }
    }
    let mut scripts = fixtures();
    scripts.push((
        "omitted-observe-selectors".into(),
        r#"{"v":1,"step":"observe"}"#.into(),
    ));
    for (name, content) in scripts {
        for (index, line) in content.lines().enumerate() {
            let raw: Value = serde_json::from_str(line).unwrap();
            let canonical = serde_json::to_value(FixtureLine::parse(line).unwrap()).unwrap();
            let tag = format!("\"step\": \"{}\"", raw["step"].as_str().unwrap());
            let declaration = fixture_ts
                .split_once(&tag)
                .unwrap()
                .1
                .split(" | { \"step\":")
                .next()
                .unwrap();
            let location = format!("{name}:{}", index + 1);
            check(&raw, &canonical, declaration, &location);
            if raw["step"] == "given" {
                for (input, output) in raw["outbox"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .zip(canonical["outbox"].as_array().unwrap())
                {
                    check(input, output, &command_ts, &location);
                }
            }
        }
    }
}

fn optional_field(declaration: &str, key: &str) -> bool {
    declaration.contains(&format!("\n{key}?:"))
}

#[test]
fn optional_field_names_cannot_match_absence_selectors() {
    for key in ["streams", "outbox"] {
        assert!(!optional_field(
            &format!("\nabsent_{key}?: Array<string>,"),
            key
        ));
        assert!(optional_field(&format!("\n{key}?: Array<string>,"), key));
    }
}

#[test]
fn every_header_declares_coverage_and_minimal_scripts_use_only_minimal_operations() {
    let minimal = [
        "clean",
        "agent-gap",
        "agent-send",
        "ping-pong",
        "token-refused",
        "token-revoked",
    ];
    let files = fixtures();
    let mut seen = BTreeSet::new();
    for (name, content) in &files {
        let rows: Vec<Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let expected = if minimal.contains(&name.as_str()) {
            "s7-min"
        } else {
            "s7"
        };
        assert_eq!(rows[0]["covers"], json!([expected]), "{name}");
        if expected == "s7-min" {
            for row in rows {
                if let Some(tag) = row["frame"]["t"].as_str() {
                    seen.insert(tag.to_owned());
                    if tag == "sub" && row["frame"].get("after").is_some() {
                        seen.insert("cursor".into());
                    }
                    if tag == "ack" && row["frame"]["ok"] == true {
                        seen.insert("ack_ok".into());
                    }
                    if tag == "gap" {
                        assert!(
                            row["frame"].get("snapshot_to").is_none(),
                            "{name}: agent gaps have no entity snapshot"
                        );
                    }
                }
                if row["step"] == "token_refused" {
                    seen.insert(format!("close{}", row["code"]));
                }
                for payload in [&row["action"], &row["frame"]] {
                    if let Some(stream) = payload.get("stream").and_then(Value::as_str) {
                        assert!(
                            stream.starts_with("agent/") && !stream.ends_with("/live"),
                            "{name}: {stream}"
                        );
                    }
                    if let Some(verb) = payload.get("verb").and_then(Value::as_str) {
                        assert_eq!(verb, "agent.send", "{name}");
                    }
                }
            }
        }
    }
    for required in [
        "hello",
        "welcome",
        "sub",
        "cursor",
        "synced",
        "gap",
        "cmd",
        "ack_ok",
        "ping",
        "pong",
        "close4401",
        "close4403",
    ] {
        assert!(
            seen.contains(required),
            "missing minimal operation: {required}"
        );
    }
    for name in minimal {
        assert!(files.iter().any(|(found, _)| found == name), "{name}");
    }
}
