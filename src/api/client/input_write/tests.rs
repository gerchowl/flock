use super::*;
use crate::test_support::ScriptedApi;
fn ping(capable: bool) -> Value {
    json!({"result":{"version":"fixture","protocol":crate::protocol::PROTOCOL_VERSION,
    "capabilities":{"pane_input_write_ack":capable}}})
}
fn raw(method: &str) -> Request {
    serde_json::from_value(json!({"id":"input","method":method,"params":{"pane_id":"p_fixture","text":"hello","keys":[]}})).unwrap()
}
#[test]
fn capable_client_binds_each_legacy_method_and_preserves_receipts() {
    for (legacy, bound) in [
        ("pane.send_text", "pane.write_text"),
        ("pane.send_keys", "pane.write_keys"),
        ("pane.send_input", "pane.write_input"),
    ] {
        let peer = ScriptedApi::new(vec![
            ("ping", Some(ping(true))),
            (bound, Some(json!({"result":{"type":"ok"}}))),
        ]);
        let result = peer.client().request_value(&raw(legacy)).unwrap();
        assert!(result.get("result").is_some());
        let calls = peer.finish();
        let limit = if bound == "pane.write_text" {
            7000
        } else {
            9000
        };
        assert!(calls[1]["params"]["write_timeout_ms"].as_u64().unwrap() < limit);
        assert_eq!(calls.len(), 2);
    }
}
#[test]
fn incapable_peer_is_refused_before_any_input() {
    let peer = ScriptedApi::new(vec![("ping", Some(ping(false)))]);
    let result = peer
        .client()
        .request_value(&raw("pane.send_input"))
        .unwrap();
    assert_eq!(result["error"]["code"], "server_version_gap");
    assert_eq!(
        crate::cli::error_code_exit("server_version_gap", false),
        crate::cli::exit::PROTOCOL_GAP
    );
    assert_eq!(peer.finish().len(), 1);
}
#[test]
fn stale_capability_cannot_downgrade_to_legacy_success() {
    let peer = ScriptedApi::new(vec![
        ("ping", Some(ping(true))),
        (
            "pane.write_input",
            Some(
                json!({"error":{"code":"invalid_request","message":"unknown variant `pane.write_input`"}}),
            ),
        ),
    ]);
    let result = peer
        .client()
        .request_value(&raw("pane.send_input"))
        .unwrap();
    assert_eq!(result["error"]["code"], "server_version_gap");
    assert_eq!(peer.finish().len(), 2);
}
#[test]
fn lost_reply_is_unknown_with_no_retirement_claim_or_retry() {
    let peer = ScriptedApi::new(vec![("ping", Some(ping(true))), ("pane.write_input", None)]);
    let result = peer
        .client()
        .request_value_retrying(&raw("pane.send_input"), CAP, None)
        .unwrap();
    assert_eq!(result["error"]["code"], "input_write_unknown");
    assert_eq!(
        result["error"]["data"]["input_write"],
        json!({"bytes_written":null,"bytes_total":null,
        "reason":"transport_lost","remainder_discarded":false})
    );
    assert_eq!(
        crate::cli::error_code_exit("input_write_unknown", false),
        crate::cli::exit::UNCONFIRMED
    );
    assert_eq!(peer.finish().len(), 2);
}
#[test]
fn authoritative_failure_keeps_exact_counts_and_is_never_retried() {
    let error = json!({"error":{"code":"input_write_failed","message":"inspect","data":{
        "input_write":{"bytes_written":2,"bytes_total":5,"reason":"writer_broken","remainder_discarded":true}}}});
    let peer = ScriptedApi::new(vec![
        ("ping", Some(ping(true))),
        ("pane.write_input", Some(error.clone())),
    ]);
    let result = peer
        .client()
        .request_value_retrying(&raw("pane.send_input"), CAP, None)
        .unwrap();
    assert_eq!(result["error"], error["error"]);
    assert_eq!(peer.finish().len(), 2);
}
#[test]
fn shorter_caller_budget_reserves_a_strict_reply_margin() {
    let peer = ScriptedApi::new(vec![
        ("ping", Some(ping(true))),
        ("pane.write_input", Some(json!({"result":{"type":"ok"}}))),
    ]);
    let result = peer
        .client()
        .request_value_retrying(
            &raw("pane.send_input"),
            CAP,
            Some(Instant::now() + Duration::from_millis(2500)),
        )
        .unwrap();
    assert!(result.get("result").is_some());
    let calls = peer.finish();
    assert!(calls[1]["params"]["write_timeout_ms"].as_u64().unwrap() < 1500);
    assert!(bind(&raw("pane.send_input"), Duration::from_millis(1001)).is_err());
    assert!(bind(&raw("pane.send_input"), Duration::from_millis(1002)).is_ok());
}
#[test]
fn guarded_submit_keeps_its_server_budget_and_refuses_short_callers() {
    let request: Request = serde_json::from_value(json!({"id":"guarded","method":"pane.submit", "params":{"pane_id":"p_fixture","text":"hello"}})).unwrap();
    assert!(bind(&request, Duration::from_secs(8)).is_err());
    assert!(bind(&request, CAP).is_err());
    assert_eq!(bind(&request, SUBMIT_CAP).unwrap().method, request.method);
    let peer = ScriptedApi::new(vec![("ping", Some(ping(true)))]);
    let result = peer
        .client()
        .request_value_with_timeout(&request, Duration::from_secs(7))
        .unwrap();
    assert_eq!(result["error"]["code"], "invalid_params");
    assert_eq!(peer.finish().len(), 1);
}
#[test]
fn capability_is_not_cached_by_socket_path() {
    let peer = ScriptedApi::new(vec![
        ("ping", Some(ping(true))),
        ("pane.write_input", Some(json!({"result":{"type":"ok"}}))),
        ("ping", Some(ping(false))),
    ]);
    let client = peer.client();
    assert!(client
        .request_value(&raw("pane.send_input"))
        .unwrap()
        .get("result")
        .is_some());
    assert_eq!(
        client.request_value(&raw("pane.send_input")).unwrap()["error"]["code"],
        "server_version_gap"
    );
    assert_eq!(peer.finish().len(), 3);
}

#[test]
fn raw_agent_send_resolves_without_using_the_legacy_input_method() {
    let peer = ScriptedApi::new(vec![
        ("ping", Some(ping(true))),
        ("pane.write_text", Some(json!({"result":{"type":"ok"}}))),
    ]);
    let request: Request = serde_json::from_value(json!({"id":"agent-input","method":"agent.send",
        "params":{"target":"fixture-agent","text":"hello","submit":false}}))
    .unwrap();
    assert!(peer
        .client()
        .request_value(&request)
        .unwrap()
        .get("result")
        .is_some());
    let calls = peer.finish();
    assert_eq!(calls[1]["params"]["pane_id"], "fixture-agent");
    assert!(calls[1]["params"]["write_timeout_ms"].as_u64().unwrap() < 7000);
}
#[test]
fn key_validation_is_not_mistaken_for_a_missing_method() {
    let error =
        json!({"error":{"code":"invalid_params","message":"unknown variant `pane.write_input`"}});
    let peer = ScriptedApi::new(vec![
        ("ping", Some(ping(true))),
        ("pane.write_input", Some(error.clone())),
    ]);
    let response = peer
        .client()
        .request_value(&raw("pane.send_input"))
        .unwrap();
    assert_eq!(response["error"], error["error"]);
    assert_eq!(peer.finish().len(), 2);
}

#[test]
fn invalid_reply_envelope_is_unknown_not_success() {
    for reply in [
        json!({}),
        json!("not an envelope"),
        json!({"id":"different-request","result":{"type":"ok"}}),
        json!({"result":{"type":"guarded_submit","outcome":"accepted","reason":null,"retried":false}}),
    ] {
        let peer = ScriptedApi::new(vec![
            ("ping", Some(ping(true))),
            ("pane.write_input", Some(reply)),
        ]);
        let response = peer
            .client()
            .request_value(&raw("pane.send_input"))
            .unwrap();
        assert_eq!(response["error"]["code"], "input_write_unknown");
        assert_eq!(peer.finish().len(), 2);
    }
}

#[test]
fn absent_capability_and_exhausted_budget_refuse_before_input() {
    let peer = ScriptedApi::new(vec![(
        "ping",
        Some(json!({"result":{"version":"old","protocol":crate::protocol::PROTOCOL_VERSION}})),
    )]);
    let response = peer
        .client()
        .request_value(&raw("pane.send_input"))
        .unwrap();
    assert_eq!(response["error"]["code"], "server_version_gap");
    assert_eq!(peer.finish().len(), 1);
    let peer = ScriptedApi::new(vec![]);
    let response = peer
        .client()
        .request_value_with_timeout(&raw("pane.send_input"), Duration::from_millis(1000))
        .unwrap();
    assert_eq!(response["error"]["code"], "invalid_params");
    assert!(peer.finish().is_empty());
}
#[test]
fn guarded_final_receipt_keeps_its_existing_envelope() {
    let result = json!({"result":{"type":"guarded_submit","outcome":"accepted","reason":null,"retried":false}});
    let peer = ScriptedApi::new(vec![
        ("ping", Some(ping(true))),
        ("pane.submit", Some(result.clone())),
    ]);
    let request: Request=serde_json::from_value(json!({"id":"guarded","method":"pane.submit","params":{"pane_id":"p_fixture","text":"hello"}})).unwrap();
    assert_eq!(
        peer.client().request_value(&request).unwrap()["result"],
        result["result"]
    );
    assert_eq!(peer.finish().len(), 2);
}

fn guarded_agent() -> Request {
    serde_json::from_value(json!({"id":"guarded-agent","method":"agent.send",
        "params":{"target":"fixture-agent","text":"hello","submit":true}}))
    .unwrap()
}

#[test]
fn explicit_write_range_errors_are_distinct_from_caller_budget_errors() {
    for timeout in [json!(0), json!(40000), json!(-1), json!(1.5), json!("10")] {
        let mut request = raw("pane.send_input");
        if let Method::PaneSendInput(p) = &mut request.method {
            p.write_timeout_ms = Some(timeout);
        }
        let error = bind(&request, CAP).unwrap_err();
        assert_eq!(error["error"]["code"], "invalid_params");
        assert_eq!(
            error["error"]["message"],
            "write_timeout_ms must be an integer in 1..=30000"
        );
    }
    let error = bind(&raw("pane.send_input"), Duration::from_millis(1001)).unwrap_err();
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .starts_with("caller budget"));
}

#[test]
fn agent_target_refusal_drops_pane_expected_but_keeps_other_metadata() {
    for data in [
        Some(json!({"expected":[{"type":"pane","pane":"<pane>"}],
            "candidates":[],"detail":"preserve"})),
        None,
    ] {
        let mut error = json!({"code":"pane_not_found","message":"missing pane"});
        if let Some(data) = &data {
            error["data"] = data.clone();
        }
        let peer = ScriptedApi::new(vec![
            ("ping", Some(ping(true))),
            ("pane.write_text", Some(json!({"error":error}))),
        ]);
        let request: Request = serde_json::from_value(json!({"id":"agent","method":"agent.send",
            "params":{"target":"missing","text":"hello","submit":false}}))
        .unwrap();
        let response = peer.client().request_value(&request).unwrap();
        assert_eq!(response["error"]["code"], "agent_not_found");
        if data.is_some() {
            assert!(response["error"]["data"].get("expected").is_none());
            assert_eq!(response["error"]["data"]["detail"], "preserve");
            assert_eq!(response["error"]["data"]["candidates"], json!([]));
        } else {
            assert!(response["error"].get("data").is_none());
        }
        assert_eq!(peer.finish().len(), 2);
    }
}

#[test]
fn guarded_agent_uses_the_server_derived_cap_and_preserves_its_method() {
    let request = guarded_agent();
    assert_eq!(cap(&request.method), SUBMIT_CAP);
    assert!(requires(&request.method));
    assert!(bind(&request, CAP).is_err());
    assert_eq!(bind(&request, SUBMIT_CAP).unwrap().method, request.method);
    let result = json!({"result":{"type":"guarded_submit","outcome":"accepted","reason":null,"retried":false}});
    let peer = ScriptedApi::new(vec![
        ("ping", Some(ping(true))),
        ("agent.send", Some(result.clone())),
    ]);
    assert_eq!(
        peer.client().request_value(&request).unwrap()["result"],
        result["result"]
    );
    let calls = peer.finish();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1]["params"]["target"], "fixture-agent");
    assert_eq!(calls[1]["params"]["submit"], true);
    assert!(calls[1]["params"].get("pane_id").is_none());
}

#[test]
fn guarded_agent_refuses_a_short_budget_before_dispatch() {
    let peer = ScriptedApi::new(vec![("ping", Some(ping(true)))]);
    let response = peer
        .client()
        .request_value_with_timeout(&guarded_agent(), Duration::from_secs(7))
        .unwrap();
    assert_eq!(response["error"]["code"], "invalid_params");
    assert_eq!(peer.finish().len(), 1);
}

#[test]
fn guarded_agent_lost_reply_is_unknown_and_never_replayed() {
    let peer = ScriptedApi::new(vec![("ping", Some(ping(true))), ("agent.send", None)]);
    let response = peer
        .client()
        .request_value_retrying(&guarded_agent(), SUBMIT_CAP, None)
        .unwrap();
    assert_eq!(response["error"]["code"], "input_write_unknown");
    assert_eq!(peer.finish().len(), 2);
}

struct GatedInvocation;
impl GatedInvocation {
    fn begin(client: &ApiClient) -> Self {
        crate::api::compatibility::begin_cli(client, true, |_| {});
        Self
    }
}
impl Drop for GatedInvocation {
    fn drop(&mut self) {
        crate::api::compatibility::end_cli();
    }
}

#[test]
fn gated_input_uses_one_fresh_probe_for_version_and_capability() {
    let peer = ScriptedApi::new(vec![
        ("ping", Some(ping(true))),
        ("pane.write_input", Some(json!({"result":{"type":"ok"}}))),
    ]);
    let client = peer.client();
    let _invocation = GatedInvocation::begin(&client);
    assert!(client
        .request_value(&raw("pane.send_input"))
        .unwrap()
        .get("result")
        .is_some());
    assert_eq!(
        peer.finish()
            .iter()
            .filter(|r| r["method"] == "ping")
            .count(),
        1
    );
}

#[test]
fn gated_exhausted_budget_does_not_connect_for_a_version_probe() {
    let directory = crate::test_support::TempDir::socket();
    let socket = directory.path().join("unserved.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let client = ApiClient::for_target(ConnectionTarget::SocketPath(socket));
    let _invocation = GatedInvocation::begin(&client);
    let response = client
        .request_value_with_timeout(&raw("pane.send_input"), Duration::ZERO)
        .unwrap();
    assert_eq!(response["error"]["code"], "invalid_params");
    assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
}

#[test]
fn guarded_agent_requires_capability_and_normalizes_an_unknown_method() {
    for capable in [false, true] {
        let mut script = vec![("ping", Some(ping(capable)))];
        if capable {
            script.push((
                "agent.send",
                Some(json!({"error":{
                "code":"unknown_method","message":"agent.send is unavailable"}})),
            ));
        }
        let peer = ScriptedApi::new(script);
        let response = peer.client().request_value(&guarded_agent()).unwrap();
        assert_eq!(response["error"]["code"], "server_version_gap");
        assert_eq!(peer.finish().len(), if capable { 2 } else { 1 });
    }
}
