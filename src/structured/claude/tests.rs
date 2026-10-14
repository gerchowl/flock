use super::*;
use serde_json::json;

const FIXTURES: &str = include_str!("../../../tests/fixtures/claude-session/events.jsonl");
const VARIANTS: &str = include_str!("../../../tests/fixtures/claude-session/variants.txt");

#[test]
fn every_fixture_parses_to_its_expected_variant() {
    assert_eq!(FIXTURES.lines().count(), VARIANTS.lines().count());
    for (line, expected) in FIXTURES.lines().zip(VARIANTS.lines()) {
        let event = SessionEvent::parse(line, "2.1.296").unwrap();
        let actual = format!("{event:?}");
        assert!(actual.starts_with(&format!("{expected}(")), "{actual}");
        let via_serde: SessionEvent = serde_json::from_str(line).unwrap();
        assert_eq!(event, via_serde);
    }
}

#[test]
fn every_known_variant_round_trips_extra_and_nested_fields() {
    for line in FIXTURES.lines() {
        let mut raw: Value = serde_json::from_str(line).unwrap();
        raw["future_field"] = json!({"nested": [null, true, 7, "future"]});
        let event = SessionEvent::parse(&raw.to_string(), "2.1.296").unwrap();
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
        let event = SessionEvent::parse(&raw.to_string(), "2.1.296").unwrap();
        let SessionEvent::Unknown { raw: kept, shape } = &event else {
            panic!("unexpected known event: {event:?}");
        };
        assert_eq!(kept, &raw);
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
    let raw = json!({"type":"assistant", "message":{}});
    let extended = json!({"type":"assistant", "message":{}, "future":null});
    assert_ne!(
        ShapeKey::derive(&raw, "v"),
        ShapeKey::derive(&extended, "v")
    );
    let SessionEvent::Assistant(payload) = SessionEvent::parse(&extended.to_string(), "v").unwrap()
    else {
        panic!("extended assistant must stay known");
    };
    assert_eq!(
        payload.extra,
        json!({"future":null}).as_object().unwrap().clone()
    );
}

#[test]
fn malformed_json_is_a_typed_json_error() {
    for line in ["{", "not json", "", "{} {}"] {
        let error: serde_json::Error = SessionEvent::parse(line, "v").unwrap_err();
        assert!(error.is_syntax() || error.is_eof());
    }
}
