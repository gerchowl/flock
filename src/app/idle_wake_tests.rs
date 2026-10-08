//! The idle wake end to end: a `msg.send` arrives over the socket and the
//! assertion is on the bytes that reach the recipient pane's PTY channel.
//! Nothing here constructs the wake's own state — AGENTS.md's #328 lesson.

use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::mpsc;

use crate::api::schema::{
    MessageTarget, Method, MsgIntent, MsgMuteParams, MsgReadParams, MsgSendParams,
    PaneSendTextParams, Request,
};
use crate::app::App;
use crate::detect::{Agent, AgentState};

/// A body no wake may ever carry. If it shows up in a pane, sender text
/// reached a wake channel — the thing ADR-0008 exists to prevent.
const MARKER: &str = "ZEBRA-7731-sender-words";

const GAP: Duration = crate::cli::pane::PANE_RUN_SUBMIT_GAP;

/// Claude's input box, as the pane would draw it, with `typed` in it.
fn claude_screen(typed: &str) -> Vec<u8> {
    let rule = "─".repeat(40);
    format!("\x1b[2J\x1b[HTask complete.\r\n{rule}\r\n❯ {typed}\r\n{rule}\r\n").into_bytes()
}

fn runtime(app: &App) -> &crate::terminal::TerminalRuntime {
    app.lookup_runtime_sender(1, app.state.workspaces[1].focused_pane_id().unwrap())
        .unwrap()
}

struct Rig {
    app: App,
    /// The recipient pane's public id.
    pane: String,
    /// Everything written into the recipient pane.
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
    let mut recipient = crate::workspace::Workspace::test_new("beta");
    let focused = recipient.focused_pane_id().expect("pane");
    let (runtime, pty) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
    runtime.test_process_pty_bytes(&claude_screen(""));
    recipient.tabs[0].runtimes.insert(focused, runtime);
    app.state.workspaces = vec![sender, recipient];
    app.state.ensure_test_terminals();
    app.state.active = Some(0);
    let pane = app.public_pane_id(1, focused).expect("public id");
    Rig { app, pane, pty }
}

fn terminal(app: &mut App) -> &mut crate::terminal::TerminalState {
    let pane_id = app.state.workspaces[1].focused_pane_id().expect("pane");
    let terminal_id = app
        .state
        .terminal_id_for_pane(1, pane_id)
        .expect("terminal");
    app.state
        .terminals
        .get_mut(&terminal_id)
        .expect("terminal state")
}

/// Claude, showing its idle prompt box, since `idle_for` ago, and seen just
/// now — the state an idle wake is for.
fn claude_idle_for(app: &mut App, idle_for: Duration) {
    let now = Instant::now();
    let terminal = terminal(app);
    for at in [now - idle_for, now] {
        terminal.set_detected_state_with_screen_signals_at(
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

fn settled() -> Duration {
    Duration::from_secs(30)
}

fn send(app: &mut App, pane: &str, correlation: &str, intent: MsgIntent) -> String {
    app.handle_api_request(Request {
        id: "req".into(),
        method: Method::MsgSend(MsgSendParams {
            from_agent: None,
            from_host: None,
            to: MessageTarget::Pane { pane: pane.into() },
            body: format!("please look at this: {MARKER}"),
            correlation_id: Some(correlation.into()),
            in_reply_to: None,
            intent,
            intent_unrecognised: None,
        }),
    })
}

fn drain(pty: &mut mpsc::Receiver<Bytes>) -> Vec<Vec<u8>> {
    let mut writes = Vec::new();
    while let Ok(bytes) = pty.try_recv() {
        writes.push(bytes.to_vec());
    }
    writes
}

/// Run the loop tick far enough past the typing for the Enter to be due.
fn tick_past_gap(app: &mut App) {
    // Echo the fake child's consumed paste, as a real composer does before Enter.
    if let Some(flight) = app
        .idle_wake
        .panes
        .values()
        .find_map(|entry| entry.in_flight.as_ref())
    {
        if let Some(attempt) = &flight.attempt {
            if crate::detect::agent_prompt_is_empty(Agent::Claude, &runtime(app).detection_text())
                == Some(true)
            {
                runtime(app).test_process_pty_bytes(&claude_screen(&attempt.text));
            }
        }
    }
    app.tick_idle_wakes(Instant::now() + GAP + Duration::from_millis(5));
}

fn read_inbox(app: &mut App, pane: &str) {
    app.handle_api_request(Request {
        id: "req".into(),
        method: Method::MsgRead(MsgReadParams {
            pane: Some(pane.into()),
        }),
    });
}

#[tokio::test]
async fn a_needs_reply_to_an_idle_claude_types_the_constant_then_submits_it_once() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, settled());

    let response = send(&mut app, &pane, "c-1", MsgIntent::NeedsReply);
    assert!(response.contains("\"queued\""), "{response}");

    // Enqueue types the sentence at once — and ONLY the sentence.
    let typed = drain(&mut pty);
    assert_eq!(
        typed,
        vec![super::idle_wake_text(1).into_bytes()],
        "the typed wake is the constant with a count, nothing else"
    );
    let typed_text = String::from_utf8(typed[0].clone()).unwrap();
    assert!(!typed_text.contains(MARKER), "sender text reached the pane");
    assert!(!typed_text.contains('\r') && !typed_text.contains('\n'));

    // The Enter is its own write, one gap later (#362).
    app.tick_idle_wakes(Instant::now());
    assert!(drain(&mut pty).is_empty(), "no Enter before the gap");
    tick_past_gap(&mut app);
    assert_eq!(drain(&mut pty), vec![b"\r".to_vec()]);

    // Once per queued set: more ticks, and more mail while the first wake is
    // in flight, type nothing.
    for _ in 0..5 {
        tick_past_gap(&mut app);
    }
    send(&mut app, &pane, "c-2", MsgIntent::NeedsReply);
    tick_past_gap(&mut app);
    assert!(drain(&mut pty).is_empty(), "a wake must never be re-typed");
}

#[tokio::test]
async fn an_fyi_never_costs_a_turn() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, settled());
    send(&mut app, &pane, "c-1", MsgIntent::Fyi);
    tick_past_gap(&mut app);
    assert!(drain(&mut pty).is_empty());
}

/// ADR-0018 §1's reply rule, through the idle wake: the recipient asked a
/// question, went idle, and the answer comes back stamped `fyi` (a reply's
/// default). It has to reach the asker, or the question was asked and never
/// heard; the mute's deferral, also `in_reply_to` the question, must not.
#[tokio::test]
async fn an_fyi_answer_to_its_own_question_idle_wakes_the_asker_but_a_deferral_does_not() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    let other = {
        let pane_id = app.state.workspaces[0].focused_pane_id().expect("pane");
        app.public_pane_id(0, pane_id).expect("public id")
    };
    let wire = |app: &mut App, to: &str, cid: &str, in_reply_to: Option<&str>, intent: &str| {
        app.handle_api_request(
            serde_json::from_value(serde_json::json!({
                "id": "req",
                "method": "msg.send",
                "params": {
                    "to": {"type": "pane", "pane": to},
                    "body": format!("the answer: {MARKER}"),
                    "correlation_id": cid,
                    "in_reply_to": in_reply_to,
                    "intent": intent,
                },
            }))
            .expect("a request a client sends"),
        )
    };
    // The asker's question, read on the other side.
    wire(&mut app, &other, "c-q", None, "needs_reply");
    read_inbox(&mut app, &other);
    claude_idle_for(&mut app, settled());

    wire(&mut app, &pane, "c-q:deferred", Some("c-q"), "fyi");
    tick_past_gap(&mut app);
    assert!(drain(&mut pty).is_empty(), "a deferral carries no answer");

    wire(&mut app, &pane, "c-a", Some("c-q"), "fyi");
    let typed = drain(&mut pty);
    assert_eq!(typed, vec![super::idle_wake_text(2).into_bytes()]);
    assert!(!String::from_utf8_lossy(&typed[0]).contains(MARKER));
}

#[tokio::test]
async fn the_count_includes_waiting_fyis_but_nothing_else_about_them() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, settled());
    send(&mut app, &pane, "c-1", MsgIntent::Fyi);
    send(&mut app, &pane, "c-2", MsgIntent::Fyi);
    send(&mut app, &pane, "c-3", MsgIntent::NeedsReply);
    assert_eq!(drain(&mut pty), vec![super::idle_wake_text(3).into_bytes()]);
}

#[tokio::test]
async fn an_agent_that_just_went_idle_is_woken_only_once_it_has_settled() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, Duration::ZERO);
    send(&mut app, &pane, "c-1", MsgIntent::NeedsReply);
    assert!(
        drain(&mut pty).is_empty(),
        "an Idle that just flipped is not settled"
    );
    assert!(
        app.idle_wake.next_deadline().is_some(),
        "the loop must be told when the settle window ends"
    );

    // The tick picks it up once the window has passed and the screen still
    // says idle.
    std::thread::sleep(Duration::from_millis(
        app.state.config.msg.idle_wake_settle_ms + 20,
    ));
    claude_idle_for(&mut app, Duration::ZERO);
    app.tick_idle_wakes(Instant::now());
    assert_eq!(drain(&mut pty), vec![super::idle_wake_text(1).into_bytes()]);
}

#[tokio::test]
async fn a_working_agent_is_not_typed_into() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    let now = Instant::now();
    terminal(&mut app).set_detected_state_with_screen_signals_at(
        Some(Agent::Claude),
        AgentState::Working,
        false,
        false,
        true,
        false,
        now - settled(),
    );
    send(&mut app, &pane, "c-1", MsgIntent::NeedsReply);
    tick_past_gap(&mut app);
    assert!(drain(&mut pty).is_empty());
}

#[tokio::test]
async fn an_idle_the_screen_does_not_show_is_not_idle() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    // Idle by default — the detector found no working chrome — but no
    // visible prompt box: an unreadable screen (#311).
    let now = Instant::now();
    let terminal = terminal(&mut app);
    for at in [now - settled(), now] {
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Claude),
            AgentState::Idle,
            false,
            false,
            false,
            false,
            at,
        );
    }
    send(&mut app, &pane, "c-1", MsgIntent::NeedsReply);
    tick_past_gap(&mut app);
    assert!(drain(&mut pty).is_empty());
}

#[tokio::test]
async fn a_stale_screen_observation_is_not_idle() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    // Last seen idle long ago, and not re-observed since: the detector has
    // stopped telling us anything.
    terminal(&mut app).set_detected_state_with_screen_signals_at(
        Some(Agent::Claude),
        AgentState::Idle,
        false,
        true,
        false,
        false,
        Instant::now() - settled(),
    );
    send(&mut app, &pane, "c-1", MsgIntent::NeedsReply);
    tick_past_gap(&mut app);
    assert!(drain(&mut pty).is_empty());
}

#[tokio::test]
async fn an_agent_without_the_inbox_tool_stays_pull_only() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    let now = Instant::now();
    let terminal = terminal(&mut app);
    for at in [now - settled(), now] {
        terminal.set_detected_state_with_screen_signals_at(
            Some(Agent::Codex),
            AgentState::Idle,
            false,
            true,
            false,
            false,
            at,
        );
    }
    send(&mut app, &pane, "c-1", MsgIntent::NeedsReply);
    tick_past_gap(&mut app);
    assert!(drain(&mut pty).is_empty());
}

#[tokio::test]
async fn a_muted_recipient_is_not_woken_until_the_mute_lifts() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, settled());
    app.handle_api_request(Request {
        id: "req".into(),
        method: Method::MsgMute(MsgMuteParams {
            pane: Some(pane.clone()),
            seconds: 60,
            reason: None,
        }),
    });
    send(&mut app, &pane, "c-1", MsgIntent::NeedsReply);
    tick_past_gap(&mut app);
    assert!(drain(&mut pty).is_empty(), "muted");
    assert!(
        app.idle_wake.next_deadline().is_some(),
        "wake when it lifts"
    );

    app.mailboxes.set_mute(&pane, 0, 0, None);
    claude_idle_for(&mut app, settled());
    app.tick_idle_wakes(Instant::now());
    assert_eq!(
        drain(&mut pty),
        vec![super::idle_wake_text(1).into_bytes()],
        "mail that became wakeable later is picked up by the tick"
    );
}

#[tokio::test]
async fn a_paused_fleet_types_nothing() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, settled());
    app.fleet_pause.paused = true;
    send(&mut app, &pane, "c-1", MsgIntent::NeedsReply);
    tick_past_gap(&mut app);
    assert!(drain(&mut pty).is_empty());
}

#[tokio::test]
async fn the_kill_switch_turns_it_off() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    app.state.config.msg.idle_wake = false;
    claude_idle_for(&mut app, settled());
    send(&mut app, &pane, "c-1", MsgIntent::NeedsReply);
    tick_past_gap(&mut app);
    assert!(drain(&mut pty).is_empty());
}

#[tokio::test]
async fn operator_input_over_the_api_holds_the_wake_for_the_quiet_window() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, settled());
    // `pane.send-text` is a human (or a human's script) at the keyboard.
    app.handle_api_request(Request {
        id: "req".into(),
        method: Method::PaneSendText(PaneSendTextParams {
            pane_id: pane.clone(),
            text: "half a thought".into(),
        }),
    });
    drain(&mut pty);

    send(&mut app, &pane, "c-1", MsgIntent::NeedsReply);
    tick_past_gap(&mut app);
    assert!(
        drain(&mut pty).is_empty(),
        "flock must not type over a human"
    );
    assert!(app.idle_wake.next_deadline().is_some());

    // Once the operator has been quiet for the window, the wake goes.
    let quiet = Duration::from_millis(app.state.config.msg.idle_wake_operator_quiet_ms);
    let runtime = runtime(&app);
    runtime.test_stamp_operator_input_at(Instant::now() - quiet - Duration::from_secs(1));
    app.tick_idle_wakes(Instant::now());
    assert_eq!(drain(&mut pty), vec![super::idle_wake_text(1).into_bytes()]);
}

#[tokio::test]
async fn keystrokes_from_an_attached_client_count_as_operator_input() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, settled());
    app.state.active = Some(1);
    app.state.selected = 1;
    app.state.mode = crate::app::Mode::Terminal;
    app.route_client_input(b"h".to_vec());
    assert!(
        !drain(&mut pty).is_empty(),
        "the keystroke reached the pane"
    );

    send(&mut app, &pane, "c-1", MsgIntent::NeedsReply);
    tick_past_gap(&mut app);
    assert!(drain(&mut pty).is_empty());
}

#[tokio::test]
async fn flocks_own_wake_does_not_count_as_operator_input() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, settled());
    send(&mut app, &pane, "c-1", MsgIntent::NeedsReply);
    tick_past_gap(&mut app);
    assert_eq!(drain(&mut pty).len(), 2, "sentence, then Enter");

    let runtime = runtime(&app);
    assert_eq!(
        runtime.last_operator_input_at(),
        None,
        "the wake's own writes must not open a quiet window — it would block \
         its own Enter"
    );
}

#[tokio::test]
async fn a_human_who_types_between_the_sentence_and_the_enter_gets_no_enter() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, settled());
    send(&mut app, &pane, "c-1", MsgIntent::NeedsReply);
    assert_eq!(drain(&mut pty).len(), 1, "sentence typed");

    app.handle_api_request(Request {
        id: "req".into(),
        method: Method::PaneSendText(PaneSendTextParams {
            pane_id: pane.clone(),
            text: "x".into(),
        }),
    });
    drain(&mut pty);
    tick_past_gap(&mut app);
    tick_past_gap(&mut app);
    assert!(
        drain(&mut pty).is_empty(),
        "submitting a prompt a human is editing is the one unrecoverable outcome"
    );
}

#[tokio::test]
async fn reading_the_inbox_or_leaving_idle_ends_the_flight_and_only_new_mail_wakes_again() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, settled());
    send(&mut app, &pane, "c-1", MsgIntent::NeedsReply);
    tick_past_gap(&mut app);
    assert_eq!(drain(&mut pty).len(), 2);

    // The agent took a turn and came back idle WITHOUT reading. The message it
    // was already told about is not re-typed — the stop hook re-told it.
    let now = Instant::now();
    terminal(&mut app).set_detected_state_with_screen_signals_at(
        Some(Agent::Claude),
        AgentState::Working,
        false,
        false,
        true,
        false,
        now - settled(),
    );
    app.tick_idle_wakes(Instant::now() + GAP + Duration::from_millis(100));
    claude_idle_for(&mut app, settled());
    tick_past_gap(&mut app);
    assert!(drain(&mut pty).is_empty(), "announced mail is not re-typed");

    runtime(&app).test_process_pty_bytes(&claude_screen(""));
    // A NEW message after the agent left Idle earns its own wake.
    send(&mut app, &pane, "c-2", MsgIntent::NeedsReply);
    assert_eq!(drain(&mut pty), vec![super::idle_wake_text(2).into_bytes()]);
    tick_past_gap(&mut app);
    drain(&mut pty);

    runtime(&app).test_process_pty_bytes(&claude_screen(""));
    // Reading clears everything; the next message starts fresh.
    read_inbox(&mut app, &pane);
    assert!(app.idle_wake.panes.is_empty());
    claude_idle_for(&mut app, settled());
    send(&mut app, &pane, "c-3", MsgIntent::NeedsReply);
    assert_eq!(drain(&mut pty), vec![super::idle_wake_text(1).into_bytes()]);
}

#[test]
fn the_sentence_is_the_adr_constant() {
    assert_eq!(
        super::idle_wake_text(2),
        "You have 2 unread message(s) from other agents. Read them with the \
         `flock_msg_read` tool."
    );
}

#[tokio::test]
async fn a_draft_left_in_the_prompt_is_never_typed_next_to() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, settled());
    // Typed long ago — outside any quiet window — and never sent.
    runtime(&app).test_process_pty_bytes(&claude_screen("half a thought"));
    send(&mut app, &pane, "c-1", MsgIntent::NeedsReply);
    tick_past_gap(&mut app);
    assert!(
        drain(&mut pty).is_empty(),
        "the Enter would have submitted someone's draft with flock's sentence"
    );
}

#[tokio::test]
async fn a_screen_that_goes_stale_inside_the_gap_gets_no_enter() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, settled());
    send(&mut app, &pane, "c-1", MsgIntent::NeedsReply);
    assert_eq!(drain(&mut pty).len(), 1, "sentence typed");

    // The Enter comes due long after the last screen observation.
    let fresh = Duration::from_millis(app.state.config.msg.idle_wake_fresh_ms);
    app.tick_idle_wakes(Instant::now() + fresh + GAP + Duration::from_millis(5));
    assert!(drain(&mut pty).is_empty());
}

#[tokio::test]
async fn reading_the_inbox_inside_the_gap_drops_the_enter() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, settled());
    send(&mut app, &pane, "c-1", MsgIntent::NeedsReply);
    assert_eq!(drain(&mut pty).len(), 1);
    read_inbox(&mut app, &pane);
    tick_past_gap(&mut app);
    assert!(drain(&mut pty).is_empty());
    assert_eq!(app.delivery_attempts()[0].state, "abandoned");
    assert_eq!(
        app.delivery_attempts()[0].reason.as_deref(),
        Some("inbox_read")
    );
}

/// A message relayed in from another host arrives as a `msg.send` carrying
/// the far sender's identity, and must wake exactly as a local one does — on
/// the recipient's own server, where its state is known (ADR-0018 §5).
#[tokio::test]
async fn a_message_relayed_from_another_host_wakes_the_same_way() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, settled());
    let response = app.handle_api_request(Request {
        id: "req".into(),
        method: Method::MsgSend(MsgSendParams {
            from_agent: Some("agent_kiln_far".into()),
            from_host: Some("kiln".into()),
            to: MessageTarget::Pane { pane: pane.clone() },
            body: format!("from the other host: {MARKER}"),
            correlation_id: Some("relayed-1".into()),
            in_reply_to: None,
            intent: MsgIntent::NeedsReply,
            intent_unrecognised: None,
        }),
    });
    assert!(response.contains("\"queued\""), "{response}");
    let typed = drain(&mut pty);
    assert_eq!(typed, vec![super::idle_wake_text(1).into_bytes()]);
    assert!(!String::from_utf8_lossy(&typed[0]).contains("toad"));
    tick_past_gap(&mut app);
    assert_eq!(drain(&mut pty), vec![b"\r".to_vec()]);
}

/// #438: under channel push the push knocks first. The idle wake holds off
/// for the grace window, so a push that started a turn leaves nothing to
/// type — and a session that never registered the channel is still woken,
/// once the window has passed.
#[tokio::test]
async fn under_channel_push_the_idle_wake_waits_out_the_grace_then_still_falls_back() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    app.state.config.msg.channel_push = true;
    app.state.config.msg.channel_push_idle_wake_grace_ms = 60_000;
    claude_idle_for(&mut app, settled());

    send(&mut app, &pane, "c-1", MsgIntent::NeedsReply);
    tick_past_gap(&mut app);
    assert!(drain(&mut pty).is_empty(), "nothing typed inside the grace");

    // No push landed (the session never registered the channel): once the
    // grace has passed, the ordinary wake types the constant.
    app.mailboxes.test_age_all(61_000);
    claude_idle_for(&mut app, settled());
    app.tick_idle_wakes(Instant::now());
    assert_eq!(drain(&mut pty), vec![super::idle_wake_text(1).into_bytes()]);
}

#[tokio::test]
async fn under_channel_push_a_turn_the_push_started_is_never_typed_over() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    app.state.config.msg.channel_push = true;
    app.state.config.msg.channel_push_idle_wake_grace_ms = 60_000;
    claude_idle_for(&mut app, settled());
    send(&mut app, &pane, "c-1", MsgIntent::NeedsReply);

    // The push landed and Claude started working before the grace ran out.
    terminal(&mut app).set_detected_state_with_screen_signals_at(
        Some(Agent::Claude),
        AgentState::Working,
        false,
        true,
        false,
        false,
        Instant::now(),
    );
    app.mailboxes.test_age_all(61_000);
    tick_past_gap(&mut app);
    assert!(
        drain(&mut pty).is_empty(),
        "a working agent is not typed into"
    );
}

#[tokio::test]
async fn guarded_submit_retries_only_enter_once_and_reports_unconfirmed() {
    use super::super::guarded_submit::{Outcome, CONFIRM_WINDOW};
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, settled());
    let now = Instant::now();
    let mut attempt = app
        .begin_guarded_submit(&pane, "hello", None, Duration::ZERO, now, false)
        .unwrap();
    assert_eq!(drain(&mut pty), vec![b"hello".to_vec()]);
    runtime(&app).test_process_pty_bytes(&claude_screen("hello"));
    assert_eq!(app.advance_guarded_submit(&pane, &mut attempt, now), None);
    assert!(drain(&mut pty).is_empty());
    assert_eq!(
        app.advance_guarded_submit(&pane, &mut attempt, now + GAP),
        None
    );
    assert_eq!(drain(&mut pty), vec![b"\r".to_vec()]);
    assert_eq!(
        app.advance_guarded_submit(&pane, &mut attempt, now + GAP + CONFIRM_WINDOW),
        None
    );
    assert_eq!(drain(&mut pty), vec![b"\r".to_vec()]);
    assert_eq!(
        app.advance_guarded_submit(&pane, &mut attempt, now + GAP + CONFIRM_WINDOW * 2),
        Some(Outcome::Unconfirmed("confirm_timeout"))
    );
    assert!(drain(&mut pty).is_empty());
}

#[tokio::test]
async fn guarded_submit_never_retypes_when_composer_clears_or_dialog_appears() {
    use super::super::guarded_submit::Outcome;
    for dialog in [false, true] {
        let Rig {
            mut app,
            pane,
            mut pty,
        } = rig();
        claude_idle_for(&mut app, settled());
        let now = Instant::now();
        let mut attempt = app
            .begin_guarded_submit(&pane, "hello", None, Duration::ZERO, now, false)
            .unwrap();
        drain(&mut pty);
        runtime(&app).test_process_pty_bytes(&claude_screen("hello"));
        assert_eq!(
            app.advance_guarded_submit(&pane, &mut attempt, now + GAP),
            None
        );
        assert_eq!(drain(&mut pty), vec![b"\r".to_vec()]);
        let screen = if dialog {
            [
                claude_screen("hello"),
                "\r\nEnter to select · Esc to cancel".as_bytes().to_vec(),
            ]
            .concat()
        } else {
            claude_screen("")
        };
        runtime(&app).test_process_pty_bytes(&screen);
        assert_eq!(
            app.advance_guarded_submit(
                &pane,
                &mut attempt,
                now + GAP + super::super::guarded_submit::CONFIRM_WINDOW
            ),
            Some(Outcome::Unconfirmed("owned_composer_not_visible"))
        );
        assert!(drain(&mut pty).is_empty());
    }
}

#[tokio::test]
async fn guarded_submit_refuses_drafts_and_concurrent_input() {
    use super::super::guarded_submit::Outcome;
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, settled());
    runtime(&app).test_process_pty_bytes(&claude_screen("human draft"));
    assert_eq!(
        app.begin_guarded_submit(&pane, "hello", None, Duration::ZERO, Instant::now(), false)
            .unwrap_err(),
        "input_not_empty"
    );
    assert!(drain(&mut pty).is_empty());
    runtime(&app).test_process_pty_bytes(&claude_screen(""));
    let now = Instant::now();
    let mut attempt = app
        .begin_guarded_submit(&pane, "hello", None, Duration::ZERO, now, false)
        .unwrap();
    drain(&mut pty);
    runtime(&app)
        .try_send_bytes(Bytes::from_static(b"human edit"))
        .unwrap();
    drain(&mut pty);
    assert_eq!(
        app.advance_guarded_submit(&pane, &mut attempt, now + GAP),
        Some(Outcome::Abandoned("operator_active"))
    );
    assert!(drain(&mut pty).is_empty());
}

#[tokio::test]
async fn guarded_submit_confirms_matching_prompt_and_short_working_turn() {
    use super::super::guarded_submit::Outcome;
    for (text, hook, sent) in [
        ("hello", true, true),
        ("hello", true, false),
        ("  hello\n world  ", true, true),
        ("hello", false, true),
        ("hello", false, false),
    ] {
        let Rig {
            mut app,
            pane,
            mut pty,
        } = rig();
        claude_idle_for(&mut app, settled());
        runtime(&app).test_process_pty_bytes(b"\x1b[?2004h");
        let now = Instant::now();
        let mut attempt = app
            .begin_guarded_submit(&pane, text, None, Duration::ZERO, now, false)
            .unwrap();
        drain(&mut pty);
        if sent {
            runtime(&app).test_process_pty_bytes(&claude_screen(text));
            assert_eq!(
                app.advance_guarded_submit(&pane, &mut attempt, now + GAP),
                None
            );
            assert_eq!(drain(&mut pty), vec![b"\r".to_vec()]);
        }
        if hook {
            let reported = app.handle_api_request(Request {
                id: "prompt".into(),
                method: Method::PaneReportPrompt(crate::api::schema::PaneReportPromptParams {
                    pane_id: pane.clone(),
                    source: "flock:claude".into(),
                    agent: "claude".into(),
                    prompt: text.into(),
                    seq: Some(1),
                }),
            });
            assert!(reported.contains("\"ok\""), "{reported}");
        } else {
            terminal(&mut app).set_detected_state_with_screen_signals_at(
                Some(Agent::Claude),
                AgentState::Working,
                false,
                false,
                true,
                false,
                now,
            );
            terminal(&mut app).set_detected_state_with_screen_signals_at(
                Some(Agent::Claude),
                AgentState::Idle,
                false,
                true,
                false,
                false,
                now,
            );
        }
        assert_eq!(
            app.advance_guarded_submit(
                &pane,
                &mut attempt,
                if !sent && hook {
                    now + GAP / 2
                } else {
                    now + GAP
                }
            ),
            Some(if !sent {
                Outcome::Abandoned("turn_started_before_enter")
            } else if hook {
                Outcome::Accepted
            } else {
                Outcome::ObservedAccepted
            })
        );
        let evidence = app.delivery_attempts();
        assert_eq!(evidence.len(), 1);
        assert_eq!(
            evidence[0].state,
            if !sent {
                "abandoned"
            } else if hook {
                "accepted"
            } else {
                "observed_accepted"
            }
        );
        assert!(evidence[0].finished_at_ms.is_some());
        assert_eq!(evidence[0].submit_sent_at_ms.is_some(), sent);
        assert!(drain(&mut pty).is_empty());
    }
}

#[tokio::test]
async fn guarded_client_submit_is_atomic_with_session_settle_and_serialization_gates() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, settled());
    let now = Instant::now();
    assert_eq!(
        app.begin_guarded_submit(
            &pane,
            "hello",
            Some("another-session"),
            Duration::ZERO,
            now,
            false
        )
        .unwrap_err(),
        "session_mismatch"
    );
    assert_eq!(
        app.begin_guarded_submit(&pane, "hello", None, settled() * 2, now, false)
            .unwrap_err(),
        "not_settled"
    );
    assert!(drain(&mut pty).is_empty());
    let submit = |id: &str| Request {
        id: id.into(),
        method: Method::PaneSubmit(crate::api::schema::PaneSubmitParams {
            self_submit_confirmed: None,
            pane_id: pane.clone(),
            text: "hello".into(),
            if_session: None,
            min_age_secs: 0,
        }),
    };
    let first = app.handle_api_request(submit("first"));
    assert!(first.contains("\"ok\""), "{first}");
    let _parked = app.pending_agent_submit.take().unwrap();
    let competing = app.handle_api_request(submit("competing"));
    assert!(competing.contains("injection_pending"), "{competing}");
    assert_eq!(drain(&mut pty), vec![b"hello".to_vec()]);
}

#[tokio::test]
async fn guarded_client_rechecks_session_and_settle_before_enter() {
    use super::super::guarded_submit::Outcome;
    for session_change in [false, true] {
        let Rig {
            mut app,
            pane,
            mut pty,
        } = rig();
        claude_idle_for(&mut app, settled());
        let report = |session: &str, seq| Request {
            id: "session".into(),
            method: Method::PaneReportAgentSession(
                crate::api::schema::PaneReportAgentSessionParams {
                    pane_id: pane.clone(),
                    source: "flock:claude".into(),
                    agent: "claude".into(),
                    seq: Some(seq),
                    agent_session_id: Some(session.into()),
                    agent_session_path: None,
                    session_start_source: Some("resume".into()),
                },
            ),
        };
        assert!(app
            .handle_api_request(report("first-session", 1))
            .contains("\"ok\""));
        let now = Instant::now();
        let mut attempt = app
            .begin_guarded_submit(
                &pane,
                "hello",
                Some("first-session"),
                Duration::from_secs(1),
                now,
                false,
            )
            .unwrap();
        assert_eq!(drain(&mut pty), vec![b"hello".to_vec()]);
        runtime(&app).test_process_pty_bytes(&claude_screen("hello"));
        if session_change {
            assert!(app
                .handle_api_request(report("second-session", 2))
                .contains("\"ok\""));
        } else {
            for state in [AgentState::Blocked, AgentState::Idle] {
                terminal(&mut app).set_detected_state_with_screen_signals_at(
                    Some(Agent::Claude),
                    state,
                    false,
                    state == AgentState::Idle,
                    false,
                    false,
                    now,
                );
            }
        }
        assert_eq!(
            app.advance_guarded_submit(&pane, &mut attempt, now + GAP),
            Some(Outcome::Abandoned(if session_change {
                "session_mismatch"
            } else {
                "not_settled"
            }))
        );
        assert!(drain(&mut pty).is_empty());
    }
}

#[tokio::test]
async fn guarded_dispatch_requires_consuming_deferred_attempt() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, settled());
    let response = app.handle_api_request(Request {
        id: "first".into(),
        method: Method::PaneSubmit(crate::api::schema::PaneSubmitParams {
            self_submit_confirmed: None,
            pane_id: pane.clone(),
            text: "hello".into(),
            if_session: None,
            min_age_secs: 0,
        }),
    });
    assert!(response.contains("ok"));
    let (_, reserved, _) = app.pending_agent_submit.as_ref().unwrap();
    assert!(app.active_submissions.contains(reserved));
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(4);
    app.event_tx = event_tx;
    let (sender, receiver) = std::sync::mpsc::channel();
    app.respond_or_park(sender, response);
    assert!(app.pending_agent_submit.is_none());
    // A later dispatch must leave the scheduled attempt's reservation intact.
    app.handle_api_request(Request {
        id: "next".into(),
        method: Method::AgentList(crate::api::schema::EmptyParams {}),
    });
    assert!(app.active_submissions.contains(&pane));
    assert_eq!(drain(&mut pty), vec![b"hello".to_vec()]);
    let crate::events::AppEvent::AgentSubmit {
        request_id,
        pane_id,
        attempt,
        respond_to,
    } = event_rx.recv().await.unwrap()
    else {
        panic!("expected submit event")
    };
    runtime(&app).test_stamp_operator_input_at(Instant::now());
    app.advance_guarded_request(request_id, pane_id, attempt, respond_to);
    assert!(!app.active_submissions.contains(&pane));
    let response: serde_json::Value = serde_json::from_str(&receiver.recv().unwrap()).unwrap();
    assert_eq!(response["result"]["outcome"], "abandoned");
    assert_eq!(response["result"]["attempt"]["reason"], "operator_active");
    assert_eq!(response["result"]["attempt"]["pane"], pane);
    assert!(response["result"]["attempt"]["attempt_id"].is_string());
}

#[tokio::test]
async fn finished_idle_wakes_release_explicit_submit_reservation() {
    use super::super::guarded_submit::CONFIRM_WINDOW;
    for abandon in [true, false] {
        let Rig {
            mut app,
            pane,
            mut pty,
        } = rig();
        claude_idle_for(&mut app, settled());
        send(&mut app, &pane, "finished-wake", MsgIntent::NeedsReply);
        drain(&mut pty);
        assert!(app.idle_wake.in_flight(&pane));
        if abandon {
            runtime(&app).test_stamp_operator_input_at(Instant::now());
            tick_past_gap(&mut app);
        } else {
            tick_past_gap(&mut app);
            let now = Instant::now() + GAP + CONFIRM_WINDOW;
            app.tick_idle_wakes(now);
            app.tick_idle_wakes(now + CONFIRM_WINDOW);
        }
        assert!(!app.idle_wake.in_flight(&pane));
        assert!(app.idle_wake.panes[&pane].in_flight.is_none());
        runtime(&app).test_process_pty_bytes(&claude_screen(""));
        claude_idle_for(&mut app, settled());
        let result = app.begin_guarded_submit(
            &pane,
            "explicit",
            None,
            Duration::ZERO,
            Instant::now() + GAP,
            false,
        );
        assert!(result.is_ok(), "{result:?}");
    }
}

#[tokio::test]
async fn guarded_mcp_submit_requires_calling_workspace_confirmation() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, settled());
    let (ws, id) = app.parse_pane_id(&pane).unwrap();
    app.test_pane_child_pids.insert(id, std::process::id());
    app.current_api_peer_pid = Some(std::process::id());
    let response = app.handle_api_request(Request {
        id: "mcp".into(),
        method: Method::PaneSubmit(crate::api::schema::PaneSubmitParams {
            pane_id: pane,
            text: "hello".into(),
            self_submit_confirmed: Some(false),
            if_session: None,
            min_age_secs: 0,
        }),
    });
    assert!(response.contains("self_submit_unconfirmed"), "{response}");
    assert_eq!(app.caller_workspace_idx(), Some(ws));
    assert!(drain(&mut pty).is_empty());
}

#[tokio::test]
async fn delivery_attempt_unconfirmed_survives_restart_without_replaying_and_late_read_wins() {
    use super::super::guarded_submit::CONFIRM_WINDOW;
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    let log = std::env::temp_dir().join(format!(
        "flock-attempt-{}-{}.jsonl",
        std::process::id(),
        crate::app::api::messages::now_ms()
    ));
    app.event_hub = crate::api::EventHub::with_persistence(log.clone());
    claude_idle_for(&mut app, settled());
    send(&mut app, &pane, "attempt-mail", MsgIntent::NeedsReply);
    let now = Instant::now();
    app.tick_idle_wakes(now);
    assert!(!drain(&mut pty).is_empty());
    let text = super::idle_wake_text(1);
    runtime(&app).test_process_pty_bytes(&claude_screen(&text));
    for at in [
        now + GAP,
        now + GAP + CONFIRM_WINDOW,
        now + GAP + CONFIRM_WINDOW * 2,
    ] {
        app.tick_idle_wakes(at);
        drain(&mut pty);
    }
    let attempts = app.delivery_attempts();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].state, "unconfirmed");
    assert_eq!(attempts[0].reason.as_deref(), Some("confirm_timeout"));
    assert!(attempts[0].retried);
    assert_eq!(app.mailboxes.queued_len(&pane), 1);
    let status = |app: &mut App| -> serde_json::Value {
        serde_json::from_str(&app.handle_api_request(Request {
            id: "status".into(),
            method: Method::MsgStatus(crate::api::schema::MsgStatusParams {
                correlation_id: "attempt-mail".into(),
            }),
        }))
        .unwrap()
    };
    // Polling must still find queued mail after transient events roll the ring.
    for revision in 0..4100 {
        app.event_hub.push(crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::PaneOutputChanged,
            data: crate::api::schema::EventData::PaneOutputChanged {
                pane_id: pane.clone(),
                workspace_id: "fixture-workspace".into(),
                revision,
            },
        });
    }
    let cold_reads = app.event_hub.cold_read_count();
    let before = status(&mut app);
    assert_eq!(before["result"]["state"], "queued");
    assert_eq!(before["result"]["attempts"][0]["state"], "unconfirmed");
    let listing = app.handle_api_request(Request {
        id: "list".into(),
        method: Method::MsgList(crate::api::schema::MsgListParams { pane: None }),
    });
    let listing: serde_json::Value = serde_json::from_str(&listing).unwrap();
    assert_eq!(
        listing["result"]["messages"][0]["attempts"][0]["state"],
        "unconfirmed"
    );
    assert_eq!(
        listing["result"]["messages"][0]["attempts"][0]["attempt_id"],
        attempts[0].attempt_id
    );
    assert_eq!(
        app.event_hub.cold_read_count(),
        cold_reads,
        "polling status/list must never scan disk"
    );
    let persisted = std::fs::read_to_string(&log).unwrap();
    for line in persisted
        .lines()
        .filter(|line| line.contains("delivery_attempt_updated"))
    {
        assert!(!line.contains(MARKER));
        assert!(!line.contains(&text));
    }
    app.event_hub = crate::api::EventHub::with_persistence(log.clone());
    let restored = app.event_hub.persisted_events_after(0);
    app.mailboxes = crate::app::mailboxes::MailboxRegistry::default();
    app.mailboxes
        .seed_from_events(restored.iter().map(|(_, _, event)| event));
    app.idle_wake = super::IdleWakeTracker::default();
    app.restore_delivery_attempts();
    assert_eq!(app.delivery_attempts(), attempts);
    runtime(&app).test_process_pty_bytes(&claude_screen(""));
    claude_idle_for(&mut app, settled());
    app.tick_idle_wakes(Instant::now());
    assert!(drain(&mut pty).is_empty());
    let read = app.handle_api_request(Request {
        id: "read".into(),
        method: Method::MsgRead(MsgReadParams {
            pane: Some(pane.clone()),
        }),
    });
    assert!(read.contains("attempt-mail"), "{read}");
    assert_eq!(status(&mut app)["result"]["state"], "read");
    let events = app.event_hub.persisted_events_after(0);
    let digest =
        crate::digest::categorize(events.iter().map(|(seq, ts, event)| (*seq, *ts, event)));
    assert!(digest
        .yellow
        .iter()
        .any(|row| row.summary.contains("confirm_timeout")
            && row.context.contains(&("pane".into(), pane.clone()))));
    assert_eq!(app.mailboxes.queued_len(&pane), 0);
    std::fs::remove_file(log).unwrap();
}

#[tokio::test]
async fn delivery_attempt_interrupted_restart_is_unconfirmed_and_deduplicated() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    let log = std::env::temp_dir().join(format!(
        "flock-attempt-{}-{}.jsonl",
        std::process::id(),
        crate::app::api::messages::now_ms()
    ));
    app.event_hub = crate::api::EventHub::with_persistence(log.clone());
    claude_idle_for(&mut app, settled());
    send(&mut app, &pane, "interrupted-mail", MsgIntent::NeedsReply);
    app.tick_idle_wakes(Instant::now());
    assert!(!drain(&mut pty).is_empty());
    assert_eq!(app.delivery_attempts()[0].state, "typed");
    app.event_hub = crate::api::EventHub::with_persistence(log.clone());
    let restored = app.event_hub.persisted_events_after(0);
    app.mailboxes = crate::app::mailboxes::MailboxRegistry::default();
    app.mailboxes
        .seed_from_events(restored.iter().map(|(_, _, event)| event));
    app.idle_wake = super::IdleWakeTracker::default();
    app.restore_delivery_attempts();
    app.restore_delivery_attempts();
    let attempts = app.delivery_attempts();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].state, "unconfirmed");
    assert_eq!(attempts[0].reason.as_deref(), Some("server_restarted"));
    terminal(&mut app).set_detected_state_with_screen_signals_at(
        None,
        AgentState::Idle,
        false,
        true,
        false,
        false,
        Instant::now(),
    );
    app.tick_idle_wakes(Instant::now());
    claude_idle_for(&mut app, settled());
    runtime(&app).test_process_pty_bytes(&claude_screen(""));
    app.tick_idle_wakes(Instant::now() + GAP);
    assert!(drain(&mut pty).is_empty());
    assert_eq!(app.mailboxes.queued_len(&pane), 1);
    std::fs::remove_file(log).unwrap();
}

#[tokio::test]
async fn delivery_attempt_accepted_wake_does_not_read_mail() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, settled());
    send(&mut app, &pane, "accepted-mail", MsgIntent::NeedsReply);
    let now = Instant::now();
    app.tick_idle_wakes(now);
    drain(&mut pty);
    let text = super::idle_wake_text(1);
    runtime(&app).test_process_pty_bytes(&claude_screen(&text));
    app.tick_idle_wakes(now + GAP);
    assert_eq!(drain(&mut pty), vec![b"\r".to_vec()]);
    app.handle_api_request(Request {
        id: "prompt".into(),
        method: Method::PaneReportPrompt(crate::api::schema::PaneReportPromptParams {
            pane_id: pane.clone(),
            source: "flock:claude".into(),
            agent: "claude".into(),
            prompt: text,
            seq: Some(1),
        }),
    });
    app.tick_idle_wakes(now + GAP + Duration::from_millis(60));
    assert_eq!(app.delivery_attempts()[0].state, "accepted");
    assert_eq!(app.mailboxes.queued_len(&pane), 1);
    assert!(drain(&mut pty).is_empty());
}

#[tokio::test]
async fn delivery_attempt_read_during_verification_is_success() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, settled());
    send(&mut app, &pane, "read-during-verify", MsgIntent::NeedsReply);
    tick_past_gap(&mut app);
    assert_eq!(app.delivery_attempts()[0].state, "submit_sent");
    drain(&mut pty);
    read_inbox(&mut app, &pane);
    assert_eq!(app.delivery_attempts()[0].state, "read");
    assert_eq!(
        app.delivery_attempts()[0].reason.as_deref(),
        Some("inbox_read")
    );
    tick_past_gap(&mut app);
    assert!(drain(&mut pty).is_empty());
}

#[tokio::test]
async fn delivery_attempt_disable_resets_tracker_and_digest_reports_abandonment() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    claude_idle_for(&mut app, settled());
    send(&mut app, &pane, "disabled-wake", MsgIntent::NeedsReply);
    assert!(!drain(&mut pty).is_empty());
    app.state.config.msg.idle_wake = false;
    app.tick_idle_wakes(Instant::now());
    assert!(app.idle_wake.panes.is_empty());
    let attempts = app.delivery_attempts();
    assert_eq!(attempts[0].state, "abandoned");
    assert_eq!(attempts[0].reason.as_deref(), Some("idle_wake_disabled"));
    let events = app.event_hub.events_after(0);
    let digest = crate::digest::categorize(events.iter().map(|(seq, event)| (*seq, 0, event)));
    let row = digest
        .yellow
        .iter()
        .find(|row| row.kind == "delivery_attempt_updated")
        .unwrap();
    assert_eq!(row.summary, "abandoned: idle_wake_disabled");
    assert!(row
        .context
        .contains(&("attempt_id".into(), attempts[0].attempt_id.clone())));
    assert!(row.context.contains(&("pane".into(), pane.clone())));
    assert!(row
        .context
        .contains(&("correlation_ids".into(), "disabled-wake".into())));
    app.state.config.msg.idle_wake = true;
    runtime(&app).test_process_pty_bytes(&claude_screen(""));
    app.tick_idle_wakes(Instant::now());
    assert!(
        !drain(&mut pty).is_empty(),
        "explicit re-enable restores dev's fresh tracker behavior"
    );
}

#[tokio::test]
async fn delivery_attempt_handoff_import_restores_interrupted_evidence() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = rig();
    let log = std::env::temp_dir().join(format!(
        "flock-attempt-handoff-{}-{}.jsonl",
        std::process::id(),
        crate::app::api::messages::now_ms()
    ));
    app.event_hub = crate::api::EventHub::with_persistence(log.clone());
    claude_idle_for(&mut app, settled());
    send(&mut app, &pane, "handoff-attempt", MsgIntent::NeedsReply);
    assert!(!drain(&mut pty).is_empty());
    let id = app.delivery_attempts()[0].attempt_id.clone();
    let snapshot = crate::persist::capture(
        &[],
        &Default::default(),
        &Default::default(),
        None,
        0,
        Default::default(),
        24,
        0.5,
        Default::default(),
        Default::default(),
    );
    let (_, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let restored = App::new_from_handoff(
        &crate::config::Config::default(),
        None,
        api_rx,
        crate::api::EventHub::with_persistence(log.clone()),
        &snapshot,
        &mut Default::default(),
    )
    .unwrap();
    let attempts = restored.delivery_attempts();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].attempt_id, id);
    assert_eq!(attempts[0].state, "unconfirmed");
    assert_eq!(attempts[0].reason.as_deref(), Some("server_restarted"));
    assert!(restored.idle_wake.panes[&pane]
        .announced
        .contains("handoff-attempt"));
    assert!(!restored.idle_wake.in_flight(&pane));
    assert!(drain(&mut pty).is_empty());
    std::fs::remove_file(log).unwrap();
}
