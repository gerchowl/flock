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
        base: None,
        base_sha: None,
        agent_id: None,
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
fn require_decide_a_restarted_delegate_is_found_by_its_agent_id() {
    // #975: a server restart re-mints the terminal id. The agent id survives,
    // and the entry is re-pointed at the new terminal and pane.
    let mut entry = entry_for("d1");
    entry.agent_id = Some("agent_a".to_string());
    let record = json!({"terminal_id": "t2", "pane_id": "p2", "agent_id": "agent_a"});
    let live = require_decide(entry, AgentFetch::Found(record)).expect("same agent");
    assert_eq!(live.terminal_id, "t2");
    assert_eq!(live.pane_id, "p2");
}

#[test]
fn require_decide_another_agent_on_a_new_terminal_is_not_a_delegate() {
    let mut entry = entry_for("d1");
    entry.agent_id = Some("agent_a".to_string());
    let record = json!({"terminal_id": "t2", "agent_id": "agent_b"});
    assert!(matches!(
        require_decide(entry, AgentFetch::Found(record)),
        Err(RequireFailure::NotDelegate)
    ));
}

#[test]
fn require_decide_backfills_the_agent_id_on_a_terminal_match() {
    let entry = entry_for("d1");
    let record = json!({"terminal_id": "t", "agent_id": "agent_a"});
    let live = require_decide(entry, AgentFetch::Found(record)).expect("same terminal");
    assert_eq!(live.agent_id.as_deref(), Some("agent_a"));
}

#[test]
fn require_decide_a_legacy_worktree_entry_survives_a_restart_in_its_worktree() {
    // An entry written before agent ids were recorded (the #975 report): the
    // agent still runs in the delegate's own worktree under its name.
    let mut entry = entry_for("d1");
    entry.worktree = Some("/wt/d1".to_string());
    let record =
        json!({"terminal_id": "t2", "pane_id": "p2", "agent_id": "agent_a", "cwd": "/wt/d1"});
    let live = require_decide(entry.clone(), AgentFetch::Found(record)).expect("same worktree");
    assert_eq!(live.terminal_id, "t2");
    assert_eq!(live.agent_id.as_deref(), Some("agent_a"));

    let elsewhere = json!({"terminal_id": "t2", "agent_id": "agent_a", "cwd": "/elsewhere"});
    assert!(matches!(
        require_decide(entry, AgentFetch::Found(elsewhere)),
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

// ---------- recorded_decide: status/result answer for a gone delegate (#817) --

#[test]
fn recorded_decide_a_record_with_no_agent_left_is_gone() {
    let out = recorded_decide(entry_for("d1"), AgentFetch::Missing, || AgentFetch::Missing);
    assert!(
        matches!(out, Ok(Recorded::Gone(ref e)) if e.name == "d1"),
        "{out:?}"
    );
}

#[test]
fn recorded_decide_a_renamed_agent_is_still_not_a_delegate() {
    let out = recorded_decide(entry_for("d1"), AgentFetch::Missing, || {
        AgentFetch::Found(json!({"terminal_id": "t", "name": "renamed"}))
    });
    assert!(matches!(out, Err(RequireFailure::NotDelegate)), "{out:?}");
}

#[test]
fn recorded_decide_a_live_agent_skips_the_terminal_lookup() {
    let out = recorded_decide(
        entry_for("d1"),
        AgentFetch::Found(json!({"terminal_id": "t"})),
        || panic!("a name that resolves needs no second lookup"),
    );
    assert!(matches!(out, Ok(Recorded::Live(_))), "{out:?}");
}

#[test]
fn recorded_decide_an_unreachable_terminal_lookup_is_a_failure_not_gone() {
    let out = recorded_decide(entry_for("d1"), AgentFetch::Missing, || {
        AgentFetch::Failed("socket closed".to_string())
    });
    assert!(matches!(out, Err(RequireFailure::Failed(_))), "{out:?}");
}

// ---------- submit_outcome: one answer for a pane that died (#817) ----------

#[test]
fn submit_outcome_a_pane_gone_before_typing_is_gone() {
    let refused = json!({"error": {"code": "pane_gone", "message": "pane_gone"}});
    assert!(matches!(submit_outcome(&refused), Err(SubmitFailure::Gone)));
}

#[test]
fn submit_outcome_a_pane_gone_during_confirmation_is_gone() {
    let abandoned = json!({"result": {"outcome": "abandoned", "reason": "pane_gone"}});
    assert!(matches!(
        submit_outcome(&abandoned),
        Err(SubmitFailure::Gone)
    ));
}

#[test]
fn submit_outcome_other_failures_keep_their_kind() {
    let refused = json!({"error": {"code": "input_not_empty", "message": "x"}});
    assert!(matches!(
        submit_outcome(&refused),
        Err(SubmitFailure::Refused(_))
    ));
    let abandoned = json!({"result": {"outcome": "abandoned", "reason": "operator_active"}});
    assert!(matches!(
        submit_outcome(&abandoned),
        Err(SubmitFailure::Unconfirmed(_))
    ));
    let accepted = json!({"result": {"outcome": "accepted"}});
    assert!(submit_outcome(&accepted).is_ok());
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
        write_timeout_ms: None,
        raw: false,
        pane_id: "p".to_string(),
        text: "x".to_string(),
        keys: Vec::new(),
    });
    let keys = Method::PaneSendKeys(PaneSendKeysParams {
        write_timeout_ms: None,
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
        .and_then(|harness| harness.startup_dialogs.first())
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

// ---------- recover_lost_brief_with: the #828 resubmit decision ---------------

const LOST_SENTENCE: &str = "Read /fixture/briefs/task.md and execute it exactly.";
const EMPTY_CODEX: &str = "OpenAI Codex\n› Ask Codex to do anything\n  ? for shortcuts\n";

struct LostBriefRun {
    outcome: Result<bool, SubmitFailure>,
    resubmits: u32,
    elapsed: Duration,
}

/// Drive the decision with one probe per fake second until it answers.
fn run_lost_brief(mut probe: impl FnMut(u64) -> Probe) -> LostBriefRun {
    let mut entry = entry_for("lost");
    entry.harness = "codex".to_string();
    entry.cursor = "term_fixture:0:3:8:i".to_string();
    let start = Instant::now();
    let settle = Duration::from_millis(300);
    let deadline = start + start_watch::StartWatch::window(settle) + LOST_BRIEF_MARGIN;
    let clock = std::cell::Cell::new(start);
    let mut second = 0;
    let resubmits = std::cell::Cell::new(0);
    let outcome = recover_lost_brief_with(
        &entry,
        LOST_SENTENCE,
        settle,
        start,
        deadline,
        SubmitFailure::Unconfirmed("owned_composer_not_visible".into()),
        || {
            second += 1;
            probe(second)
        },
        || {
            resubmits.set(resubmits.get() + 1);
            Ok(())
        },
        || clock.get(),
        || clock.set(clock.get() + Duration::from_secs(1)),
    );
    LostBriefRun {
        outcome,
        resubmits: resubmits.get(),
        elapsed: clock.get() - start,
    }
}

fn idle_probe(cursor: &str, screen: &str) -> Probe {
    Probe::Record {
        status: crate::api::schema::AgentStatus::Idle,
        cursor: Cursor::parse(cursor).unwrap(),
        screen: screen.to_string(),
        unfaint: screen.to_string(),
    }
}

#[test]
fn a_brief_swallowed_at_startup_is_resubmitted_once_after_the_window() {
    let run = run_lost_brief(|_| idle_probe("term_fixture:0:3:8:i", EMPTY_CODEX));
    assert!(matches!(run.outcome, Ok(true)), "{:?}", run.outcome);
    assert_eq!(run.resubmits, 1);
    assert!(
        run.elapsed >= start_watch::StartWatch::window(Duration::from_millis(300)),
        "never before the not_started window: {:?}",
        run.elapsed
    );
}

#[test]
fn a_brief_that_shows_on_screen_is_never_resubmitted() {
    // vigil-5: the brief was queued, then the composer emptied again.
    let shown =
        format!("› Read /fixture/briefs/task.md and\n  execute it exactly.\n\n{EMPTY_CODEX}");
    let run = run_lost_brief(|second| {
        idle_probe(
            "term_fixture:0:3:8:i",
            if second == 3 { &shown } else { EMPTY_CODEX },
        )
    });
    assert!(
        matches!(run.outcome, Err(SubmitFailure::Unconfirmed(_))),
        "{:?}",
        run.outcome
    );
    assert_eq!(run.resubmits, 0);
    assert_eq!(
        run.elapsed,
        Duration::from_secs(2),
        "it stops at the sighting"
    );
}

#[test]
fn a_turn_that_starts_late_confirms_the_brief_without_a_resubmit() {
    let run = run_lost_brief(|second| {
        if second < 20 {
            idle_probe("term_fixture:0:3:8:i", EMPTY_CODEX)
        } else {
            idle_probe("term_fixture:0:4:9:i", EMPTY_CODEX)
        }
    });
    assert!(matches!(run.outcome, Ok(false)), "{:?}", run.outcome);
    assert_eq!(run.resubmits, 0);
}

#[test]
fn a_brief_running_past_the_weekly_notice_is_taken_and_never_resubmitted() {
    // #870: the record has not caught up, but the screen shows the brief
    // echoed with its turn running under it.
    let screen = include_str!("../../tests/fixtures/codex/working-after-weekly-notice.txt")
        .replace("/fixture/codex-870/briefs/63.md", "/fixture/briefs/task.md");
    let run = run_lost_brief(|_| idle_probe("term_fixture:0:3:8:i", &screen));
    assert!(matches!(run.outcome, Ok(false)), "{:?}", run.outcome);
    assert_eq!(run.resubmits, 0);
    assert_eq!(run.elapsed, Duration::ZERO, "decided on the first look");
}

#[test]
fn queued_startup_evidence_or_a_restart_never_resubmits() {
    let queued =
        "Waiting for startup · esc cancel\n› Ask Codex to do anything\n  ? for shortcuts\n";
    let run = run_lost_brief(|_| idle_probe("term_fixture:0:3:8:i", queued));
    assert!(matches!(run.outcome, Err(SubmitFailure::Unconfirmed(_))));
    assert_eq!(run.resubmits, 0);

    let run = run_lost_brief(|_| idle_probe("term_fixture:1:3:8:i", EMPTY_CODEX));
    assert!(matches!(run.outcome, Err(SubmitFailure::Unconfirmed(_))));
    assert_eq!(run.resubmits, 0);

    let run = run_lost_brief(|_| Probe::Gone);
    assert!(matches!(run.outcome, Err(SubmitFailure::Gone)));
    assert_eq!(run.resubmits, 0);
}

#[test]
fn only_codex_retypes_a_lost_brief() {
    for harness in &HARNESSES {
        assert_eq!(
            harness.resubmit_lost_brief,
            harness.name == "codex",
            "{} (opencode keeps #636's never-retype rule)",
            harness.name
        );
    }
}

// ---------- the failed-start tail (#925) ------------------------------------

/// A start that fails reads the pane's recent buffer, bounded by the caller's
/// deadline, and asks for more rows than it prints so a screen's blank bottom
/// cannot crowd the content out.
#[test]
fn pane_tail_reads_the_recent_buffer_within_the_deadline() {
    let deadline = Instant::now() + Duration::from_secs(1);
    let tail = pane_tail("w2:p1", deadline, |method, bound| {
        assert_eq!(bound, Some(deadline));
        let Method::PaneRead(params) = method else {
            panic!("expected pane read")
        };
        assert_eq!(params.pane_id, "w2:p1");
        assert_eq!(params.source, ReadSource::Recent);
        assert!(params.lines.unwrap() > FAILURE_TAIL_LINES);
        assert!(params.strip_ansi);
        Ok(json!({"result": {"read": {"text": "a\nb\n"}}}))
    });
    assert_eq!(tail.as_deref(), Some("a\nb\n"));

    let failed = pane_tail("w2:p1", deadline, |_, _| Err(BoundedError::TimedOut));
    assert_eq!(failed, None, "a read that fails prints nothing");
    let no_pane = pane_tail("", deadline, |_, _| panic!("no pane, no read"));
    assert_eq!(no_pane, None);
}

/// The printed block: the last twenty non-trailing-blank lines, each behind a
/// bar, under a header naming the delegate. An all-blank pane prints nothing.
#[test]
fn tail_block_keeps_the_last_twenty_lines_and_drops_the_blank_bottom() {
    let mut screen: Vec<String> = (1..=30).map(|n| format!("line {n}")).collect();
    screen.push("\u{276f} ghost suggestion   ".to_string());
    screen.extend(std::iter::repeat_n(String::new(), 10));
    let block = tail_block("d1", &screen.join("\n")).expect("a block");
    let lines: Vec<&str> = block.lines().collect();
    assert_eq!(lines[0], "delegate d1: last 20 pane line(s):");
    assert_eq!(lines.len(), 21, "{block}");
    assert_eq!(lines[1], "  | line 12");
    assert_eq!(
        lines[20], "  | \u{276f} ghost suggestion",
        "the composer line, trailing spaces trimmed"
    );

    let short = tail_block("d1", "only\n\n").unwrap();
    assert_eq!(short, "delegate d1: last 1 pane line(s):\n  | only\n");
    assert_eq!(tail_block("d1", "\n   \n\n"), None);
}

/// #938: `data.pane_tail` holds exactly the lines the block prints, bare.
#[test]
fn tail_lines_are_the_printed_lines_without_the_bar() {
    let mut screen: Vec<String> = (1..=25).map(|n| format!("line {n}  ")).collect();
    screen.extend(std::iter::repeat_n(String::new(), 3));
    let text = screen.join("\n");
    let lines = tail_lines(&text);
    assert_eq!(lines.len(), 20);
    assert_eq!((lines[0], lines[19]), ("line 6", "line 25"));
    assert!(tail_lines("\n  \n").is_empty());
}

/// `--keep-on-failure` is a start flag, and only that.
#[test]
fn keep_on_failure_is_a_start_flag() {
    let args = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
    let start = parse_flags(Verb::Start, &args(&["d1", "--keep-on-failure"])).unwrap();
    assert!(as_start(&start).keep_on_failure);
    assert!(!as_start(&parse_flags(Verb::Start, &args(&["d1"])).unwrap()).keep_on_failure);
    for verb in [Verb::Send, Verb::Wait, Verb::Reap] {
        assert!(parse_flags(verb, &args(&["d1", "--keep-on-failure"])).is_err());
    }
}

/// Real local socket and App dispatch, with no open repository workspace.
struct ClosedWorkspaceFixture {
    app: crate::app::App,
    server: Option<crate::api::server::ServerHandle>,
    repo: PathBuf,
    checkout: PathBuf,
    root: crate::test_support::TempDir,
    env: Vec<(&'static str, Option<std::ffi::OsString>)>,
}
impl ClosedWorkspaceFixture {
    fn new() -> Self {
        let root = crate::test_support::TempDir::socket();
        let mut env = vec![("FLOCK_CONFIG_PATH", std::env::var_os("FLOCK_CONFIG_PATH"))];
        std::env::remove_var("FLOCK_CONFIG_PATH");
        for (key, value) in [
            ("HOME", root.path().to_path_buf()),
            ("XDG_CONFIG_HOME", root.path().join("config")),
            ("XDG_STATE_HOME", root.path().join("state")),
            ("FLOCK_SOCKET_PATH", root.path().join("api.sock")),
        ] {
            env.push((key, std::env::var_os(key)));
            std::env::set_var(key, value);
        }
        let repo = crate::test_support::create_committed_repo("delegate-explicit-repo");
        let checkout = root.path().join("checkout");
        crate::test_support::run_git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "worktree/delegate-recovery",
                checkout.to_str().unwrap(),
                "HEAD",
            ],
        );
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let hub = crate::api::EventHub::default();
        let mut app = crate::app::App::new_with_runtime_config(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            hub.clone(),
            crate::app::AppRuntimeConfig {
                merge_gate: crate::worktree::MergeGateContext {
                    gh_program: root.path().join("missing-gh"),
                },
                ..Default::default()
            },
        );
        app.state.workspaces.clear();
        app.state.active = None;
        let server =
            crate::api::server::start_server_with_capabilities(tx, hub, None, Default::default())
                .unwrap();
        Self {
            app,
            server: Some(server),
            repo,
            checkout,
            root,
            env,
        }
    }

    async fn drive<T: Send + 'static>(&mut self, call: impl FnOnce() -> T + Send + 'static) -> T {
        let (done, mut result) = tokio::sync::oneshot::channel();
        let client = std::thread::spawn(move || {
            let _ = done.send(call());
        });
        let output = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                tokio::select! {
                    output = &mut result => break output.unwrap(),
                    request = self.app.api_rx.recv() => {
                        let request = request.expect("owned listener remains live");
                        self.app.current_api_peer_pid = request.peer_pid;
                        self.app.current_api_peer_birth = Some(request.peer_birth);
                        self.app.current_api_peer_uid = Some(request.peer_uid);
                        let response = self.app.handle_api_request(request.request);
                        request.respond_to.send(response).unwrap();
                    }
                }
            }
        })
        .await
        .expect("bounded delegate fixture request/response");
        client.join().unwrap();
        output
    }
}
impl Drop for ClosedWorkspaceFixture {
    fn drop(&mut self) {
        self.server.take().unwrap().finish_for_test();
        std::fs::remove_dir_all(&self.repo).unwrap();
        for (key, prior) in self.env.drain(..).rev() {
            match prior {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[tokio::test]
async fn reap_with_all_workspaces_closed_removes_checkout_and_registry_without_pruning_siblings() {
    let _guard = crate::config::test_config_env_guard();
    let mut fixture = ClosedWorkspaceFixture::new();
    let sibling = fixture.root.path().join("temporarily-absent");
    crate::test_support::run_git(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "-b",
            "worktree/absent-sibling",
            sibling.to_str().unwrap(),
            "HEAD",
        ],
    );
    std::fs::remove_dir_all(&sibling).unwrap();
    let mut entry = entry_for("closed-delegate");
    entry.mode = "worktree".into();
    entry.worktree = Some(fixture.checkout.to_string_lossy().into_owned());
    entry.repo_root = Some(fixture.repo.to_string_lossy().into_owned());
    entry.branch = Some("worktree/delegate-recovery".into());
    write_entry(&entry).unwrap();
    let path = entry_path(&entry.name);
    let result = fixture
        .drive(|| delegate_reap(&["closed-delegate".into(), "--json".into()]))
        .await
        .unwrap();
    assert_eq!(result, 0);
    assert!(!fixture.checkout.exists());
    assert!(!path.exists());
    assert!(crate::worktree::list_existing_worktrees(&fixture.repo)
        .unwrap()
        .iter()
        .any(|row| row.path == sibling));
}

#[tokio::test]
async fn placement_failure_cleans_up_with_no_open_workspace() {
    let _guard = crate::config::test_config_env_guard();
    let mut fixture = ClosedWorkspaceFixture::new();
    let repo = fixture.repo.to_string_lossy().into_owned();
    let result = fixture
        .drive(move || {
            teardown_after_place_failure(
                "failed-placement",
                Some(&[]),
                Mode::Worktree,
                &start_flags_worktree("worktree/delegate-recovery"),
                &repo,
                CreateFailure::TimedOut,
            )
        })
        .await;
    assert_eq!(result, crate::cli::exit::FAILURE);
    assert!(!fixture.checkout.exists());
    assert!(fixture.repo.join(".git").is_dir());
}

#[tokio::test]
async fn cli_explicit_repo_kill_works_without_an_open_workspace() {
    let _guard = crate::config::test_config_env_guard();
    let mut fixture = ClosedWorkspaceFixture::new();
    let repo = fixture.repo.to_string_lossy().into_owned();
    let checkout = fixture.checkout.to_string_lossy().into_owned();
    let result = fixture
        .drive(move || {
            crate::cli::worktree::run_worktree_command(&[
                "kill".into(),
                "--path".into(),
                checkout,
                "--repo".into(),
                repo,
                "--keep-branch".into(),
                "--keep-procs".into(),
                "--json".into(),
            ])
        })
        .await
        .unwrap();
    assert_eq!(result, 0);
    assert!(!fixture.checkout.exists());
}
