//! The armed self-compaction end to end: a `pane.arm_self_compact` arrives
//! over the socket and the assertion is on the bytes that reach that pane's
//! PTY, driven by the same `pane.report_agent_session` the `flk hook claude
//! session` shim posts. Nothing here constructs the arming directly — AGENTS.md's
//! #328 lesson, and the reason the load-bearing test is the one that starts at
//! the verb and ends at a keystroke.
//!
//! The pure state machine (phases, submit gaps, the timeout that names which
//! half was lost) is tested in `agent_self_compact`.

use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::mpsc;

use crate::api::schema::{Method, PaneArmSelfCompactParams, PaneReportAgentSessionParams, Request};
use crate::app::App;
use crate::detect::{Agent, AgentState};

const GAP: Duration = crate::cli::pane::PANE_RUN_SUBMIT_GAP;
const SETTLED: Duration = Duration::from_secs(30);

/// Claude's input box, as the pane would draw it, with `typed` in it. The
/// empty-prompt gate reads this off the screen, so the gates need a real one.
fn claude_screen(typed: &str) -> Vec<u8> {
    let rule = "─".repeat(40);
    format!("\x1b[2J\x1b[HTask complete.\r\n{rule}\r\n❯ {typed}\r\n{rule}\r\n").into_bytes()
}

struct Rig {
    app: App,
    /// The agent pane's public id — the one that compacts itself.
    pane: String,
    /// Everything written into that pane.
    pty: mpsc::Receiver<Bytes>,
}

fn rig() -> Rig {
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &crate::config::Config::default(),
        true,
        None,
        api_rx,
        crate::api::EventHub::default(),
    );
    let sender = crate::workspace::Workspace::test_new("alpha");
    let mut agent = crate::workspace::Workspace::test_new("beta");
    let focused = agent.focused_pane_id().expect("pane");
    let (runtime, pty) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
    runtime.test_process_pty_bytes(&claude_screen(""));
    agent.tabs[0].runtimes.insert(focused, runtime);
    app.state.workspaces = vec![sender, agent];
    app.state.ensure_test_terminals();
    app.state.active = Some(0);
    let pane = app.public_pane_id(1, focused).expect("public id");
    Rig { app, pane, pty }
}

/// The agent pane lives in workspace 1, the second one in the rig.
const WS: usize = 1;

fn runtime(app: &App) -> &crate::terminal::TerminalRuntime {
    let pane_id = app.state.workspaces[WS].focused_pane_id().expect("pane");
    app.lookup_runtime_sender(WS, pane_id).expect("runtime")
}

fn terminal(app: &mut App) -> &mut crate::terminal::TerminalState {
    let pane_id = app.state.workspaces[WS].focused_pane_id().expect("pane");
    let terminal_id = app.state.terminal_id_for_pane(WS, pane_id).expect("t");
    app.state.terminals.get_mut(&terminal_id).expect("state")
}

/// Claude, showing its idle prompt box, idle since `idle_for` ago and seen
/// just now — the state a self-compaction is allowed to type into.
fn claude_idle_for(app: &mut App, idle_for: Duration) {
    claude_idle_at(app, idle_for, Instant::now());
}

/// As [`claude_idle_for`], but observed at `observed_at` rather than now. A
/// test that drives the clock forward has to move the observation with it: the
/// freshness gate would otherwise (correctly) refuse a screen read six minutes
/// ago, and the test would be measuring the wrong thing.
fn claude_idle_at(app: &mut App, idle_for: Duration, observed_at: Instant) {
    let now = observed_at;
    for at in [now - idle_for, now] {
        terminal(app).set_detected_state_with_screen_signals_at(
            Some(Agent::Claude),
            AgentState::Idle,
            false,
            true,
            false,
            false,
            at,
        );
    }
}

fn arm(app: &mut App, pane: &str, continuation: &str) -> String {
    app.handle_api_request(Request {
        id: "req".into(),
        method: Method::PaneArmSelfCompact(PaneArmSelfCompactParams {
            pane: Some(pane.to_string()),
            continuation: Some(continuation.to_string()),
            abort: false,
        }),
    })
}

/// Exactly what `flk hook claude session` posts once the harness has compacted.
fn session_started_with(app: &mut App, pane: &str, source: &str) {
    app.handle_api_request(Request {
        id: "req".into(),
        method: Method::PaneReportAgentSession(PaneReportAgentSessionParams {
            pane_id: pane.to_string(),
            source: "flock:claude".into(),
            agent: "claude".into(),
            seq: Some(1),
            agent_session_id: Some("sess-1".into()),
            agent_session_path: None,
            session_start_source: Some(source.to_string()),
        }),
    });
}

fn drain(pty: &mut mpsc::Receiver<Bytes>) -> Vec<Vec<u8>> {
    let mut writes = Vec::new();
    while let Ok(bytes) = pty.try_recv() {
        writes.push(bytes.to_vec());
    }
    writes
}

fn written(pty: &mut mpsc::Receiver<Bytes>) -> String {
    drain(pty)
        .iter()
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .collect()
}

/// Ask for the compaction and press Enter on it, as two ticks.
fn request_compaction(app: &mut App, pty: &mut mpsc::Receiver<Bytes>) {
    let now = Instant::now();
    app.tick_self_compacts(now);
    app.tick_self_compacts(now + GAP);
    drain(pty);
}

/// Type the continuation and press Enter on it, as two ticks. Separate from
/// [`request_compaction`] because they are separated by the harness reporting
/// the compaction back, and because the continuation's gates are re-evaluated
/// from scratch — which is the whole point of routing it through the tick.
fn deliver_continuation(app: &mut App, _pty: &mut mpsc::Receiver<Bytes>) {
    let now = Instant::now();
    app.tick_self_compacts(now);
    app.tick_self_compacts(now + GAP);
}

fn armed_continuation(app: &App) -> Option<String> {
    terminal_const(app)
        .armed_self_compact
        .as_ref()
        .map(|armed| armed.continuation.clone())
}

fn terminal_const(app: &App) -> &crate::terminal::TerminalState {
    let pane_id = app.state.workspaces[WS].focused_pane_id().expect("pane");
    let terminal_id = app.state.terminal_id_for_pane(WS, pane_id).expect("t");
    app.state.terminals.get(&terminal_id).expect("state")
}

#[tokio::test]
async fn the_whole_sequence_runs_unattended() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, Duration::from_secs(1));

    // The agent calls the verb mid-turn, so the pane is Working.
    terminal(&mut app).set_detected_state_with_screen_signals_at(
        Some(Agent::Claude),
        AgentState::Working,
        false,
        false,
        false,
        false,
        Instant::now(),
    );
    let reply = arm(&mut app, &pane, "Next: run just check, then open the PR.");
    assert!(
        reply.contains("\"state\":\"armed\""),
        "the verb must answer armed: {reply}"
    );
    assert!(
        reply.contains("nothing has been compacted"),
        "the answer must not read as though the context is already shorter: {reply}"
    );

    // Mid-turn: nothing may be typed.
    app.tick_self_compacts(Instant::now());
    assert!(
        written(&mut pty).is_empty(),
        "the harness cannot compact inside a turn"
    );

    // The turn ends. flock asks the harness to compact, then presses Enter.
    // `now` is read AFTER going idle: that transition moves the pane's state
    // clock, and a tick time from before it would look like the future.
    claude_idle_for(&mut app, SETTLED);
    let now = Instant::now();
    app.tick_self_compacts(now);
    let compact = written(&mut pty);
    assert!(
        compact.contains("/compact"),
        "expected the compact command, got {compact:?}"
    );
    app.tick_self_compacts(now + GAP);
    assert!(!written(&mut pty).is_empty(), "the Enter is its own write");

    // The harness reports the compaction back. That advances the phase and
    // types NOTHING: the continuation is owed to a prompt box, so it waits for
    // the tick and the gates rather than going in while the agent may be busy.
    session_started_with(&mut app, &pane, "compact");
    assert!(
        written(&mut pty).is_empty(),
        "a confirmation must not type into the pane; Claude Code auto-compacts \
         MID-TURN, and a human may be in this box"
    );

    deliver_continuation(&mut app, &mut pty);
    let continuation = written(&mut pty);
    assert!(
        continuation.contains("Next: run just check, then open the PR."),
        "the agent's own handoff prompt must be typed back, got {continuation:?}"
    );

    // `deliver_continuation` typed the prompt and pressed Enter, and that Enter
    // is the end of the sequence: the arming is gone. Left behind it would log
    // this success as a `continuation_timeout` when its clock ran out, and
    // refuse the next arming as `pending` until then.
    assert_eq!(
        armed_continuation(&app),
        None,
        "a finished sequence must not leave an arming behind"
    );
}

#[tokio::test]
async fn the_enter_is_a_second_write_a_gap_later() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, SETTLED);
    arm(&mut app, &pane, "carry on");
    let now = Instant::now();
    app.tick_self_compacts(now);
    let after_text = drain(&mut pty);
    assert!(!after_text.is_empty());

    app.tick_self_compacts(now + GAP - Duration::from_millis(1));
    assert!(
        drain(&mut pty).is_empty(),
        "text and Enter in one read is a paste with a newline in it, not a \
         submitted command (#362)"
    );

    app.tick_self_compacts(now + GAP);
    let after_enter = drain(&mut pty);
    assert!(
        !after_enter.is_empty(),
        "the Enter must still be sent once the gap elapses"
    );
    assert_ne!(after_text, after_enter);
}

/// The loop wakes a compaction at `state_changed_at + settle`, and the screen
/// was observed when the turn ended. So at the moment flock is asked to type,
/// the screen is exactly `settle` old. Reusing `settle` as the freshness bound
/// therefore puts the wake ON the boundary, and one millisecond of jitter
/// reads as `stale_screen` — a compaction that silently never fires.
#[tokio::test]
async fn the_wake_is_not_scheduled_exactly_on_the_freshness_boundary() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    let config = &app.state.config.session;
    assert!(
        config.self_compact_fresh_ms > config.self_compact_settle_ms,
        "the screen is observed when the turn ends and read when the settle \
         expires, so freshness needs slack the settle does not — the same \
         split `[msg] idle_wake_fresh_ms` makes"
    );

    // And a settled pane with a freshly read screen actually fires, rather
    // than being refused as stale at the moment the loop wakes for it.
    claude_idle_for(&mut app, SETTLED);
    arm(&mut app, &pane, "carry on");
    app.tick_self_compacts(Instant::now());
    assert!(
        written(&mut pty).contains("/compact"),
        "settled and freshly read is exactly the state the wake exists for"
    );
}

#[tokio::test]
async fn a_human_at_the_keyboard_keeps_it() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, SETTLED);
    arm(&mut app, &pane, "carry on");
    // `pane.send_text` is a human (or a human's script) at the keyboard.
    app.handle_api_request(Request {
        id: "req".into(),
        method: Method::PaneSendText(crate::api::schema::PaneSendTextParams {
            pane_id: pane.clone(),
            text: "half a thought".into(),
        }),
    });
    drain(&mut pty);

    app.tick_self_compacts(Instant::now());
    assert!(
        written(&mut pty).is_empty(),
        "flock must not type over an operator"
    );
    assert!(
        app.self_compact_deadline.is_some(),
        "the quiet window must wake the loop when it lifts, not wait for traffic"
    );
}

#[tokio::test]
async fn a_non_empty_prompt_box_is_never_typed_into() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, SETTLED);
    arm(&mut app, &pane, "carry on");
    runtime(&app).test_process_pty_bytes(&claude_screen("half a thought"));

    app.tick_self_compacts(Instant::now());
    assert!(
        written(&mut pty).is_empty(),
        "quiet is not empty: a draft left in the box would be submitted with it"
    );
}

#[tokio::test]
async fn arming_twice_keeps_the_first_handoff_prompt() {
    let Rig {
        mut app,
        pane,
        pty: _,
    } = rig();
    claude_idle_for(&mut app, SETTLED);

    let first = arm(&mut app, &pane, "first");
    assert!(first.contains("\"state\":\"armed\""), "{first}");
    let second = arm(&mut app, &pane, "second");
    assert!(
        second.contains("\"state\":\"pending\""),
        "the second arming must be refused, not silently overwrite: {second}"
    );
    assert_eq!(
        armed_continuation(&app).as_deref(),
        Some("first"),
        "an agent that armed twice has two written prompts; keeping the second \
         would discard work it believed it had saved"
    );
}

#[tokio::test]
async fn aborting_reports_whether_there_was_anything_to_drop() {
    let Rig {
        mut app,
        pane,
        pty: _,
    } = rig();
    claude_idle_for(&mut app, SETTLED);

    let nothing = app.handle_api_request(Request {
        id: "req".into(),
        method: Method::PaneArmSelfCompact(PaneArmSelfCompactParams {
            pane: Some(pane.clone()),
            continuation: None,
            abort: true,
        }),
    });
    assert!(nothing.contains("\"state\":\"aborted\""), "{nothing}");

    arm(&mut app, &pane, "carry on");
    let dropped = app.handle_api_request(Request {
        id: "req".into(),
        method: Method::PaneArmSelfCompact(PaneArmSelfCompactParams {
            pane: Some(pane),
            continuation: None,
            abort: true,
        }),
    });
    assert!(dropped.contains("\"state\":\"aborted\""), "{dropped}");
    assert_eq!(armed_continuation(&app), None);
}

#[tokio::test]
async fn a_session_start_that_is_not_a_compaction_drops_the_arming() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, SETTLED);
    arm(&mut app, &pane, "carry on");
    request_compaction(&mut app, &mut pty);

    // The operator cancelled the slash command, so the session restarted
    // instead. Delivering the continuation now would claim a compaction that
    // never happened.
    session_started_with(&mut app, &pane, "resume");
    assert!(
        written(&mut pty).is_empty(),
        "no continuation into an uncompacted session"
    );
    assert_eq!(
        armed_continuation(&app),
        None,
        "the arming must be dropped, not left to fire on some later start"
    );
}

/// Claude Code compacts by itself when the context fills up — which is the
/// exact moment an agent reaches for this verb. So the report can beat flock's
/// own `/compact`, and it must still count.
#[tokio::test]
async fn a_compaction_the_harness_started_itself_still_delivers() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, SETTLED);
    arm(&mut app, &pane, "carry on");

    // The harness got there first — and it did so MID-TURN, which is the only
    // way this happens in practice.
    terminal(&mut app).set_detected_state_with_screen_signals_at(
        Some(Agent::Claude),
        AgentState::Working,
        false,
        false,
        false,
        false,
        Instant::now(),
    );
    session_started_with(&mut app, &pane, "compact");
    deliver_continuation(&mut app, &mut pty);
    assert!(
        written(&mut pty).is_empty(),
        "the agent is mid-turn: the handoff prompt waits for it to stop, \
         rather than landing in a box the agent is still using"
    );

    // The turn ends, and the prompt is delivered then.
    claude_idle_for(&mut app, SETTLED);
    deliver_continuation(&mut app, &mut pty);
    assert!(
        written(&mut pty).contains("carry on"),
        "an unrequested compaction is still a compaction; the handoff prompt \
         is what the agent armed for"
    );

    // And flock must not compact a context that was just compacted.
    assert!(
        !written(&mut pty).contains("/compact"),
        "a second /compact would compact a context that was just compacted"
    );
}

#[tokio::test]
async fn a_resume_while_only_armed_survives() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, SETTLED);
    arm(&mut app, &pane, "carry on");
    // Nothing has been typed, so a restart is not a failure of the sequence.
    session_started_with(&mut app, &pane, "resume");
    assert_eq!(
        armed_continuation(&app).as_deref(),
        Some("carry on"),
        "an arming that wrote nothing must outlive a restart"
    );
    assert!(drain(&mut pty).is_empty());
}

#[tokio::test]
async fn a_compaction_that_never_reports_back_times_out() {
    let Rig {
        mut app,
        pane,
        pty: _,
    } = rig();
    claude_idle_for(&mut app, SETTLED);
    arm(&mut app, &pane, "carry on");
    let now = Instant::now();
    app.tick_self_compacts(now);
    app.tick_self_compacts(now + GAP);

    // The harness never answers. Without a bound the arming would sit there
    // refusing every later arming forever.
    let timeout = Duration::from_millis(app.state.config.session.self_compact_timeout_ms);
    app.tick_self_compacts(now + timeout + Duration::from_secs(1));
    assert_eq!(
        armed_continuation(&app),
        None,
        "an unreported compaction must not hold the arming forever — it would \
         wedge the pane against every later arming too"
    );
}

#[tokio::test]
async fn an_arming_the_agent_never_finishes_is_never_timed_out() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    // Mid-turn: working, and staying working.
    terminal(&mut app).set_detected_state_with_screen_signals_at(
        Some(Agent::Claude),
        AgentState::Working,
        false,
        false,
        false,
        false,
        Instant::now(),
    );
    arm(&mut app, &pane, "carry on");

    let timeout = Duration::from_millis(app.state.config.session.self_compact_timeout_ms);
    app.tick_self_compacts(Instant::now() + timeout * 3);
    assert_eq!(
        armed_continuation(&app).as_deref(),
        Some("carry on"),
        "an agent that armed mid-turn may be working for minutes; the timeout \
         covers the half flock cannot see into, not this one"
    );
    assert!(drain(&mut pty).is_empty());
}

#[tokio::test]
async fn an_agent_whose_harness_cannot_compact_is_refused_by_name() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    terminal(&mut app).set_detected_state_with_screen_signals_at(
        Some(Agent::OpenCode),
        AgentState::Idle,
        false,
        true,
        false,
        false,
        Instant::now(),
    );

    let reply = arm(&mut app, &pane, "carry on");
    assert!(
        reply.contains("cannot be asked to compact its own"),
        "an arming that could never fire must be refused at the verb, naming \
         why, so the agent does not wait forever for a continuation: {reply}"
    );
    assert_eq!(armed_continuation(&app), None);
    assert!(drain(&mut pty).is_empty());
}

#[tokio::test]
async fn self_compact_can_be_turned_off_entirely() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, SETTLED);
    app.state.config.session.self_compact = false;

    let reply = arm(&mut app, &pane, "carry on");
    assert!(
        reply.contains("self_compact_unavailable") && reply.contains("self_compact"),
        "a refusal has to say which switch: {reply}"
    );
    assert_eq!(armed_continuation(&app), None);
    assert!(drain(&mut pty).is_empty());
}

#[tokio::test]
async fn flocks_own_writes_do_not_open_a_quiet_window() {
    let Rig {
        mut app,
        pane,
        pty: _,
    } = rig();
    claude_idle_for(&mut app, SETTLED);
    arm(&mut app, &pane, "carry on");
    let now = Instant::now();
    app.tick_self_compacts(now);
    app.tick_self_compacts(now + GAP);

    assert_eq!(
        runtime(&app).last_operator_input_at(),
        None,
        "flock's own writes must not open a quiet window — it would block its \
         own Enter"
    );
}

#[tokio::test]
async fn the_continuation_lands_in_the_panes_prompt_history() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, SETTLED);
    arm(&mut app, &pane, "Next: open the PR");
    request_compaction(&mut app, &mut pty);
    session_started_with(&mut app, &pane, "compact");
    deliver_continuation(&mut app, &mut pty);

    let history: Vec<&str> = terminal_const(&app)
        .prompt_history
        .iter()
        .map(|entry| entry.text.as_str())
        .collect();
    assert!(
        history.contains(&"Next: open the PR"),
        "the agent's own prompt is now a prompt in its session, and that panel \
         is where an operator looks to see what it told itself to do next: \
         {history:?}"
    );
}

/// B1: the timeout clock must not be able to expire during the very tick that
/// submits the command. An agent arms mid-turn, so a turn longer than the
/// timeout used to leave `/compact` typed and then dropped before its Enter.
///
/// Driven in real time with a tiny timeout rather than by moving the clock
/// forward: a synthetic future collides with the hook's own real clock, and a
/// test that has to reconcile two clocks is testing the harness, not the rule.
#[tokio::test]
async fn a_turn_longer_than_the_timeout_still_completes() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    app.state.config.session.self_compact_timeout_ms = 1;
    // Working, so the turn outlasts the timeout.
    terminal(&mut app).set_detected_state_with_screen_signals_at(
        Some(Agent::Claude),
        AgentState::Working,
        false,
        false,
        false,
        false,
        Instant::now(),
    );
    arm(&mut app, &pane, "carry on");
    std::thread::sleep(Duration::from_millis(40));
    app.tick_self_compacts(Instant::now());
    assert!(
        written(&mut pty).is_empty(),
        "still mid-turn: nothing may be typed"
    );

    // The turn ends, far later than the timeout measured from arming would
    // allow.
    claude_idle_for(&mut app, SETTLED);
    app.tick_self_compacts(Instant::now());
    assert!(
        written(&mut pty).contains("/compact"),
        "a long turn must not eat the timeout: nothing was waiting on the \
         harness until now, so nothing could time out"
    );
    app.tick_self_compacts(Instant::now() + GAP);
    session_started_with(&mut app, &pane, "compact");
    deliver_continuation(&mut app, &mut pty);
    assert!(
        written(&mut pty).contains("carry on"),
        "and the sequence still completes"
    );
}

/// B2: every success used to be logged as a `continuation_timeout` once its
/// clock ran out, because the arming outlived the sequence it belonged to.
#[tokio::test]
async fn a_successful_run_does_not_wedge_the_pane() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, SETTLED);
    arm(&mut app, &pane, "carry on");
    request_compaction(&mut app, &mut pty);
    session_started_with(&mut app, &pane, "compact");
    deliver_continuation(&mut app, &mut pty);

    assert_eq!(armed_continuation(&app), None, "the arming is gone");

    // Long past when the old clock would have fired.
    let timeout = Duration::from_millis(app.state.config.session.self_compact_timeout_ms);
    app.tick_self_compacts(Instant::now() + timeout * 2);
    assert_eq!(armed_continuation(&app), None);

    // And the next arming is answered on its own merits, not refused as
    // `pending` by a sequence that already finished.
    let floor = Duration::from_secs(app.state.config.session.self_compact_min_interval_secs);
    app.state.config.session.self_compact_min_interval_secs = 0;
    app.terminal_mut_for_test(WS, app.state.workspaces[WS].focused_pane_id().unwrap())
        .last_self_compact_completed = Some(Instant::now() - floor);
    let reply = arm(&mut app, &pane, "and then the docs");
    assert!(
        reply.contains("\"state\":\"armed\""),
        "a finished sequence must not refuse the next arming: {reply}"
    );
}

/// B3: a draft a human typed while the compaction ran must never be submitted
/// together with the agent's own handoff prompt.
#[tokio::test]
async fn a_draft_typed_during_compaction_is_not_submitted_with_the_continuation() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, SETTLED);
    arm(&mut app, &pane, "carry on");
    request_compaction(&mut app, &mut pty);
    session_started_with(&mut app, &pane, "compact");

    // A human starts typing while the compaction is in flight.
    runtime(&app).test_process_pty_bytes(&claude_screen("half a thought"));
    deliver_continuation(&mut app, &mut pty);
    assert!(
        written(&mut pty).is_empty(),
        "an occupied prompt box is never typed into — submitting it would \
         submit the human's draft too"
    );
    assert_eq!(
        armed_continuation(&app).as_deref(),
        Some("carry on"),
        "the prompt is kept, not discarded: the human clears the box and it \
         goes on a later tick"
    );

    // The human clears it, and the prompt is delivered.
    claude_idle_for(&mut app, SETTLED);
    runtime(&app).test_process_pty_bytes(&claude_screen(""));
    app.terminal_mut_for_test(WS, app.state.workspaces[WS].focused_pane_id().unwrap())
        .state_changed_at = Some(Instant::now() - SETTLED);
    runtime(&app).test_stamp_operator_input_at(Instant::now() - Duration::from_secs(60));
    deliver_continuation(&mut app, &mut pty);
    assert!(written(&mut pty).contains("carry on"));
}

/// The idle wake and a self-compaction both type flock-authored text and press
/// Enter one `pane run` gap later. A gap is the same length for both, so two
/// writes in the same window are two submissions the pane cannot tell apart.
#[tokio::test]
async fn the_idle_wake_in_flight_defers_the_compaction_rather_than_interleaving() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, SETTLED);
    arm(&mut app, &pane, "carry on");

    // Something the idle wake already typed is still waiting for its Enter.
    app.test_arm_idle_wake_in_flight(&pane);
    app.tick_self_compacts(Instant::now());
    assert!(
        written(&mut pty).is_empty(),
        "two flock submissions in one gap go in as one line"
    );
    assert_eq!(armed_continuation(&app).as_deref(), Some("carry on"));
}

/// B4: a saved continuation must survive intact. Even though `encode_api_text`
/// strips paste delimiters, arming refuses controls rather than changing the
/// continuation, and also protects the non-bracketed raw-byte path.
#[test]
fn a_continuation_cannot_break_out_of_the_paste_or_run_as_a_command() {
    use crate::agent_self_compact::{check_continuation, ContinuationProblem as P};
    // The paste terminator, then raw keys.
    assert_eq!(check_continuation("ok\x1b[201~\u{1b}"), Err(P::Control));
    // A bare CSI, a bare ESC, and an OSC.
    assert_eq!(check_continuation("ok\x1b[2J"), Err(P::Control));
    assert_eq!(check_continuation("ok\x1b"), Err(P::Control));
    assert_eq!(check_continuation("ok\x1b]0;title\x07"), Err(P::Control));
    // A control byte with no escape at all — the bracketed-paste-off case,
    // where the text is sent as raw bytes.
    assert_eq!(check_continuation("ok\u{7}"), Err(P::Control));
    // A newline submits the prompt early, and a submit nobody gated is the one
    // outcome the empty-box gate exists to prevent.
    assert_eq!(check_continuation("ok\nrm -rf /"), Err(P::LineBreak));
    assert_eq!(check_continuation("ok\r"), Err(P::LineBreak));
    // Claude Code executes these rather than reading them.
    assert_eq!(check_continuation("/compact"), Err(P::HarnessCommand));
    assert_eq!(check_continuation("  !rm -rf /"), Err(P::HarnessCommand));
    // Ordinary prose, and a slash or bang that is NOT leading, is fine.
    assert_eq!(
        check_continuation("run just check, then a/b and c!"),
        Ok(())
    );
}
