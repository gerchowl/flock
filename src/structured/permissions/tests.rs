use super::*;

fn request() -> ToolRequest {
    serde_json::from_value(json!({"subtype":"can_use_tool","tool_name":"Bash",
        "tool_use_id":"tool-example","input":{"command":"true"}}))
    .unwrap()
}
fn args(id: &str, decision: Decision) -> AgentDecide {
    AgentDecide {
        agent: "fixture-agent".into(),
        request_id: id.into(),
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
            b.prepare(&principal, &args("request-example", Decision::AllowOnce))
                .err(),
            Some(ErrorCode::NotAuthorized)
        );
        assert_eq!(b.waiting(), vec!["request-example"]);
    }
    assert!(book()
        .prepare(&token(), &args("request-example", Decision::AllowOnce))
        .is_ok());
    let mut spoof =
        json!({"agent":"fixture-agent","request_id":"request-example","decision":"allow_once"});
    spoof["principal"] = json!({"token":"fixture-client"});
    assert!(serde_json::from_value::<AgentDecide>(spoof).is_err());
}

#[test]
fn unknown_and_closed_requests_are_distinct_and_never_retry() {
    let mut b = book();
    let missing = b
        .prepare(&token(), &args("unknown", Decision::Deny))
        .err()
        .unwrap();
    assert_eq!(missing, ErrorCode::PermissionRequestUnknown);
    assert_eq!(missing.retry(), crate::cli::exit::Retry::Never);
    let answer = b
        .prepare(&token(), &args("request-example", Decision::Deny))
        .unwrap();
    assert_eq!(
        b.prepare(&token(), &args("request-example", Decision::AllowOnce))
            .err(),
        Some(ErrorCode::PermissionRequestClosed)
    );
    assert!(b.begin_answer(&answer.id));
    b.audited(&answer);
    assert!(b.waiting().is_empty());
    assert!(
        b.blocked(),
        "stdin must receive the audited answer before input resumes"
    );
    b.written(answer);
    assert!(!b.blocked());
    let closed = b
        .prepare(&token(), &args("request-example", Decision::Deny))
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
    let mut wrong = args("request-example", Decision::Deny);
    wrong.agent = "other-agent".into();
    assert_eq!(
        b.prepare(&token(), &wrong).err(),
        Some(ErrorCode::AgentNotFound)
    );
    b.end();
    assert!(b.blocked());
    assert!(b.waiting().is_empty());
    assert_eq!(
        b.prepare(&token(), &args("request-example", Decision::Deny))
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
    b.prepare(&token(), &args("request-example", Decision::Deny))
        .unwrap();
    b.aborted("request-example");
    assert!(b
        .prepare(&token(), &args("request-example", Decision::Deny))
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
            .prepare(&token(), &args(&id, Decision::AllowAlways))
            .unwrap();
        assert!(b.begin_answer(&answer.id));
        b.audited(&answer);
        b.written(answer);
    }
    b.observe("deny", request()).unwrap();
    b.prepare(&token(), &args("deny", Decision::Deny)).unwrap();
    for id in ["same-a", "same-b"] {
        b.observe(id, request()).unwrap();
        b.prepare(&token(), &args(id, Decision::AllowAlways))
            .unwrap();
    }
    let mut distinct = request();
    distinct.input.insert("n".into(), json!(100));
    b.observe("last-slot", distinct).unwrap();
    b.prepare(&token(), &args("last-slot", Decision::AllowAlways))
        .unwrap();
    let mut overflow = request();
    overflow.input.insert("n".into(), json!(101));
    b.observe("overflow", overflow).unwrap();
    assert_eq!(
        b.prepare(&token(), &args("overflow", Decision::AllowAlways))
            .err()
            .map(|code| code.as_str()),
        Some("permission_grant_limit")
    );
    b.aborted("last-slot");
    assert!(b
        .prepare(&token(), &args("overflow", Decision::AllowAlways))
        .is_ok());
}

#[test]
fn grant_is_installed_only_after_audit_and_cleared_on_runtime_end() {
    let mut b = book();
    let answer = b
        .prepare(&token(), &args("request-example", Decision::AllowAlways))
        .unwrap();
    assert!(b.grants.is_empty());
    assert!(b.begin_answer(&answer.id));
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
        .prepare(&token(), &args("request-example", Decision::AllowAlways))
        .unwrap();
    let closing = b
        .close(Some("request-example"), CloseReason::Cancelled)
        .unwrap();
    for close in closing {
        b.closed(&close.id);
    }
    assert!(!b.begin_answer(&answer.id));
    b.audited(&answer);
    assert!(b.grants.is_empty());
    b.observe("following-request", request()).unwrap();
    assert!(b.automatic("following-request").unwrap().is_none());
}

#[test]
fn automatic_reservations_hold_input_without_requesting_human_attention() {
    let mut b = book();
    let grant = b
        .prepare(&token(), &args("request-example", Decision::AllowAlways))
        .unwrap();
    assert!(b.begin_answer(&grant.id));
    b.audited(&grant);
    b.written(grant);
    b.observe("automatic-request", request()).unwrap();
    let answer = b.automatic("automatic-request").unwrap().unwrap();
    assert!(b.waiting().is_empty());
    assert!(
        b.blocked(),
        "grant reservation must still hold user input until delivery"
    );
    assert_eq!(answer.audit["grant_request_id"], "request-example");
    b.aborted("automatic-request");
    assert_eq!(b.waiting(), vec!["automatic-request"]);
}

#[test]
fn exact_grants_preserve_numeric_representation_and_unicode() {
    let mut b = Book::new("fixture-agent".into());
    let mut original = request();
    original.input.insert("number".into(), json!(1));
    original.input.insert("text".into(), json!("é"));
    b.observe("grant", original.clone()).unwrap();
    let grant = b
        .prepare(&token(), &args("grant", Decision::AllowAlways))
        .unwrap();
    assert!(b.begin_answer(&grant.id));
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
        b.closed(&id);
    }
    assert_eq!(
        b.prepare(&token(), &args("request-0", Decision::Deny))
            .err(),
        Some(ErrorCode::PermissionRequestClosed)
    );
    b.observe("next-request", request()).unwrap();
    b.closed("next-request");
    for id in ["request-0", "never-seen"] {
        assert_eq!(
            b.prepare(&token(), &args(id, Decision::Deny)).err(),
            Some(ErrorCode::PermissionRequestUnknown)
        );
    }
    assert_eq!(
        b.prepare(&token(), &args("request-1", Decision::Deny))
            .err(),
        Some(ErrorCode::PermissionRequestClosed)
    );
    assert_eq!(
        b.prepare(&token(), &args("next-request", Decision::Deny))
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
    b.closed("second-opened");
    b.closed("first-opened");
    for n in 0..CLOSED_PERMISSION_REQUEST_WINDOW - 1 {
        let id = format!("retired-{n}");
        b.observe(&id, request()).unwrap();
        b.closed(&id);
    }
    assert_eq!(
        b.prepare(&token(), &args("second-opened", Decision::Deny))
            .err(),
        Some(ErrorCode::PermissionRequestUnknown)
    );
    assert_eq!(
        b.prepare(&token(), &args("first-opened", Decision::Deny))
            .err(),
        Some(ErrorCode::PermissionRequestClosed)
    );
    assert_eq!(b.waiting(), ["live"]);
    let answer = b
        .prepare(&token(), &args("live", Decision::AllowOnce))
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
        .prepare(&token(), &args("grant-origin", Decision::AllowAlways))
        .unwrap();
    assert!(b.begin_answer(&answer.id));
    b.audited(&answer);
    b.written(answer);
    for n in 0..CLOSED_PERMISSION_REQUEST_WINDOW {
        let id = format!("retired-{n}");
        let mut other = request();
        other.input.insert("n".into(), json!(n));
        b.observe(&id, other).unwrap();
        b.closed(&id);
    }
    assert_eq!(
        b.prepare(&token(), &args("grant-origin", Decision::Deny))
            .err(),
        Some(ErrorCode::PermissionRequestUnknown)
    );
    b.observe("exact-followup", request()).unwrap();
    let automatic = b.automatic("exact-followup").unwrap().unwrap();
    assert_eq!(automatic.audit["grant_request_id"], "grant-origin");
    assert_eq!(
        automatic.response["response"]["response"]["behavior"],
        "allow"
    );
}
