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
fn allow_always_is_explicitly_unsupported_without_closing() {
    let mut b = book();
    let code = b
        .prepare(&token(), &args("request-example", Decision::AllowAlways))
        .err()
        .unwrap();
    assert_eq!(code, ErrorCode::PermissionDecisionUnsupported);
    assert_eq!(
        ErrorCode::from_code("permission_decision_unsupported"),
        Some(code)
    );
    assert_eq!(code.retry(), crate::cli::exit::Retry::Never);
    assert_eq!(b.waiting(), vec!["request-example"]);
    assert!(b
        .prepare(&token(), &args("request-example", Decision::Deny))
        .is_ok());
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
    for n in 1..16 {
        b.observe(&format!("request-{n}"), request()).unwrap();
    }
    assert_eq!(
        b.observe("overflow", request()).err(),
        Some(ErrorCode::InvalidRequest)
    );
}
