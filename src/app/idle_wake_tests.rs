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
    tick_past_gap(&mut app);
    claude_idle_for(&mut app, settled());
    tick_past_gap(&mut app);
    assert!(drain(&mut pty).is_empty(), "announced mail is not re-typed");

    // A NEW message after the agent left Idle earns its own wake.
    send(&mut app, &pane, "c-2", MsgIntent::NeedsReply);
    assert_eq!(drain(&mut pty), vec![super::idle_wake_text(2).into_bytes()]);
    tick_past_gap(&mut app);
    drain(&mut pty);

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
