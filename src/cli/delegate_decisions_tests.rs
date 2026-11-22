//! Frozen unit tests for the delegate's destructive decisions (P578 r4-2).
//!
//! Conductor-written. Each decision is a pure function so it can be tested
//! without a server or a clock. The governing rule: when in doubt, destroy
//! nothing; missing evidence is never permission.

use super::*;
use serde_json::json;
use std::io;
use std::time::{Duration, Instant};

fn refused(code: &str) -> CreateFailure {
    CreateFailure::Refused(ServerError {
        code: code.to_string(),
        message: "refused".to_string(),
    })
}

// ------------------------------------------------------------ K2: leftovers

#[test]
fn k2_a_server_refusal_created_nothing_so_nothing_is_cleaned() {
    for listed_before in [true, false] {
        assert_eq!(
            leftover_check(&refused("worktree_create_failed"), listed_before),
            LeftoverCheck::Nothing
        );
    }
}

#[test]
fn k2_a_transport_failure_looks_for_a_leftover_only_with_a_before_list() {
    let failures = [
        CreateFailure::TimedOut,
        CreateFailure::Transport(io::ErrorKind::ConnectionReset),
        CreateFailure::Transport(io::ErrorKind::WouldBlock),
    ];
    for failure in &failures {
        assert_eq!(leftover_check(failure, true), LeftoverCheck::Look);
        assert_eq!(leftover_check(failure, false), LeftoverCheck::CannotCheck);
    }
}

// ------------------------------------------------------- K1: parent close

fn parent_record() -> serde_json::Value {
    json!({
        "workspace_id": "w2",
        "pane_count": 1,
        "worktree": {"checkout_path": "/repo", "is_linked_worktree": false, "repo_key": "k"}
    })
}

fn row(id: &str, linked: bool, key: &str) -> serde_json::Value {
    json!({
        "workspace_id": id,
        "worktree": {"is_linked_worktree": linked, "repo_key": key}
    })
}

#[test]
fn k1_the_parent_closes_only_with_all_evidence() {
    let others = [
        row("w2", false, "k"),
        row("w3", true, "k"),
        row("w4", false, "k"),
    ];
    // w3 is OUR delegate's workspace; it is not "another" linked worktree.
    assert!(parent_closable(
        Some(&parent_record()),
        Some("/repo"),
        Some(&others),
        "w3",
        Some("k")
    ));
}

#[test]
fn k1_a_failed_workspace_list_never_closes_the_parent() {
    assert!(!parent_closable(
        Some(&parent_record()),
        Some("/repo"),
        None,
        "w3",
        Some("k")
    ));
}

#[test]
fn k1_an_unresolved_parent_or_root_never_closes() {
    let others = [row("w2", false, "k")];
    assert!(!parent_closable(
        None,
        Some("/repo"),
        Some(&others),
        "w3",
        Some("k")
    ));
    assert!(!parent_closable(
        Some(&parent_record()),
        None,
        Some(&others),
        "w3",
        Some("k")
    ));
}

#[test]
fn k1_a_parent_with_another_checkout_root_or_pane_count_stays_open() {
    let others = [row("w2", false, "k")];
    let mut moved = parent_record();
    moved["worktree"]["checkout_path"] = json!("/elsewhere");
    assert!(!parent_closable(
        Some(&moved),
        Some("/repo"),
        Some(&others),
        "w3",
        Some("k")
    ));
    let mut busy = parent_record();
    busy["pane_count"] = json!(2);
    assert!(!parent_closable(
        Some(&busy),
        Some("/repo"),
        Some(&others),
        "w3",
        Some("k")
    ));
    let mut unknown = parent_record();
    unknown.as_object_mut().unwrap().remove("pane_count");
    assert!(!parent_closable(
        Some(&unknown),
        Some("/repo"),
        Some(&others),
        "w3",
        Some("k")
    ));
}

#[test]
fn k1_another_linked_worktree_of_the_repo_keeps_the_parent_open() {
    let others = [
        row("w2", false, "k"),
        row("w3", true, "k"),
        row("w9", true, "k"),
    ];
    assert!(!parent_closable(
        Some(&parent_record()),
        Some("/repo"),
        Some(&others),
        "w3",
        Some("k")
    ));
    // Without a recorded repo key, ANY other linked worktree keeps it open.
    let foreign = [row("w2", false, "k"), row("w9", true, "other")];
    assert!(!parent_closable(
        Some(&parent_record()),
        Some("/repo"),
        Some(&foreign),
        "w3",
        None
    ));
}

// ------------------------------------------------- K3: terminal identity

fn panes(ids: &[&str]) -> Vec<serde_json::Value> {
    ids.iter().map(|id| json!({"terminal_id": id})).collect()
}

#[test]
fn k3_a_recorded_terminal_in_the_workspace_is_positive_evidence() {
    let listed = panes(&["term_1", "term_2"]);
    assert!(holds_recorded_terminal(Some(&listed), &["term_2", ""]));
    assert!(holds_recorded_terminal(Some(&listed), &["", "term_1"]));
}

#[test]
fn k3_no_evidence_is_not_ours() {
    let listed = panes(&["term_1", ""]);
    // A failed pane list.
    assert!(!holds_recorded_terminal(None, &["term_1"]));
    // Nothing recorded, or only empty ids: an empty id matches nothing, not even an empty pane id.
    assert!(!holds_recorded_terminal(Some(&listed), &[]));
    assert!(!holds_recorded_terminal(Some(&listed), &["", ""]));
    // Recorded terminals that are not in the workspace.
    assert!(!holds_recorded_terminal(Some(&listed), &["term_9"]));
}

// -------------------------------------------------- K5: await failures

fn transport(kind: io::ErrorKind) -> BoundedError {
    BoundedError::Transport(io::Error::from(kind))
}

#[test]
fn k5_a_timed_out_request_inside_an_await_is_a_timeout() {
    let now = Instant::now();
    let later = now + Duration::from_secs(60);
    assert_eq!(
        await_failure(&BoundedError::TimedOut, Some(later), now),
        AwaitFailure::Timeout
    );
    assert_eq!(
        await_failure(&BoundedError::TimedOut, None, now),
        AwaitFailure::Timeout
    );
}

#[test]
fn k5_any_request_error_after_the_deadline_is_a_timeout() {
    let now = Instant::now();
    for kind in [
        io::ErrorKind::ConnectionReset,
        io::ErrorKind::WouldBlock,
        io::ErrorKind::TimedOut,
        io::ErrorKind::BrokenPipe,
    ] {
        assert_eq!(
            await_failure(&transport(kind), Some(now), now),
            AwaitFailure::Timeout
        );
    }
}

#[test]
fn k6b_only_a_slow_request_before_the_deadline_is_not_yet() {
    let now = Instant::now();
    let later = Some(now + Duration::from_secs(60));
    for kind in [io::ErrorKind::TimedOut, io::ErrorKind::WouldBlock] {
        assert_eq!(
            await_failure(&transport(kind), later, now),
            AwaitFailure::NotYet
        );
        assert_eq!(
            await_failure(&transport(kind), None, now),
            AwaitFailure::NotYet
        );
    }
    for kind in [io::ErrorKind::ConnectionRefused, io::ErrorKind::BrokenPipe] {
        assert!(matches!(
            await_failure(&transport(kind), later, now),
            AwaitFailure::Fail(_)
        ));
    }
}
