use super::*;
use fixture::FixtureLine;
use serde_json::json;

#[test]
fn every_registered_and_future_command_error_decodes_without_reconnect() {
    for code in crate::api::error_code::ErrorCode::ALL
        .iter()
        .map(|code| code.as_str())
        .chain(["future_wire_error"])
    {
        let frame = json!({"t":"ack","cmd_id":"fixture","ok":false,"error":{
            "code":code,"message":"fixture refusal","exit":crate::cli::exit::FAILURE,"retry":"never"
        }});
        let decoded: ServerFrame =
            serde_json::from_value(frame.clone()).unwrap_or_else(|err| panic!("{code}: {err}"));
        assert_eq!(serde_json::to_value(decoded).unwrap(), frame);
    }
}

#[test]
fn malformed_known_and_bare_future_payloads_have_no_opaque_fallback() {
    for raw in [
        json!({"type":"assistant","message":false}),
        json!({"type":"future"}),
    ] {
        let line = json!({"v":1,"step":"send","frame":{"t":"ev","sub_id":"s","stream":"agent/a","seq":1,"ts_ms":0,"ev":raw}});
        assert!(FixtureLine::parse(&line.to_string()).is_err(), "{raw}");
    }
}

#[test]
fn generated_wire_payload_has_a_closed_known_projection() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/client/src/gen");
    let wire = std::fs::read_to_string(root.join("WireEvent.ts")).unwrap();
    assert!(wire.contains("KnownSessionEvent"), "{wire}");
    assert!(wire.contains("JsonValue"), "{wire}");
    let known = std::fs::read_to_string(root.join("KnownSessionEvent.ts")).unwrap();
    assert!(!known.contains(" | unknown"), "{known}");
}

macro_rules! syntax {
    ($name:ident, $value:expr) => {
        #[test]
        fn $name() {
            let raw = $value;
            let line = FixtureLine::parse(&raw.to_string()).unwrap();
            assert_eq!(
                FixtureLine::parse(&serde_json::to_string(&line).unwrap()).unwrap(),
                line
            );
        }
    };
}
syntax!(
    given_is_explicit,
    json!({"v":1,"step":"given","cursors":{"fleet":2},"outbox":[]})
);
syntax!(
    client_action_is_separate_from_expectation,
    json!({"v":1,"step":"client","action":{"kind":"subscribe","sub_id":"s1","stream":"fleet"}})
);
syntax!(
    observe_carries_client_and_stream_state,
    json!({"v":1,"step":"observe","connection":"Live","streams":{"s1":{"state":"Current","cursor":2}},"outbox":{"c1":"pending"}})
);
syntax!(
    expect_can_require_absence,
    json!({"v":1,"step":"expect","frame":{"t":"sub","sub_id":"s1","stream":"fleet"},"fields":["sub_id","stream"],"absent":["after"]})
);
syntax!(
    quiet_has_an_explicit_virtual_ceiling,
    json!({"v":1,"step":"quiet","ceiling_ms":1000})
);
syntax!(
    reconnect_is_an_expectation_not_a_stimulus,
    json!({"v":1,"step":"expect_connect","within_ms":500})
);
syntax!(
    abandoned_socket_can_deliver_a_frame,
    json!({"v":1,"step":"send","socket":"previous","frame":{"t":"ping","nonce":"old"}})
);

#[test]
fn wire_error_constants_match_registered_and_subscription_spellings() {
    use crate::api::error_code::ErrorCode;
    assert_eq!(ErrorCode::InvalidAgent.retry().as_str(), "never");
    assert_eq!(CURSOR_AHEAD, ErrorCode::CursorAhead.as_str());
    assert_eq!(OPERATOR_ONLY, ErrorCode::OperatorOnly.as_str());
    assert_eq!(SESSION_UNCONFIRMED, ErrorCode::SessionUnconfirmed.as_str());
    for (code, spelling) in [
        (SubfailCode::InvalidAgent, ErrorCode::InvalidAgent.as_str()),
        (SubfailCode::CursorAhead, CURSOR_AHEAD),
        (SubfailCode::OperatorOnly, OPERATOR_ONLY),
        (SubfailCode::UnknownStream, UNKNOWN_STREAM),
        (SubfailCode::Scope, SCOPE),
        (SubfailCode::SubIdReused, SUB_ID_REUSED),
    ] {
        assert_eq!(serde_json::to_value(&code).unwrap(), spelling);
        let frame = json!({"t":"subfail","sub_id":"s1","code":spelling,"message":"Refused"});
        assert_eq!(
            serde_json::to_value(serde_json::from_value::<ServerFrame>(frame.clone()).unwrap())
                .unwrap(),
            frame
        );
    }
    // S7 registers the new auth/scope/stream codes when it starts emitting them.
    let ack = json!({"t":"ack","cmd_id":"c","ok":false,"error":{
        "code":AUTH_REVOKED,"message":"revoked","exit":crate::cli::exit::FAILURE,"retry":"after_human"}});
    assert_eq!(
        serde_json::to_value(serde_json::from_value::<ServerFrame>(ack.clone()).unwrap()).unwrap(),
        ack
    );
}

#[test]
fn wire_retry_matches_the_complete_cli_registry() {
    use crate::cli::exit::Retry;
    fn wire(retry: Retry) -> WireRetry {
        match retry {
            Retry::NotApplicable => WireRetry::NotApplicable,
            Retry::Never => WireRetry::Never,
            Retry::Maybe => WireRetry::Maybe,
            Retry::Yes => WireRetry::Yes,
            Retry::Later => WireRetry::Later,
            Retry::AfterInspecting => WireRetry::AfterInspecting,
            Retry::AfterHuman => WireRetry::AfterHuman,
            Retry::AfterHandoff => WireRetry::AfterHandoff,
        }
    }
    fn cli(retry: WireRetry) -> Retry {
        match retry {
            WireRetry::NotApplicable => Retry::NotApplicable,
            WireRetry::Never => Retry::Never,
            WireRetry::Maybe => Retry::Maybe,
            WireRetry::Yes => Retry::Yes,
            WireRetry::Later => Retry::Later,
            WireRetry::AfterInspecting => Retry::AfterInspecting,
            WireRetry::AfterHuman => Retry::AfterHuman,
            WireRetry::AfterHandoff => Retry::AfterHandoff,
        }
    }
    for exit in crate::cli::exit::ALL {
        let expected = exit.retry.as_str();
        assert_eq!(serde_json::to_value(wire(exit.retry)).unwrap(), expected);
        assert_eq!(
            cli(serde_json::from_value(json!(expected)).unwrap()),
            exit.retry
        );
    }
}

#[test]
fn unknown_wrapper_serde_and_ts_shape_are_exact() {
    let value = json!({"type":"unknown","raw":"  diagnostic 1e2 ","shape":"sender-key"});
    let parsed: UnknownEvent = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), value);
    assert!(
        serde_json::from_value::<UnknownEvent>(json!({"type":"future","raw":"x","shape":"s"}))
            .is_err()
    );
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/client/src/gen");
    let wrapper = std::fs::read_to_string(root.join("UnknownEvent.ts")).unwrap();
    let wrapper: String = wrapper.chars().filter(|ch| !ch.is_whitespace()).collect();
    let tag = std::fs::read_to_string(root.join("UnknownTag.ts")).unwrap();
    assert!(
        wrapper.contains("type:UnknownTag,")
            && wrapper.contains("raw:string,")
            && wrapper.contains("shape:string,"),
        "{wrapper}"
    );
    assert!(tag.contains(r#"type UnknownTag = "unknown";"#), "{tag}");
}

#[test]
fn negative_assertions_reject_typos_and_scripts_require_final_quiet() {
    for value in [
        json!({"v":1,"step":"close","code":4401,"reason":"use token_refused"}),
        json!({"v":1,"step":"close","code":4403,"reason":"use token_refused"}),
        json!({"v":1,"step":"quiet","ceiling_ms":0}),
        json!({"v":1,"step":"expect_connect","within_ms":0}),
        json!({"v":1,"step":"client","action":{"kind":"connect","typo":true}}),
        json!({"v":1,"step":"expect","frame":{"t":"sub","sub_id":"s","stream":"fleet"},"fields":[],"absent":["afetr"]}),
        json!({"v":1,"step":"expect","frame":{"t":"sub","sub_id":"s","stream":"fleet","after":0},"fields":[],"absent":["after"]}),
    ] {
        assert!(FixtureLine::parse(&value.to_string()).is_err(), "{value}");
    }
    let given = r#"{"v":1,"step":"given","cursors":{},"outbox":[]}"#;
    let quiet = r#"{"v":1,"step":"quiet","ceiling_ms":1}"#;
    assert!(FixtureLine::parse_script(given).is_err());
    assert!(FixtureLine::parse_script(quiet).is_err());
    assert!(FixtureLine::parse_script(&format!("{given}\n{given}\n{quiet}")).is_err());
    assert!(FixtureLine::parse_script(&format!("{given}\n{quiet}")).is_ok());
}

#[test]
fn recovery_timing_constants_match_adr() {
    assert_eq!(
        (SUBSCRIPTION_DEADLINE_MS, ACK_DEADLINE_MS),
        (30_000, 30_000)
    );
    assert_eq!(
        (
            BACKOFF_BASE_MS,
            BACKOFF_FACTOR,
            BACKOFF_CAP_MS,
            BACKOFF_JITTER_DIVISOR,
            BACKOFF_RESET_MS
        ),
        (500, 2, 30_000, 2, 60_000)
    );
}
