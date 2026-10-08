//! Call-site tests for the inventory-driven decisions (P578 r4-3, W7).
//!
//! The frozen pure-function tests prove the DECISIONS; these prove the WIRING
//! around them: that the right inventory requests are made, that failures
//! stop before anything destructive, and that `require_delegate` maps every
//! `AgentFetch` to the right exit path. The decision/callsite split keeps
//! each function's tests independent of the socket.

use super::*;
use serde_json::json;
use std::cell::RefCell;
use std::io;
use std::rc::Rc;

// ---------- fixtures ---------------------------------------------------------

fn start_flags_worktree(branch: &str) -> StartFlags {
    StartFlags {
        branch: Some(branch.to_string()),
        worktree: true,
        ..StartFlags::default()
    }
}

fn start_flags_cwd(cwd: &str) -> StartFlags {
    StartFlags {
        cwd: Some(cwd.to_string()),
        ..StartFlags::default()
    }
}

fn worktree_row(path: &str, branch: &str) -> serde_json::Value {
    json!({"path": path, "branch": branch})
}

fn ok_response() -> serde_json::Value {
    json!({"result": {}})
}

fn cleanup_cwd() -> Cleanup {
    Cleanup {
        name: "d1".to_string(),
        workspace_id: "w_delegate".to_string(),
        pane_id: "p".to_string(),
        root_pane: "rp".to_string(),
        root_pane_terminal_id: "rt".to_string(),
        terminal_id: "t".to_string(),
        mode: Mode::Cwd,
        worktree: None,
        parent_workspace_id: Some("w_parent".to_string()),
        repo_root: Some("/repo".to_string()),
        repo_key: Some("k".to_string()),
    }
}

/// Capture of what a fake `kill_path` closure saw.
#[derive(Default)]
struct KillLog {
    calls: Vec<String>,
}

type Log = Rc<RefCell<KillLog>>;

fn record_kill(log: &Log, path: &str) -> Result<serde_json::Value, ServerError> {
    log.borrow_mut().calls.push(path.to_string());
    Ok(ok_response())
}

// ---------- teardown_after_place_failure_with --------------------------------

#[test]
fn w7_place_failure_a_failed_post_list_kills_nothing() {
    let log: Log = Rc::new(RefCell::new(KillLog::default()));
    let log_clone = log.clone();
    let before = vec![worktree_row("/repo/x", "feat/x")];
    let exit = teardown_after_place_failure_with(
        "d1",
        Some(&before),
        Mode::Worktree,
        &start_flags_worktree("feat/x"),
        CreateFailure::TimedOut,
        |_deadline| None, // The post-create list FAILED.
        |path| record_kill(&log_clone, path),
    );
    assert_eq!(exit, 1);
    assert!(
        log.borrow().calls.is_empty(),
        "a failed post-create list is not permission to kill anything: {:?}",
        log.borrow().calls
    );
}

#[test]
fn w7_place_failure_look_with_a_new_checkout_kills_exactly_that_path() {
    let log: Log = Rc::new(RefCell::new(KillLog::default()));
    let log_clone = log.clone();
    let before = vec![worktree_row("/repo/existing", "feat/other")];
    let after = vec![
        worktree_row("/repo/existing", "feat/other"),
        worktree_row("/repo/stray", "feat/x"),
    ];
    let exit = teardown_after_place_failure_with(
        "d1",
        Some(&before),
        Mode::Worktree,
        &start_flags_worktree("feat/x"),
        CreateFailure::TimedOut,
        |_deadline| Some(after.clone()),
        |path| record_kill(&log_clone, path),
    );
    assert_eq!(exit, 1);
    assert_eq!(log.borrow().calls, vec!["/repo/stray".to_string()]);
}

#[test]
fn w7_place_failure_look_with_no_new_checkout_kills_nothing() {
    let log: Log = Rc::new(RefCell::new(KillLog::default()));
    let log_clone = log.clone();
    let before = vec![worktree_row("/repo/a", "feat/a")];
    let after = before.clone();
    let exit = teardown_after_place_failure_with(
        "d1",
        Some(&before),
        Mode::Worktree,
        &start_flags_worktree("feat/x"),
        CreateFailure::TimedOut,
        |_deadline| Some(after.clone()),
        |path| record_kill(&log_clone, path),
    );
    assert_eq!(exit, 1);
    assert!(log.borrow().calls.is_empty());
}

#[test]
fn w7_place_failure_a_refusal_never_lists() {
    let listed: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
    let listed_clone = listed.clone();
    let log: Log = Rc::new(RefCell::new(KillLog::default()));
    let log_clone = log.clone();
    let before = vec![worktree_row("/repo/a", "feat/a")];
    let refused = CreateFailure::Refused(ServerError {
        code: "worktree_create_failed".to_string(),
        message: "branch already exists".to_string(),
    });
    let exit = teardown_after_place_failure_with(
        "d1",
        Some(&before),
        Mode::Worktree,
        &start_flags_worktree("feat/x"),
        refused,
        |_deadline| {
            *listed_clone.borrow_mut() = true;
            Some(Vec::new())
        },
        |path| record_kill(&log_clone, path),
    );
    assert_eq!(exit, 1);
    assert!(
        !*listed.borrow(),
        "a Refused failure must not even LIST — leftover_check says Nothing"
    );
    assert!(log.borrow().calls.is_empty());
}

#[test]
fn w7_place_failure_cwd_transport_prints_and_never_lists() {
    let listed: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
    let listed_clone = listed.clone();
    let log: Log = Rc::new(RefCell::new(KillLog::default()));
    let log_clone = log.clone();
    let exit = teardown_after_place_failure_with(
        "d1",
        None,
        Mode::Cwd,
        &start_flags_cwd("/some/cwd"),
        CreateFailure::Transport(io::ErrorKind::ConnectionReset),
        |_deadline| {
            *listed_clone.borrow_mut() = true;
            Some(Vec::new())
        },
        |path| record_kill(&log_clone, path),
    );
    assert_eq!(exit, 1);
    assert!(
        !*listed.borrow(),
        "cwd mode has nothing to list for; a transport failure never asks"
    );
    assert!(log.borrow().calls.is_empty());
}

// ---------- parent_to_close_with ---------------------------------------------

fn target_with_parent() -> Cleanup {
    cleanup_cwd()
}

fn parent_record() -> serde_json::Value {
    json!({
        "workspace_id": "w_parent",
        "pane_count": 1,
        "worktree": {"checkout_path": "/repo", "is_linked_worktree": false, "repo_key": "k"}
    })
}

fn list_rows() -> Vec<serde_json::Value> {
    vec![
        json!({"workspace_id": "w_parent", "worktree": {"is_linked_worktree": false, "repo_key": "k"}}),
        json!({"workspace_id": "w_delegate", "worktree": {"is_linked_worktree": true, "repo_key": "k"}}),
    ]
}

#[test]
fn w7_parent_a_failed_list_returns_none() {
    let target = target_with_parent();
    let out = parent_to_close_with(&target, |_id| Some(parent_record()), || None).expect("no err");
    assert!(out.is_none());
}

#[test]
fn w7_parent_a_failed_record_returns_none() {
    let target = target_with_parent();
    let out = parent_to_close_with(&target, |_id| None, || Some(list_rows())).expect("no err");
    assert!(out.is_none());
}

#[test]
fn w7_parent_all_evidence_present_returns_some() {
    let target = target_with_parent();
    let out = parent_to_close_with(&target, |_id| Some(parent_record()), || Some(list_rows()))
        .expect("no err");
    assert_eq!(out.as_deref(), Some("w_parent"));
}

// ---------- parent_closable: malformed-row rules (W6) ------------------------

#[test]
fn w6_parent_closable_a_row_with_no_workspace_id_keeps_parent_open() {
    let others = [
        json!({"worktree": {"is_linked_worktree": false, "repo_key": "k"}}), // no id
    ];
    assert!(!parent_closable(
        Some(&parent_record()),
        Some("/repo"),
        Some(&others),
        "w_delegate",
        Some("k"),
    ));
}

#[test]
fn w6_parent_closable_a_row_without_boolean_is_linked_keeps_parent_open() {
    // A `worktree` block IS present but `is_linked_worktree` isn't a boolean:
    // the row CLAIMS a worktree but its linkage is malformed. Refuse to
    // close the parent on the strength of that row.
    let others = [
        json!({"workspace_id": "w_other", "worktree": {"is_linked_worktree": "yes", "repo_key": "k"}}),
    ];
    assert!(!parent_closable(
        Some(&parent_record()),
        Some("/repo"),
        Some(&others),
        "w_delegate",
        Some("k"),
    ));
    let missing = [json!({"workspace_id": "w_other", "worktree": {"repo_key": "k"}})];
    assert!(!parent_closable(
        Some(&parent_record()),
        Some("/repo"),
        Some(&missing),
        "w_delegate",
        Some("k"),
    ));
}

#[test]
fn w6_parent_closable_an_operator_workspace_with_no_worktree_block_does_not_block() {
    // A legitimate non-worktree workspace has `worktree: None` and is simply
    // not linked — the parent close may still proceed. This is what a19 and
    // the operator workspaces in the integration suite look like.
    let others = [
        json!({"workspace_id": "w_parent", "worktree": {"is_linked_worktree": false, "repo_key": "k"}}),
        json!({"workspace_id": "w_operator"}),
    ];
    assert!(parent_closable(
        Some(&parent_record()),
        Some("/repo"),
        Some(&others),
        "w_delegate",
        Some("k"),
    ));
    let null_worktree = [
        json!({"workspace_id": "w_parent", "worktree": {"is_linked_worktree": false, "repo_key": "k"}}),
        json!({"workspace_id": "w_operator", "worktree": null}),
    ];
    assert!(parent_closable(
        Some(&parent_record()),
        Some("/repo"),
        Some(&null_worktree),
        "w_delegate",
        Some("k"),
    ));
}

// ---------- require_decide: pure mapping of AgentFetch -> RequireFailure -----

fn entry_for(name: &str) -> Entry {
    Entry {
        name: name.to_string(),
        terminal_id: "t".to_string(),
        pane_id: "p".to_string(),
        root_pane: "rp".to_string(),
        workspace_id: "w".to_string(),
        mode: "cwd".to_string(),
        worktree: None,
        branch: None,
        parent_workspace_id: None,
        repo_root: None,
        repo_key: None,
        harness: "opencode".to_string(),
        model: None,
        sandbox: None,
        round: 1,
        brief: "x".to_string(),
        submitted_at_ms: 0,
        cursor: String::new(),
        created_at_ms: 0,
        root_pane_terminal_id: "rt".to_string(),
    }
}

#[test]
fn w7_require_decide_a_found_record_with_matching_terminal_is_ours() {
    let entry = entry_for("d1");
    let record = json!({"terminal_id": "t"});
    assert!(require_decide(entry, AgentFetch::Found(record)).is_ok());
}

#[test]
fn w7_require_decide_a_mismatched_terminal_is_not_a_delegate() {
    let entry = entry_for("d1");
    let record = json!({"terminal_id": "someone_else"});
    assert!(matches!(
        require_decide(entry, AgentFetch::Found(record)),
        Err(RequireFailure::NotDelegate)
    ));
}

#[test]
fn w7_require_decide_a_missing_agent_is_not_a_delegate() {
    let entry = entry_for("d1");
    assert!(matches!(
        require_decide(entry, AgentFetch::Missing),
        Err(RequireFailure::NotDelegate)
    ));
}

#[test]
fn w7_require_decide_a_failed_lookup_is_a_transport_failure_not_a_delegate_denial() {
    let entry = entry_for("d1");
    let out = require_decide(entry, AgentFetch::Failed("socket closed".to_string()));
    assert!(
        matches!(out, Err(RequireFailure::Failed(ref r)) if r == "socket closed"),
        "Failed must stay Failed — collapsing it into NotDelegate was the F4 blocker"
    );
}

#[test]
fn w7_require_decide_a_timeout_hands_the_entry_back() {
    let entry = entry_for("d1");
    let out = require_decide(entry.clone(), AgentFetch::TimedOut);
    match out {
        Err(RequireFailure::TimedOut(boxed)) => assert_eq!(boxed.name, entry.name),
        other => panic!("expected TimedOut(entry), got {other:?}"),
    }
}

// ---------- W10: "already gone" is reachable ---------------------------------

#[test]
fn w10_a_close_that_answers_workspace_not_found_is_done() {
    // The scenario the P578 r4-4 brief names: a race between two reaps.
    // One wins; the loser's `close_workspace` returns `workspace_not_found`.
    // `is_already_gone` must treat that as the state we wanted, not an error.
    let not_found = ServerError {
        code: "workspace_not_found".to_string(),
        message: "workspace w42 not found".to_string(),
    };
    assert!(is_already_gone(&not_found, None));
}

#[test]
fn w10_a_kill_that_answers_not_git_worktree_is_done() {
    let not_git = ServerError {
        code: "not_git_worktree".to_string(),
        message: "path is not a git worktree".to_string(),
    };
    assert!(is_already_gone(&not_git, Some("/some/path")));
}

#[test]
fn w10_an_unknown_code_without_a_vanished_path_is_still_an_error() {
    let other = ServerError {
        code: "dirty_worktree_requires_force".to_string(),
        message: "the worktree is dirty".to_string(),
    };
    // Root (/) always exists, so the path-not-exists fallback does not fire
    // here, and `dirty_worktree_requires_force` is not in ALREADY_GONE_CODES.
    assert!(!is_already_gone(&other, Some("/")));
    assert!(!is_already_gone(&other, None));
}

#[test]
fn w10_an_unknown_code_with_a_vanished_path_is_already_gone() {
    let other = ServerError {
        code: "some_other_refusal".to_string(),
        message: "whatever".to_string(),
    };
    // A path that does not exist IS evidence the checkout is gone; the
    // server's wording does not have to match our code list.
    assert!(is_already_gone(
        &other,
        Some("/this/path/surely/does/not/exist/under/claude/flock")
    ));
}

// ---------- W12: cap_for caps PaneSendInput / PaneSendKeys at 10 s -----------

#[test]
fn w12_cap_for_submit_methods_is_at_most_10_seconds() {
    use crate::api::schema::{PaneSendInputParams, PaneSendKeysParams};
    use std::time::Duration;
    let input = Method::PaneSendInput(PaneSendInputParams {
        pane_id: "p".to_string(),
        text: "x".to_string(),
        keys: Vec::new(),
    });
    let keys = Method::PaneSendKeys(PaneSendKeysParams {
        pane_id: "p".to_string(),
        keys: vec!["Enter".to_string()],
    });
    assert!(
        cap_for(&input) <= Duration::from_secs(10),
        "{:?}",
        cap_for(&input)
    );
    assert!(
        cap_for(&keys) <= Duration::from_secs(10),
        "{:?}",
        cap_for(&keys)
    );
}

// ---------- G12: find_new_checkout uses same_path, not raw strings -----------

#[test]
fn w13_find_new_checkout_treats_two_spellings_of_one_path_as_the_same() {
    // Pre-existing checkout spelled with a trailing slash in `before`, same
    // path without one in `after`: a raw-string comparison would call the
    // second entry "new" and force-kill it. `same_path` canonicalises.
    let before = vec![json!({"path": "/repo/wt/x/", "branch": "feat/x"})];
    let after = vec![
        json!({"path": "/repo/wt/x", "branch": "feat/x"}),
        json!({"path": "/repo/wt/y", "branch": "feat/y"}),
    ];
    let new = find_new_checkout(&before, &after, Some("feat/x"));
    assert_eq!(
        new, None,
        "a respelled pre-existing checkout must not be force-killed: {new:?}"
    );
    // And when there IS a new one, filtering by branch still finds it.
    let new = find_new_checkout(&before, &after, Some("feat/y"));
    assert_eq!(new.as_deref(), Some("/repo/wt/y"));
}

#[tokio::test]
async fn delegate_startup_dialog_ignores_scrollback() {
    let dialog = HARNESSES
        .iter()
        .find(|harness| harness.name == "codex")
        .and_then(|harness| harness.startup_dialog.as_ref())
        .expect("Codex dialog");
    let screen = "Hooks need review\r\n1 hook is new or changed.\r\nHooks can run outside the sandbox after you trust them.\r\n› 1. Review hooks\r\n2. Trust all and continue\r\n3. Continue without trusting (hooks won't run)\r\nenter confirm · esc skip";
    let mut bytes = screen.as_bytes().to_vec();
    bytes.extend_from_slice("\r\n".repeat(24).as_bytes());
    bytes.extend_from_slice(b"\x1b[2J\x1b[H");
    let pane = crate::terminal::TerminalRuntime::test_with_scrollback_bytes(80, 24, 65_536, &bytes);
    assert!(
        (dialog.shows)(&pane.recent_text(STARTUP_DIALOG_LINES as usize)),
        "the old dialog remains in scrollback"
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    let shown = pane_shows_startup_dialog_with("fixture:p1", dialog, deadline, |method, bound| {
        assert_eq!(bound, Some(deadline));
        let Method::PaneRead(params) = method else {
            panic!("expected pane read")
        };
        assert_eq!(params.source, ReadSource::Detection);
        let text = match params.source {
            ReadSource::Detection => pane.detection_text(),
            ReadSource::Recent => pane.recent_text(params.lines.unwrap_or(40) as usize),
            _ => panic!("unexpected read source"),
        };
        Ok(json!({"result": {"read": {"text": text}}}))
    });
    assert!(
        !shown,
        "a dialog outside the live screen cannot block startup"
    );
}
