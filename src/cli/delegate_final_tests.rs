//! Frozen unit tests for the conductor's final fixes (P578 rr5: H1-H3).
//!
//! Conductor-written. Each defect from the final re-review is pinned through a
//! pure function, so the decision cannot drift from its test.

use super::*;
use serde_json::json;

fn server_error(code: &str) -> ServerError {
    ServerError {
        code: code.to_string(),
        message: "x".to_string(),
    }
}

const MISSING: &str = "/nonexistent/p578-h1/checkout";

// ------------------------------------------------------------ H1

#[test]
fn h1_a_timeout_or_transport_failure_is_never_already_gone() {
    // The checkout path is absent locally, but the request never got an answer:
    // nothing is known about the server side, so this is not "gone".
    for code in ["timeout", "transport"] {
        assert!(
            !is_already_gone(&server_error(code), Some(MISSING)),
            "{code} with a missing path must not read as gone"
        );
        assert!(!is_already_gone(&server_error(code), None));
    }
}

#[test]
fn h1_a_server_answer_with_the_path_gone_is_already_gone() {
    for code in ALREADY_GONE_CODES {
        assert!(is_already_gone(&server_error(code), None), "{code}");
    }
    // The server answered (some other refusal) and the checkout is not on disk.
    assert!(is_already_gone(
        &server_error("dirty_worktree_requires_force"),
        Some(MISSING)
    ));
    // The server answered, but the checkout is still there: not gone.
    assert!(!is_already_gone(
        &server_error("dirty_worktree_requires_force"),
        Some("/")
    ));
}

// ------------------------------------------------------------ H2

#[test]
fn h2_a_respelled_pre_existing_checkout_is_not_new() {
    let before = [json!({"path": "/repo/.worktrees/feat-x", "branch": "feat-x"})];
    for spelling in ["/repo/.worktrees/feat-x/", "/repo/./.worktrees/feat-x"] {
        assert!(
            !checkout_is_new(&before, spelling),
            "{spelling} is the operator's existing checkout"
        );
    }
    assert!(checkout_is_new(&before, "/repo/.worktrees/feat-y"));
    assert!(checkout_is_new(&[], "/repo/.worktrees/feat-x"));
}

// ------------------------------------------------------------ H3

#[test]
fn h3_no_result_with_a_gone_agent_is_gone_not_running() {
    assert_eq!(
        no_result_verdict(&AgentFetch::Missing),
        NoResultVerdict::Gone
    );
}

#[test]
fn h3_no_result_follows_the_live_status() {
    let found = |status: &str| AgentFetch::Found(json!({"agent_status": status}));
    assert_eq!(
        no_result_verdict(&found("working")),
        NoResultVerdict::Running
    );
    assert_eq!(
        no_result_verdict(&found("blocked")),
        NoResultVerdict::Running
    );
    assert_eq!(no_result_verdict(&found("idle")), NoResultVerdict::NoResult);
    assert_eq!(no_result_verdict(&found("done")), NoResultVerdict::NoResult);
    assert!(matches!(
        no_result_verdict(&AgentFetch::TimedOut),
        NoResultVerdict::Fail(_)
    ));
    assert!(matches!(
        no_result_verdict(&AgentFetch::Failed("dead socket".into())),
        NoResultVerdict::Fail(_)
    ));
}
