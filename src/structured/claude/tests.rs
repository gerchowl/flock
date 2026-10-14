use super::*;
use serde_json::json;

const FIXTURES: &str = include_str!("../../../tests/fixtures/claude-session/events.jsonl");
const VARIANTS: &str = include_str!("../../../tests/fixtures/claude-session/variants.txt");

#[test]
fn every_fixture_parses_to_its_expected_variant() {
    assert_eq!(FIXTURES.lines().count(), VARIANTS.lines().count());
    for (line, expected) in FIXTURES.lines().zip(VARIANTS.lines()) {
        let event = SessionEvent::parse(line, "2.1.296");
        let actual = format!("{event:?}");
        assert!(actual.starts_with(&format!("{expected}(")), "{actual}");
        assert!(event.extra().unwrap().is_empty(), "{actual}");
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            serde_json::from_str::<Value>(line).unwrap()
        );
    }
}

#[test]
fn every_known_variant_round_trips_extra_and_nested_fields() {
    for line in FIXTURES.lines() {
        let mut raw: Value = serde_json::from_str(line).unwrap();
        raw["future_field"] = json!({"nested": [null, true, 7, "future"]});
        let event = SessionEvent::parse(&raw.to_string(), "2.1.296");
        assert!(!matches!(event, SessionEvent::Unknown { .. }));
        assert_eq!(serde_json::to_value(event).unwrap(), raw);
    }
}

#[test]
fn unknown_pairs_and_incompatible_payloads_preserve_raw() {
    for raw in [
        json!({"type":"future", "subtype":"new", "z":7, "a":true}),
        json!({"type":"system", "subtype":"future", "status":null}),
        json!({"type":"assistant", "subtype":"future", "message":{}}),
        json!({"type":"stream_event", "event":{"type":"future"}}),
        json!({"type":"control_request", "request":{"subtype":"future"}}),
        json!({"type":"control_response", "response":{"subtype":"future"}}),
        json!({"type":"result", "subtype":"future", "is_error":false}),
        json!({"type":"assistant", "message":42}),
        json!({"type":"result", "subtype":"success", "is_error":"false"}),
        json!({"type":"system", "subtype":42}),
        json!({"type":"system", "subtype":"status"}),
        json!({"type":42}),
        json!({}),
        json!([]),
        json!(null),
        json!(true),
        json!(42),
        json!("text"),
    ] {
        let event = SessionEvent::parse(&raw.to_string(), "2.1.296");
        let SessionEvent::Unknown {
            raw: kept,
            value: parsed,
            shape,
        } = &event
        else {
            panic!("unexpected known event: {event:?}");
        };
        assert_eq!(kept, &raw.to_string());
        assert_eq!(parsed, &raw);
        assert_eq!(shape, &ShapeKey::derive(&raw, "2.1.296"));
        assert_eq!(serde_json::to_value(event).unwrap(), raw);
    }
}

#[test]
fn shape_key_is_stable_sorted_versioned_and_unambiguous() {
    let raw = json!({"type":"future", "subtype":"new", "z":7, "a":true});
    let key = ShapeKey::derive(&raw, "2.1.296");
    assert_eq!(
        key.0,
        r#"["claude","2.1.296",["future","new",null,null,null],["a","subtype","type","z"]]"#
    );
    let reordered: Value =
        serde_json::from_str(r#"{"z":99,"type":"future","a":false,"subtype":"new"}"#).unwrap();
    assert_eq!(key, ShapeKey::derive(&reordered, "2.1.296"));
    assert_ne!(key, ShapeKey::derive(&raw, "2.1.297"));
    assert_ne!(
        key,
        ShapeKey::derive(
            &json!({"type":"future", "subtype":"other", "z":7, "a":true}),
            "2.1.296"
        )
    );
    assert_ne!(
        key,
        ShapeKey::derive(&json!({"type":"future", "subtype":"new", "z":7}), "2.1.296")
    );
    for path in ["event", "request", "response"] {
        let field = if path == "event" { "type" } else { "subtype" };
        let a = json!({"type":"future", path:{field:"a"}});
        let b = json!({"type":"future", path:{field:"b"}});
        assert_ne!(ShapeKey::derive(&a, "v"), ShapeKey::derive(&b, "v"));
    }
    assert_eq!(
        serde_json::from_value::<ShapeKey>(serde_json::to_value(&key).unwrap()).unwrap(),
        key
    );
}

#[test]
fn known_extra_changes_shape_without_losing_known_variant() {
    let raw = json!({"type":"assistant", "message":{}, "session_id":"session-example", "uuid":"event-example", "parent_tool_use_id":null, "request_id":"request-example"});
    assert!(SessionEvent::parse(&raw.to_string(), "v")
        .extra()
        .unwrap()
        .is_empty());
    let mut changed_value = raw.clone();
    changed_value["request_id"] = json!("other-request-example");
    assert_eq!(
        ShapeKey::derive(&raw, "v"),
        ShapeKey::derive(&changed_value, "v")
    );
    let mut extended = raw.clone();
    extended["future"] = Value::Null;
    assert_ne!(
        ShapeKey::derive(&raw, "v"),
        ShapeKey::derive(&extended, "v")
    );
    let SessionEvent::Assistant(payload) = SessionEvent::parse(&extended.to_string(), "v") else {
        panic!("extended assistant must stay known");
    };
    assert_eq!(
        payload.extra,
        json!({"future":null}).as_object().unwrap().clone()
    );
}

#[test]
fn non_json_lines_are_unknown_with_exact_raw_and_string_value() {
    for line in ["{", "not json", "", "{} {}", " \t diagnostic \r"] {
        let event = SessionEvent::parse(line, "2.1.296");
        let SessionEvent::Unknown { raw, value, shape } = &event else {
            panic!("expected Unknown");
        };
        assert_eq!(raw, line);
        assert_eq!(value, &Value::String(line.to_owned()));
        assert_eq!(shape, &ShapeKey::derive(value, "2.1.296"));
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            Value::String(line.to_owned())
        );
    }
}

#[test]
fn review_non_json_is_unknown_with_exact_raw() {
    let line = " \t diagnostic, not JSON \r";
    let event = SessionEvent::parse(line, "2.1.296");
    let SessionEvent::Unknown { raw, .. } = event else {
        panic!("expected Unknown")
    };
    assert_eq!(raw, line);
}

#[test]
fn review_unknown_json_retains_wire_bytes() {
    let line = " { \"z\" : 1.00e+2, \"type\" : \"future\", \"a\" : 0 } ";
    let event = SessionEvent::parse(line, "2.1.296");
    let SessionEvent::Unknown { raw, value, .. } = &event else {
        panic!("expected Unknown")
    };
    assert_eq!(raw, line);
    assert_ne!(value.to_string(), line);
    assert_eq!(serde_json::to_string(&event).unwrap(), line.trim());
}

#[test]
fn review_catalogue_envelope_is_not_extra() {
    let event = SessionEvent::parse(FIXTURES.lines().nth(3).unwrap(), "2.1.296");
    let SessionEvent::Init(payload) = event else {
        panic!("expected Init")
    };
    assert!(payload.extra.is_empty(), "{:?}", payload.extra);
}

#[test]
fn every_parse_path_requires_and_retains_the_supplied_version() {
    for line in [
        r#"{"type":"future"}"#,
        r#"{"type":"assistant","message":42}"#,
        "not JSON",
    ] {
        let a = SessionEvent::parse(line, "2.1.296");
        let b = SessionEvent::parse(line, "2.1.297");
        let (SessionEvent::Unknown { shape: a, .. }, SessionEvent::Unknown { shape: b, .. }) =
            (a, b)
        else {
            panic!("expected Unknown");
        };
        assert_ne!(a, b);
        let identity: Value = serde_json::from_str(&a.0).unwrap();
        assert_eq!(identity[1], "2.1.296");
    }
}

#[test]
fn optional_envelope_fields_preserve_absence_and_explicit_null() {
    for raw in [
        json!({"type":"assistant", "message":{}}),
        json!({"type":"assistant", "message":{}, "session_id":"session-example", "uuid":"event-example", "parent_tool_use_id":null, "timestamp":null}),
        json!({"type":"assistant", "message":{}, "parent_tool_use_id":"tool-example"}),
    ] {
        let event = SessionEvent::parse(&raw.to_string(), "2.1.296");
        assert!(event.extra().unwrap().is_empty());
        assert_eq!(serde_json::to_value(event).unwrap(), raw);
    }
}
