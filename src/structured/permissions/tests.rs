use super::*;

// Unknown handles deliberately use sequence zero, which the book never issues.
fn sequence(b: &Book, id: &str) -> u64 {
    b.open
        .get(id)
        .map(|r| r.request_seq)
        .or_else(|| {
            b.closed_ids
                .iter()
                .find_map(|(seq, closed)| (closed == id).then_some(*seq))
        })
        .unwrap_or(0)
}

fn request() -> ToolRequest {
    serde_json::from_value(json!({"subtype":"can_use_tool","tool_name":"Bash",
        "tool_use_id":"tool-example","input":{"command":"true"}}))
    .unwrap()
}
fn args(b: &Book, id: &str, decision: Decision) -> AgentDecide {
    AgentDecide {
        agent: "fixture-agent".into(),
        request_id: id.into(),
        request_seq: sequence(b, id),
        decision,
        message: Some("operator reason".into()),
    }
}
fn token() -> Principal<'static> {
    Principal::Token(crate::structured::permissions::auth::fixture(
        "fixture-client",
        true,
    ))
}
fn book() -> Book {
    let mut b = Book::new("fixture-agent".into());
    b.observe("request-example", request()).unwrap();
    b
}

#[test]
fn only_an_authenticated_operate_token_can_decide() {
    for principal in [
        Principal::Operator,
        Principal::Agent("fixture-agent"),
        Principal::Agent("other-agent"),
        Principal::Lease("fixture-agent"),
        Principal::Token(crate::structured::permissions::auth::fixture(
            "fixture-client",
            false,
        )),
        Principal::Token(crate::structured::permissions::auth::fixture("", true)),
    ] {
        let mut b = book();
        assert_eq!(
            b.prepare(
                &principal,
                &args(&b, "request-example", Decision::AllowOnce)
            )
            .err(),
            Some(ErrorCode::NotAuthorized)
        );
        assert_eq!(b.waiting(), vec![("request-example".into(), 1)]);
    }
    let mut b = book();
    assert!(b
        .prepare(&token(), &args(&b, "request-example", Decision::AllowOnce))
        .is_ok());
    let mut spoof = json!({"agent":"fixture-agent","request_id":"request-example","request_seq":1,"decision":"allow_once"});
    spoof["principal"] = json!({"token":"fixture-client"});
    assert!(serde_json::from_value::<AgentDecide>(spoof).is_err());
}

#[test]
fn unknown_and_closed_requests_are_distinct_and_never_retry() {
    let mut b = book();
    let missing = b
        .prepare(&token(), &args(&b, "unknown", Decision::Deny))
        .err()
        .unwrap();
    assert_eq!(missing, ErrorCode::PermissionRequestUnknown);
    assert_eq!(missing.retry(), crate::cli::exit::Retry::Never);
    let answer = b
        .prepare(&token(), &args(&b, "request-example", Decision::Deny))
        .unwrap();
    assert_eq!(
        b.prepare(&token(), &args(&b, "request-example", Decision::AllowOnce))
            .err(),
        Some(ErrorCode::PermissionRequestClosed)
    );
    assert!(b.begin_answer(&answer.id, answer.request_seq));
    b.audited(&answer);
    assert!(b.waiting().is_empty());
    assert!(
        b.blocked(),
        "stdin must receive the audited answer before input resumes"
    );
    b.written(answer);
    assert!(!b.blocked());
    let closed = b
        .prepare(&token(), &args(&b, "request-example", Decision::Deny))
        .err()
        .unwrap();
    assert_eq!(closed, ErrorCode::PermissionRequestClosed);
    assert_eq!(closed.retry(), crate::cli::exit::Retry::Never);
}

#[test]
fn lifecycle_and_duplicate_ids_fail_closed() {
    let mut b = book();
    assert_eq!(
        b.observe("request-example", request()).err(),
        Some(ErrorCode::InvalidRequest)
    );
    let mut wrong = args(&b, "request-example", Decision::Deny);
    wrong.agent = "other-agent".into();
    assert_eq!(
        b.prepare(&token(), &wrong).err(),
        Some(ErrorCode::AgentNotFound)
    );
    b.end();
    assert!(b.blocked());
    assert!(b.waiting().is_empty());
    assert_eq!(
        b.prepare(&token(), &args(&b, "request-example", Decision::Deny))
            .err(),
        Some(ErrorCode::PermissionRequestClosed)
    );
    assert_eq!(
        b.observe("new-request", request()).err(),
        Some(ErrorCode::InvalidRequest)
    );
}

#[test]
fn failed_admission_can_retry_and_requests_are_bounded() {
    let mut b = book();
    b.prepare(&token(), &args(&b, "request-example", Decision::Deny))
        .unwrap();
    b.aborted("request-example", sequence(&b, "request-example"));
    assert!(b
        .prepare(&token(), &args(&b, "request-example", Decision::Deny))
        .is_ok());
    for n in 1..MAX_OPEN_PERMISSION_REQUESTS {
        b.observe(&format!("request-{n}"), request()).unwrap();
    }
    assert_eq!(
        b.observe("overflow", request()).err(),
        Some(ErrorCode::PermissionRequestLimit)
    );
}

#[test]
fn session_grants_reserve_distinct_keys_and_abort_releases_capacity() {
    let mut b = Book::new("fixture-agent".into());
    for n in 0..MAX_GRANTS - 2 {
        let id = format!("grant-{n}");
        let mut r = request();
        r.input.insert("n".into(), json!(n));
        b.observe(&id, r).unwrap();
        let answer = b
            .prepare(&token(), &args(&b, &id, Decision::AllowAlways))
            .unwrap();
        assert!(b.begin_answer(&answer.id, answer.request_seq));
        b.audited(&answer);
        b.written(answer);
    }
    b.observe("deny", request()).unwrap();
    b.prepare(&token(), &args(&b, "deny", Decision::Deny))
        .unwrap();
    for id in ["same-a", "same-b"] {
        b.observe(id, request()).unwrap();
        b.prepare(&token(), &args(&b, id, Decision::AllowAlways))
            .unwrap();
    }
    let mut distinct = request();
    distinct.input.insert("n".into(), json!(100));
    b.observe("last-slot", distinct).unwrap();
    b.prepare(&token(), &args(&b, "last-slot", Decision::AllowAlways))
        .unwrap();
    let mut overflow = request();
    overflow.input.insert("n".into(), json!(101));
    b.observe("overflow", overflow).unwrap();
    assert_eq!(
        b.prepare(&token(), &args(&b, "overflow", Decision::AllowAlways))
            .err()
            .map(|code| code.as_str()),
        Some("permission_grant_limit")
    );
    b.aborted("last-slot", sequence(&b, "last-slot"));
    assert!(b
        .prepare(&token(), &args(&b, "overflow", Decision::AllowAlways))
        .is_ok());
}

#[test]
fn grant_is_installed_only_after_audit_and_cleared_on_runtime_end() {
    let mut b = book();
    let answer = b
        .prepare(
            &token(),
            &args(&b, "request-example", Decision::AllowAlways),
        )
        .unwrap();
    assert!(b.grants.is_empty());
    assert!(b.begin_answer(&answer.id, answer.request_seq));
    b.audited(&answer);
    assert_eq!(b.grants.len(), 1);
    b.end();
    assert!(b.grants.is_empty());
    b.audited(&answer);
    assert!(
        b.grants.is_empty(),
        "a late audit cannot resurrect a terminated runtime grant"
    );
}

#[test]
fn cancelled_grant_reservation_cannot_install_a_grant_after_its_close() {
    let mut b = book();
    let answer = b
        .prepare(
            &token(),
            &args(&b, "request-example", Decision::AllowAlways),
        )
        .unwrap();
    let closing = b
        .close(Some("request-example"), CloseReason::Cancelled)
        .unwrap();
    for close in closing {
        b.closed(&close.id, close.request_seq);
    }
    assert!(!b.begin_answer(&answer.id, answer.request_seq));
    b.audited(&answer);
    assert!(b.grants.is_empty());
    b.observe("following-request", request()).unwrap();
    assert!(b.automatic("following-request").unwrap().is_none());
}

#[test]
fn automatic_reservations_hold_input_without_requesting_human_attention() {
    let mut b = book();
    let grant = b
        .prepare(
            &token(),
            &args(&b, "request-example", Decision::AllowAlways),
        )
        .unwrap();
    assert!(b.begin_answer(&grant.id, grant.request_seq));
    b.audited(&grant);
    b.written(grant);
    b.observe("automatic-request", request()).unwrap();
    let answer = b.automatic("automatic-request").unwrap().unwrap();
    assert!(b.waiting().is_empty());
    assert!(
        b.blocked(),
        "grant reservation must still hold user input until delivery"
    );
    assert_eq!(
        serde_json::to_value(&answer.audit).unwrap()["grant_request_id"],
        "request-example"
    );
    b.aborted("automatic-request", sequence(&b, "automatic-request"));
    assert_eq!(
        b.waiting(),
        vec![(
            "automatic-request".into(),
            sequence(&b, "automatic-request")
        )]
    );
}

#[test]
fn exact_grants_preserve_numeric_representation_and_unicode() {
    let mut b = Book::new("fixture-agent".into());
    let mut original = request();
    original.input.insert("number".into(), json!(1));
    original.input.insert("text".into(), json!("é"));
    b.observe("grant", original.clone()).unwrap();
    let grant = b
        .prepare(&token(), &args(&b, "grant", Decision::AllowAlways))
        .unwrap();
    assert!(b.begin_answer(&grant.id, grant.request_seq));
    b.audited(&grant);
    b.written(grant);
    let mut float = original.clone();
    float.input.insert("number".into(), json!(1.0));
    let mut decomposed = original.clone();
    decomposed.input.insert("text".into(), json!("e\u{301}"));
    for (id, request) in [("float", float), ("decomposed", decomposed)] {
        b.observe(id, request).unwrap();
        assert!(b.automatic(id).unwrap().is_none(), "{id} must prompt again");
    }
    b.observe("exact", original).unwrap();
    assert!(b.automatic("exact").unwrap().is_some());
}

#[test]
fn closed_request_window_distinguishes_recent_evicted_and_unknown_ids() {
    let mut b = Book::new("fixture-agent".into());
    for n in 0..CLOSED_PERMISSION_REQUEST_WINDOW {
        let id = format!("request-{n}");
        b.observe(&id, request()).unwrap();
        b.closed(&id, sequence(&b, &id));
    }
    assert_eq!(
        b.prepare(&token(), &args(&b, "request-0", Decision::Deny))
            .err(),
        Some(ErrorCode::PermissionRequestClosed)
    );
    b.observe("next-request", request()).unwrap();
    b.closed("next-request", sequence(&b, "next-request"));
    for id in ["request-0", "never-seen"] {
        assert_eq!(
            b.prepare(&token(), &args(&b, id, Decision::Deny)).err(),
            Some(ErrorCode::PermissionRequestUnknown)
        );
    }
    assert_eq!(
        b.prepare(&token(), &args(&b, "request-1", Decision::Deny))
            .err(),
        Some(ErrorCode::PermissionRequestClosed)
    );
    assert_eq!(
        b.prepare(&token(), &args(&b, "next-request", Decision::Deny))
            .err(),
        Some(ErrorCode::PermissionRequestClosed)
    );
}

#[test]
fn closed_id_eviction_keeps_live_requests_and_uses_closure_order() {
    let mut b = Book::new("fixture-agent".into());
    b.observe("live", request()).unwrap();
    b.observe("first-opened", request()).unwrap();
    b.observe("second-opened", request()).unwrap();
    b.closed("second-opened", sequence(&b, "second-opened"));
    b.closed("first-opened", sequence(&b, "first-opened"));
    for n in 0..CLOSED_PERMISSION_REQUEST_WINDOW - 1 {
        let id = format!("retired-{n}");
        b.observe(&id, request()).unwrap();
        b.closed(&id, sequence(&b, &id));
    }
    assert_eq!(
        b.prepare(&token(), &args(&b, "second-opened", Decision::Deny))
            .err(),
        Some(ErrorCode::PermissionRequestUnknown)
    );
    assert_eq!(
        b.prepare(&token(), &args(&b, "first-opened", Decision::Deny))
            .err(),
        Some(ErrorCode::PermissionRequestClosed)
    );
    assert_eq!(b.waiting(), [("live".into(), 1)]);
    let answer = b
        .prepare(&token(), &args(&b, "live", Decision::AllowOnce))
        .unwrap();
    assert_eq!(
        answer.response["response"]["response"]["updatedInput"],
        json!(request().input)
    );
    assert_eq!(b.closed_ids.len(), CLOSED_PERMISSION_REQUEST_WINDOW);
    assert_eq!(b.closed_fifo.len(), CLOSED_PERMISSION_REQUEST_WINDOW);
}

#[test]
fn session_grant_survives_eviction_of_its_originating_request_id() {
    let mut b = Book::new("fixture-agent".into());
    b.observe("grant-origin", request()).unwrap();
    let answer = b
        .prepare(&token(), &args(&b, "grant-origin", Decision::AllowAlways))
        .unwrap();
    assert!(b.begin_answer(&answer.id, answer.request_seq));
    b.audited(&answer);
    b.written(answer);
    for n in 0..CLOSED_PERMISSION_REQUEST_WINDOW {
        let id = format!("retired-{n}");
        let mut other = request();
        other.input.insert("n".into(), json!(n));
        b.observe(&id, other).unwrap();
        b.closed(&id, sequence(&b, &id));
    }
    assert_eq!(
        b.prepare(&token(), &args(&b, "grant-origin", Decision::Deny))
            .err(),
        Some(ErrorCode::PermissionRequestUnknown)
    );
    b.observe("exact-followup", request()).unwrap();
    let automatic = b.automatic("exact-followup").unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&automatic.audit).unwrap()["grant_request_id"],
        "grant-origin"
    );
    assert_eq!(
        automatic.response["response"]["response"]["behavior"],
        "allow"
    );
}

#[test]
fn evicted_request_id_reuse_cannot_accept_a_stale_decision() {
    let mut b = book();
    let stale = args(&b, "request-example", Decision::AllowOnce);
    b.closed("request-example", sequence(&b, "request-example"));
    for n in 0..CLOSED_PERMISSION_REQUEST_WINDOW {
        let id = format!("retired-{n}");
        b.observe(&id, request()).unwrap();
        b.closed(&id, sequence(&b, &id));
    }
    b.observe("request-example", request()).unwrap();
    assert_eq!(
        b.prepare(&token(), &stale).err(),
        Some(ErrorCode::PermissionRequestUnknown)
    );
    assert!(b.grants.is_empty());
    assert!(b
        .prepare(&token(), &args(&b, "request-example", Decision::AllowOnce))
        .is_ok());
}

#[test]
fn evicted_request_id_reuse_cannot_install_a_stale_session_grant() {
    let mut b = book();
    let stale = args(&b, "request-example", Decision::AllowAlways);
    b.closed("request-example", sequence(&b, "request-example"));
    for n in 0..CLOSED_PERMISSION_REQUEST_WINDOW {
        let id = format!("retired-{n}");
        b.observe(&id, request()).unwrap();
        b.closed(&id, sequence(&b, &id));
    }
    b.observe("request-example", request()).unwrap();
    assert_eq!(
        b.prepare(&token(), &stale).err(),
        Some(ErrorCode::PermissionRequestUnknown)
    );
    assert!(b.open["request-example"].grant.is_none());
    assert!(
        b.grants.is_empty(),
        "a delayed decision must not create a grant"
    );
    b.observe("following-request", request()).unwrap();
    assert!(b.automatic("following-request").unwrap().is_none());
}

#[test]
fn decision_handles_are_required_and_closed_classification_matches_both_fields() {
    assert!(serde_json::from_value::<AgentDecide>(json!({
        "agent":"fixture-agent", "request_id":"request-example", "decision":"allow_once"
    }))
    .is_err());
    let mut b = book();
    let closed = args(&b, "request-example", Decision::Deny);
    b.closed(&closed.request_id, closed.request_seq);
    assert_eq!(
        b.prepare(&token(), &closed).err(),
        Some(ErrorCode::PermissionRequestClosed)
    );
    let mut wrong_id = closed;
    wrong_id.request_id = "never-seen".into();
    assert_eq!(
        b.prepare(&token(), &wrong_id).err(),
        Some(ErrorCode::PermissionRequestUnknown)
    );
}

#[test]
fn request_sequence_exhaustion_never_wraps_or_reopens_a_handle() {
    let mut b = Book::new("fixture-agent".into());
    b.resume_after(u64::MAX - 1);
    assert_eq!(b.observe("last", request()).unwrap().request_seq, u64::MAX);
    b.closed("last", u64::MAX);
    assert_eq!(
        b.observe("overflow", request()).err(),
        Some(ErrorCode::InvalidRequest)
    );
    assert!(b.waiting().is_empty());
    assert_eq!(
        b.capacity_refusal("overflow").err(),
        Some(ErrorCode::InvalidRequest)
    );
}

#[test]
fn queued_answer_for_an_evicted_handle_cannot_mutate_its_replacement() {
    let mut b = book();
    let stale = b
        .prepare(
            &token(),
            &args(&b, "request-example", Decision::AllowAlways),
        )
        .unwrap();
    b.closed(&stale.id, stale.request_seq);
    for n in 0..CLOSED_PERMISSION_REQUEST_WINDOW {
        let id = format!("retired-{n}");
        b.observe(&id, request()).unwrap();
        b.closed(&id, sequence(&b, &id));
    }
    b.observe("request-example", request()).unwrap();
    let current = b
        .prepare(&token(), &args(&b, "request-example", Decision::Deny))
        .unwrap();
    assert!(current.request_seq > stale.request_seq);
    assert!(!b.begin_answer(&stale.id, stale.request_seq));
    b.audited(&stale);
    assert!(b.grants.is_empty());
    b.written(stale);
    assert!(b.begin_answer(&current.id, current.request_seq));
    b.audited(&current);
    b.written(current);
    assert!(b.grants.is_empty());
    assert!(!b.blocked());
}

#[test]
fn typed_audits_preserve_existing_json_and_closed_without_open() {
    use crate::structured::claude::{KnownSessionEvent, SessionEvent};
    use crate::structured_wire::PermissionRecord;
    fn check(record: &PermissionRecord, expected: Value) {
        let value = serde_json::to_value(record).unwrap();
        assert_eq!(value, expected);
        let raw = value.to_string();
        assert!(!matches!(
            SessionEvent::parse(&raw, "fixture"),
            SessionEvent::Unknown { .. }
        ));
        let known: KnownSessionEvent = serde_json::from_str(&raw).unwrap();
        assert_eq!(serde_json::to_value(known).unwrap(), expected);
    }
    let mut b = Book::new("fixture-agent".into());
    check(
        &b.observe("p1", request()).unwrap(),
        json!({"type":"flk_permission_request","state":"open","request_id":"p1","request_seq":1,"request":request()}),
    );
    let answer = b
        .answer("p1", 1, Decision::Deny, None, "fixture-client", "token")
        .unwrap();
    check(
        &answer.audit,
        json!({"type":"flk_permission_request","state":"closed","request_id":"p1","request_seq":1,"decision":"deny","message":null,"principal":"fixture-client","source":"token"}),
    );
    for (reason, wire) in [
        (CloseReason::Ended, "ended"),
        (CloseReason::Cancelled, "cancelled"),
        (CloseReason::Result, "result"),
        (CloseReason::Interrupted, "interrupted"),
        (CloseReason::SessionEnded, "session_ended"),
    ] {
        let mut b = book();
        let closes = b.close(None, reason).unwrap();
        check(
            &closes[0].audit,
            json!({"type":"flk_permission_request","state":"closed","request_id":"request-example","request_seq":1,"reason":wire}),
        );
    }
    let mut b = Book::new("fixture-agent".into());
    check(
        &b.capacity_refusal("excess").unwrap().audit,
        json!({"type":"flk_permission_request","state":"closed","request_id":"excess","request_seq":1,"decision":"deny","message":"permission_request_limit","principal":"flk","source":"limit","reason":"permission_request_limit","error_code":"permission_request_limit"}),
    );
}

#[test]
fn future_permission_shapes_remain_lossless_unknown_events() {
    use crate::structured::claude::SessionEvent;
    let base = serde_json::to_value(open_record("p1", 1, &request())).unwrap();
    for (key, value) in [("future", json!(true)), ("state", json!("decided"))] {
        let mut raw = base.clone();
        raw[key] = value;
        let text = raw.to_string();
        let event = SessionEvent::parse(&text, "fixture");
        assert!(matches!(event, SessionEvent::Unknown { .. }));
        assert_eq!(serde_json::to_string(&event).unwrap(), text);
    }
    let raw = r#"{"type":"flk_permission_request","state":"closed","request_id":"p1","request_seq":1,"reason":"future"}"#;
    assert!(matches!(
        SessionEvent::parse(raw, "fixture"),
        SessionEvent::Unknown { .. }
    ));
}
