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
        wait.grace_decision(Grace::Open, Instant::now()),
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
        wait.grace_decision(Grace::Exhausted, Instant::now()),
        SettledDecision::Report {
            outcome: Outcome::NoResult,
            ..
        }
    ));
}

// ---------- the round loop, over a scripted settle and store --------------

use super::super::settled::{GoneReason, LastRecord, SettledOutcome};
use std::collections::VecDeque;

/// A settle core and result store that answer from a script, on a clock that
/// only moves when the loop sleeps. Asking for a settle the script does not
/// have panics, so a loop that never ends fails rather than hangs.
struct Scripted {
    settles: VecDeque<SettledOutcome>,
    results: VecDeque<serde_json::Value>,
    now: Instant,
    started: Instant,
    settled: usize,
    polled: usize,
}

impl Scripted {
    fn new(settles: Vec<SettledOutcome>, results: Vec<serde_json::Value>) -> Self {
        let now = Instant::now();
        Self {
            settles: settles.into(),
            results: results.into(),
            now,
            started: now,
            settled: 0,
            polled: 0,
        }
    }

    fn slept(&self) -> Duration {
        self.now.duration_since(self.started)
    }
}

impl RoundIo for Scripted {
    fn settle(&mut self, _after: Option<&str>) -> io::Result<(SettledOutcome, bool)> {
        self.settled += 1;
        let settled = self
            .settles
            .pop_front()
            .expect("the loop settled more often than the script allows");
        Ok((settled, false))
    }

    fn agent_result(&mut self) -> Result<serde_json::Value, BoundedError> {
        self.polled += 1;
        // The last answer stands until the script says otherwise.
        let answer = if self.results.len() > 1 {
            self.results.pop_front()
        } else {
            self.results.front().cloned()
        };
        Ok(answer.expect("the script has no agent.result answer"))
    }

    fn agent_record(&mut self) -> AgentFetch {
        // The same turn the settle ended on: no new turn has started.
        AgentFetch::Found(json!({"turn_cursor": "t:0:2:3:i"}))
    }

    fn now(&self) -> Instant {
        self.now
    }

    fn sleep(&mut self, until: Instant, interval: Duration) {
        self.now = until.min(self.now + interval).max(self.now);
    }
}

fn settled() -> SettledOutcome {
    SettledOutcome::Settled {
        held_ms: 5_000,
        last: LastRecord {
            pane_id: Some("p".into()),
            agent_status: Some("idle".into()),
            turn_cursor: Some("t:0:2:3:i".into()),
        },
    }
}

fn reply(status: Option<&str>, text: &str) -> serde_json::Value {
    let mut result = json!({
        "pane_id": "p", "workspace_id": "w", "agent": "claude", "session_id": "s",
        "finished": true, "text": text, "offset": 0, "total_chars": text.len(),
        "at_ms": SUBMITTED_MS + 2_000,
    });
    if let Some(status) = status {
        result["status"] = json!(status);
    }
    json!({"id": "r", "result": {"type": "agent_result", "result": result}})
}

fn ended(io: &mut Scripted, round: u64) -> (Outcome, i32) {
    let entry = entry(round);
    let wait = waiting(&entry);
    match wait.decide(io).expect("the loop ends") {
        RoundEnd::Report { outcome, .. } => (outcome, outcome.exit_code()),
        RoundEnd::Usage(reason) | RoundEnd::Failed(reason) => panic!("not an outcome: {reason}"),
    }
}

#[test]
fn a_round_that_ended_without_a_sentinel_is_exit_5_at_once() {
    // The shape a resumed session ends a round on (#582): the harness's own
    // `No response requested.`, a finished reply with no sentinel.
    let mut io = Scripted::new(vec![settled()], vec![reply(None, "No response requested.")]);
    assert_eq!(ended(&mut io, 2), (Outcome::NoSentinel, 5));
    assert_eq!((io.settled, io.polled), (1, 1));
    assert_eq!(io.slept(), Duration::ZERO);
}

#[test]
fn a_round_that_ended_with_nothing_written_is_no_result_after_one_grace() {
    let mut io = Scripted::new(vec![settled()], vec![no_result(None)]);
    let grace = harness_for(&entry(1)).result_grace;
    assert_eq!(ended(&mut io, 1), (Outcome::NoResult, 5));
    assert_eq!(io.settled, 1);
    assert!(io.slept() >= grace && io.slept() <= grace + RESULT_POLL);
}

#[test]
fn a_dead_pane_is_exit_4() {
    let closed = SettledOutcome::Gone {
        reason: GoneReason::Closed,
        last: LastRecord::default(),
    };
    let mut io = Scripted::new(vec![closed], vec![]);
    assert_eq!(ended(&mut io, 2), (Outcome::Gone, 4));
    assert_eq!(io.polled, 0);

    // Gone between the settle and the grace's question.
    let gone = json!({"id": "r", "error": {"code": "agent_not_found", "message": "gone"}});
    let mut io = Scripted::new(vec![settled()], vec![gone]);
    assert_eq!(ended(&mut io, 2), (Outcome::Gone, 4));
}

#[test]
fn a_still_running_turn_keeps_waiting_until_it_ends() {
    let open = || no_result(Some(json!({"turn_open_at_ms": SUBMITTED_MS + 939})));
    let mut io = Scripted::new(
        vec![settled(), settled(), settled()],
        vec![open(), open(), reply(Some("done"), "DONE: shipped")],
    );
    assert_eq!(ended(&mut io, 1), (Outcome::Done, 0));
    // Each open answer cost a settle, never a grace: the screen's quiet is
    // what paces the loop, not a timer.
    assert_eq!((io.settled, io.polled), (3, 3));
    assert_eq!(io.slept(), Duration::ZERO);
}
