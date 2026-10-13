//! #1257: a round the harness still records as running is never `no_result`.
//!
//! The screen can read idle inside a turn (every Claude pane on a server did
//! for a minute after a client detached, #1264), so the settle fires mid-round and
//! the reply grace asks `agent.result`. These pin what the grace makes of the
//! store's answer, for a long first round and for a round `send` started.

use super::*;
use serde_json::json;

const SUBMITTED_MS: u64 = 1_791_853_575_000;

fn entry(round: u64) -> Entry {
    Entry {
        name: "d".to_string(),
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
        harness: "claude".to_string(),
        model: None,
        sandbox: None,
        round,
        brief: "x".to_string(),
        submitted_at_ms: SUBMITTED_MS,
        cursor: "t:0:1:2:i".to_string(),
        created_at_ms: 0,
        root_pane_terminal_id: "rt".to_string(),
        base: None,
        base_sha: None,
        agent_id: None,
    }
}

fn waiting(entry: &Entry) -> Await<'_> {
    Await {
        harness: harness_for(entry),
        entry,
        after: Some(entry.cursor.clone()),
        submitted_at_ms: entry.submitted_at_ms,
        deadline: None,
        settle_ms: None,
        silence_ms: None,
        max_chars: None,
        json: false,
    }
}

fn no_result(data: Option<serde_json::Value>) -> serde_json::Value {
    let mut error = json!({"code": "no_result", "message": "session s has no assistant reply yet"});
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({"id": "r", "error": error})
}

fn earlier_reply(turn_open_at_ms: Option<u64>) -> serde_json::Value {
    let mut result = json!({
        "pane_id": "p", "workspace_id": "w", "agent": "claude", "session_id": "s",
        "finished": false, "status": "done", "status_text": "one", "text": "DONE: one",
        "offset": 0, "total_chars": 9, "at_ms": SUBMITTED_MS - 60_000,
    });
    if let Some(at_ms) = turn_open_at_ms {
        result["turn_open_at_ms"] = json!(at_ms);
    }
    json!({"id": "r", "result": {"type": "agent_result", "result": result}})
}

#[test]
fn a_long_first_round_still_running_is_open_not_no_result() {
    let entry = entry(1);
    let wait = waiting(&entry);
    let open = no_result(Some(json!({"turn_open_at_ms": SUBMITTED_MS + 939})));
    assert!(matches!(wait.classify_result(&open), ResultPoll::Open));
    assert!(matches!(
        wait.grace_decision(Grace::Open),
        SettledDecision::SettleAgain
    ));
}

#[test]
fn a_round_started_by_send_still_running_is_open_not_no_result() {
    let entry = entry(2);
    let wait = waiting(&entry);
    let open = earlier_reply(Some(SUBMITTED_MS + 939));
    assert!(matches!(wait.classify_result(&open), ResultPoll::Open));
}

#[test]
fn without_an_open_turn_from_this_round_the_grace_is_unchanged() {
    let entry = entry(2);
    let wait = waiting(&entry);
    // An older server, or a turn that ended with nothing written.
    assert!(matches!(
        wait.classify_result(&no_result(None)),
        ResultPoll::NotYet
    ));
    assert!(matches!(
        wait.classify_result(&earlier_reply(None)),
        ResultPoll::NotYet
    ));
    // A turn open since before this round's submit says nothing about it.
    let before = no_result(Some(json!({"turn_open_at_ms": SUBMITTED_MS - 1})));
    assert!(matches!(wait.classify_result(&before), ResultPoll::NotYet));
    // A grace that ran out with no evidence of a running round is still
    // `no_result`.
    assert!(matches!(
        wait.grace_decision(Grace::Exhausted),
        SettledDecision::Report {
            outcome: Outcome::NoResult,
            ..
        }
    ));
}
