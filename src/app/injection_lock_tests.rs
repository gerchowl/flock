//! The injection lock end to end (#850): every assertion is on the bytes that
//! reach the pane's PTY channel, against fixture screens a fake harness paints
//! in answer to them. Time is passed in, never slept through.

use self::harness::{Harness, CLAUDE, CODEX, OPENCODE};
use super::*;
use crate::app::guarded_submit::{Attempt, Outcome, INJECTION_LEASE};

const POLL: Duration = Duration::from_millis(50);
const STEP: Duration = Duration::from_secs(1);
const DRAFT: &str = "half a thought, not sent";

mod harness {
    use crate::detect::Agent;

    /// A harness's idle screen with `typed` in its composer.
    pub(super) struct Harness {
        pub(super) agent: Agent,
        pub(super) screen: fn(&str) -> String,
    }

    pub(super) const CLAUDE: Harness = Harness {
        agent: Agent::Claude,
        screen: |typed| {
            let rule = "─".repeat(40);
            format!("Task complete.\n{rule}\n❯ {typed}\n{rule}\n")
        },
    };

    pub(super) const CODEX: Harness = Harness {
        agent: Agent::Codex,
        screen: |typed| {
            let idle = include_str!("../../tests/fixtures/codex-submit-721/inline-idle.txt");
            if typed.is_empty() {
                idle.to_string()
            } else {
                idle.replace("Ask Codex to do anything", typed)
            }
        },
    };

    pub(super) const OPENCODE: Harness = Harness {
        agent: Agent::OpenCode,
        screen: |typed| {
            let typed = if typed.is_empty() {
                "Ask anything…"
            } else {
                typed
            };
            format!("┃\n┃  {typed}\n┃\n┃  Build test-model\n╹\ntab agents ctrl+p commands\n")
        },
    };
}

/// A pane wide enough for the widest fixture, showing `harness` idle.
fn lock_rig(harness: &Harness) -> Rig {
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
    let (runtime, pty) = crate::terminal::TerminalRuntime::test_with_channel(160, 40);
    recipient.tabs[0].runtimes.insert(focused, runtime);
    app.state.workspaces = vec![sender, recipient];
    app.state.ensure_test_terminals();
    app.state.active = Some(0);
    let pane = app.public_pane_id(1, focused).expect("public id");
    paint(&app, harness, "");
    idle(&mut app, harness.agent, Instant::now());
    Rig { app, pane, pty }
}

fn paint(app: &App, harness: &Harness, typed: &str) {
    let screen = (harness.screen)(typed).replace('\n', "\r\n");
    runtime(app).test_process_pty_bytes(format!("\x1b[2J\x1b[H{screen}").as_bytes());
}

fn idle(app: &mut App, agent: Agent, now: Instant) {
    let terminal = terminal(app);
    for at in [now - settled(), now] {
        terminal.set_detected_state_with_screen_signals_at(
            Some(agent),
            AgentState::Idle,
            false,
            true,
            false,
            false,
            at,
        );
    }
}

/// The agent took a turn: the cursor moves, which confirms an Enter.
fn took_turn(app: &mut App, agent: Agent, now: Instant) {
    let terminal = terminal(app);
    terminal.set_detected_state_with_screen_signals_at(
        Some(agent),
        AgentState::Working,
        false,
        false,
        true,
        false,
        now,
    );
    terminal.set_detected_state_with_screen_signals_at(
        Some(agent),
        AgentState::Idle,
        false,
        true,
        false,
        false,
        now,
    );
}

fn inject(app: &App, pane: &str, now: Instant) -> Attempt {
    app.begin_guarded_injection(pane, "hello", None, Duration::ZERO, now, false, true)
        .expect("injection begins")
}

fn held(app: &App) -> bool {
    runtime(app).injection_lease_until().is_some()
}

fn send_text(app: &mut App, pane: &str, text: &str) {
    let response = app.handle_api_request(Request {
        id: "req".into(),
        method: Method::PaneSendText(PaneSendTextParams {
            pane_id: pane.into(),
            text: text.into(),
        }),
    });
    assert!(response.contains("\"result\""), "{response}");
}

/// Drive an attempt whose sentence is typed through its Enter and the turn.
fn submit(app: &mut App, harness: &Harness, pane: &str, attempt: &mut Attempt, at: Instant) {
    paint(app, harness, "hello");
    assert_eq!(app.advance_guarded_submit(pane, attempt, at), None);
    took_turn(app, harness.agent, at);
}

#[tokio::test]
async fn keystrokes_from_both_seams_are_held_and_replay_in_order_after_the_submit() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = lock_rig(&CLAUDE);
    let now = Instant::now();
    let mut attempt = app
        .begin_guarded_submit(&pane, "hello", None, Duration::ZERO, now, false)
        .unwrap();
    assert_eq!(drain(&mut pty), vec![b"hello".to_vec()]);
    assert_eq!(
        runtime(&app).injection_lease_until(),
        Some(now + INJECTION_LEASE)
    );

    // An attached client's keystroke and a `pane.send-text` from another tool,
    // between the sentence and its Enter.
    runtime(&app)
        .try_send_bytes(Bytes::from_static(b"a"))
        .unwrap();
    send_text(&mut app, &pane, "b");
    runtime(&app)
        .try_send_bytes(Bytes::from_static(b"c"))
        .unwrap();
    assert!(drain(&mut pty).is_empty(), "nothing interleaves");
    assert_eq!(
        runtime(&app).last_operator_input_at(),
        None,
        "held keys do not abandon the injection as operator input"
    );

    paint(&app, &CLAUDE, "hello");
    assert_eq!(
        app.advance_guarded_submit(&pane, &mut attempt, now + GAP),
        None
    );
    assert_eq!(drain(&mut pty), vec![b"\r".to_vec()]);
    took_turn(&mut app, Agent::Claude, now + GAP);
    assert_eq!(
        app.advance_guarded_submit(&pane, &mut attempt, now + GAP),
        Some(Outcome::ObservedAccepted)
    );
    assert_eq!(drain(&mut pty), vec![b"abc".to_vec()], "replayed in order");
    assert!(!held(&app));
    assert!(runtime(&app).last_operator_input_at().is_some());
}

#[tokio::test]
async fn a_pane_without_an_injection_is_never_held() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = lock_rig(&CLAUDE);
    runtime(&app)
        .try_send_bytes(Bytes::from_static(b"a"))
        .unwrap();
    send_text(&mut app, &pane, "b");
    assert_eq!(drain(&mut pty), vec![b"a".to_vec(), b"b".to_vec()]);
    assert!(!held(&app));
    assert_eq!(app.next_injection_lease_deadline(), None);

    // Nor is one whose operator switched the lock off.
    app.state.config.ui.injection_lock.enabled = false;
    // Past the operator's own quiet gap, so the injection may begin.
    let _attempt = app
        .begin_guarded_submit(
            &pane,
            "hello",
            None,
            Duration::ZERO,
            Instant::now() + STEP,
            false,
        )
        .unwrap();
    drain(&mut pty);
    assert!(!held(&app));
    runtime(&app)
        .try_send_bytes(Bytes::from_static(b"c"))
        .unwrap();
    assert_eq!(drain(&mut pty), vec![b"c".to_vec()]);
}

#[tokio::test]
async fn a_lease_whose_owner_vanished_ends_on_its_bound() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = lock_rig(&CLAUDE);
    let now = Instant::now();
    // The requester disconnects: nothing ever advances this attempt again.
    drop(inject(&app, &pane, now));
    drain(&mut pty);
    runtime(&app)
        .try_send_bytes(Bytes::from_static(b"a"))
        .unwrap();
    assert_eq!(
        app.next_injection_lease_deadline(),
        Some(now + INJECTION_LEASE),
        "the loop wakes for the bound"
    );
    assert!(!app.tick_injection_leases(now + INJECTION_LEASE - POLL));
    assert!(drain(&mut pty).is_empty());
    assert!(app.tick_injection_leases(now + INJECTION_LEASE));
    assert_eq!(drain(&mut pty), vec![b"a".to_vec()]);
    assert!(!held(&app));
}

#[tokio::test]
async fn an_injection_that_fails_releases_the_pane() {
    let Rig { app, pane, mut pty } = lock_rig(&CLAUDE);
    let now = Instant::now();
    let mut attempt = app
        .begin_guarded_submit(&pane, "hello", None, Duration::ZERO, now, false)
        .unwrap();
    drain(&mut pty);
    runtime(&app)
        .try_send_bytes(Bytes::from_static(b"a"))
        .unwrap();
    // The paste never shows, so the Enter is never authorized.
    paint(&app, &CLAUDE, "something else");
    assert_eq!(
        app.advance_guarded_submit(
            &pane,
            &mut attempt,
            now + crate::app::guarded_submit::CONFIRM_WINDOW
        ),
        Some(Outcome::Unconfirmed("owned_composer_not_visible"))
    );
    assert_eq!(drain(&mut pty), vec![b"a".to_vec()]);
    assert!(!held(&app));
}

#[tokio::test]
async fn a_stopping_or_handing_off_server_writes_what_a_lease_holds() {
    let Rig { app, pane, mut pty } = lock_rig(&CLAUDE);
    let _attempt = inject(&app, &pane, Instant::now());
    drain(&mut pty);
    runtime(&app)
        .try_send_bytes(Bytes::from_static(b"z"))
        .unwrap();
    assert!(drain(&mut pty).is_empty());
    app.end_all_injection_leases();
    assert_eq!(drain(&mut pty), vec![b"z".to_vec()]);
    assert!(!held(&app));
}

fn clear_keys_for(app: &App, draft: &str) -> Vec<u8> {
    let key = |code| {
        runtime(app).encode_terminal_key(
            crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::empty()).into(),
        )
    };
    let mut keys = key(crossterm::event::KeyCode::End);
    for _ in 0..draft.chars().count() + 8 {
        keys.extend(key(crossterm::event::KeyCode::Backspace));
    }
    keys
}

#[tokio::test]
async fn a_draft_moves_aside_and_comes_back_exactly_in_every_harness() {
    for harness in [&CLAUDE, &CODEX, &OPENCODE] {
        let Rig {
            mut app,
            pane,
            mut pty,
        } = lock_rig(harness);
        let agent = harness.agent;
        paint(&app, harness, DRAFT);
        let now = Instant::now();
        let mut attempt = inject(&app, &pane, now);
        assert_eq!(
            drain(&mut pty),
            vec![clear_keys_for(&app, DRAFT)],
            "{agent:?}: the draft is cleared, nothing typed beside it"
        );
        assert_eq!(app.delivery_attempts()[0].draft.as_deref(), Some("stashed"));
        runtime(&app)
            .try_send_bytes(Bytes::from_static(b"z"))
            .unwrap();

        // Not cleared yet: wait, typing nothing.
        assert_eq!(
            app.advance_guarded_submit(&pane, &mut attempt, now + POLL),
            None
        );
        assert!(drain(&mut pty).is_empty(), "{agent:?}");
        paint(&app, harness, "");
        let at = now + 2 * POLL;
        assert_eq!(app.advance_guarded_submit(&pane, &mut attempt, at), None);
        assert_eq!(drain(&mut pty), vec![b"hello".to_vec()], "{agent:?}");

        let at = at + GAP;
        submit(&mut app, harness, &pane, &mut attempt, at);
        assert_eq!(drain(&mut pty), vec![b"\r".to_vec()], "{agent:?}");

        // The turn landed and the composer emptied: the draft goes back,
        // and the outcome waits until it reads back.
        paint(&app, harness, "");
        assert_eq!(app.advance_guarded_submit(&pane, &mut attempt, at), None);
        assert_eq!(
            drain(&mut pty),
            vec![DRAFT.as_bytes().to_vec()],
            "{agent:?}"
        );
        assert!(held(&app), "{agent:?}: still held while it is restored");
        paint(&app, harness, DRAFT);
        assert_eq!(
            app.advance_guarded_submit(&pane, &mut attempt, at + POLL),
            Some(Outcome::ObservedAccepted),
            "{agent:?}"
        );
        assert_eq!(
            drain(&mut pty),
            vec![b"z".to_vec()],
            "{agent:?}: then the held key"
        );
        assert_eq!(
            app.delivery_attempts()[0].draft.as_deref(),
            Some("restored")
        );
        assert!(app.kept_drafts.borrow().drafts(&pane).is_empty());
    }
}

#[tokio::test]
async fn a_draft_that_cannot_be_read_back_is_kept_and_surfaced() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = lock_rig(&CLAUDE);
    paint(&app, &CLAUDE, DRAFT);
    let now = Instant::now();
    let mut attempt = inject(&app, &pane, now);
    paint(&app, &CLAUDE, "");
    assert_eq!(
        app.advance_guarded_submit(&pane, &mut attempt, now + POLL),
        None
    );
    submit(&mut app, &CLAUDE, &pane, &mut attempt, now + POLL + GAP);
    paint(&app, &CLAUDE, "");
    let at = now + POLL + GAP;
    assert_eq!(app.advance_guarded_submit(&pane, &mut attempt, at), None);
    drain(&mut pty);
    // The harness mangles it: the composer never shows the draft.
    paint(&app, &CLAUDE, "half a th");
    assert_eq!(
        app.advance_guarded_submit(&pane, &mut attempt, at + STEP),
        Some(Outcome::ObservedAccepted),
        "the submit still landed"
    );
    assert_eq!(app.delivery_attempts()[0].draft.as_deref(), Some("kept"));
    assert_eq!(app.kept_drafts.borrow().drafts(&pane), [DRAFT.to_string()]);

    app.surface_kept_drafts();
    let notice = app
        .state
        .notifications
        .newest_first()
        .next()
        .expect("filed");
    assert_eq!(notice.pane_id.as_deref(), Some(pane.as_str()));
    assert!(
        !notice.body.as_deref().unwrap_or_default().contains(DRAFT),
        "the durable log never carries the draft"
    );

    let stash = |app: &mut App, restore: bool| -> serde_json::Value {
        let target = crate::api::schema::PaneTarget {
            pane_id: pane.clone(),
        };
        serde_json::from_str(&app.handle_api_request(Request {
            id: "stash".into(),
            method: if restore {
                Method::PaneStashRestore(target)
            } else {
                Method::PaneStashShow(target)
            },
        }))
        .unwrap()
    };
    assert_eq!(stash(&mut app, false)["result"]["drafts"][0], DRAFT);
    // Restore types only into an empty composer, as the operator would.
    assert_eq!(stash(&mut app, true)["error"]["code"], "input_not_empty");
    assert!(drain(&mut pty).is_empty());
    paint(&app, &CLAUDE, "");
    assert_eq!(stash(&mut app, true)["result"]["drafts"][0], DRAFT);
    assert_eq!(drain(&mut pty), vec![DRAFT.as_bytes().to_vec()]);
    assert_eq!(
        stash(&mut app, false)["result"]["drafts"],
        serde_json::json!([])
    );
    assert_eq!(stash(&mut app, true)["error"]["code"], "stash_empty");
}

#[tokio::test]
async fn a_draft_the_clear_did_not_touch_stays_where_it_was() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = lock_rig(&CLAUDE);
    paint(&app, &CLAUDE, DRAFT);
    let now = Instant::now();
    let mut attempt = inject(&app, &pane, now);
    drain(&mut pty);
    assert_eq!(
        app.advance_guarded_submit(&pane, &mut attempt, now + STEP),
        Some(Outcome::Abandoned("draft_not_cleared"))
    );
    assert!(drain(&mut pty).is_empty(), "nothing typed beside the draft");
    assert_eq!(
        app.delivery_attempts()[0].draft.as_deref(),
        Some("untouched")
    );
    assert!(app.kept_drafts.borrow().drafts(&pane).is_empty());

    // Cleared but never typed into: the draft is gone from the screen, so it
    // is kept rather than lost.
    let mut attempt = inject(&app, &pane, now);
    drain(&mut pty);
    paint(&app, &CLAUDE, "");
    // Something else started a turn while the composer was being cleared.
    took_turn(&mut app, Agent::Claude, now);
    assert_eq!(
        app.advance_guarded_submit(&pane, &mut attempt, now + POLL),
        Some(Outcome::Abandoned("turn_started_before_enter"))
    );
    assert!(drain(&mut pty).is_empty());
    assert_eq!(app.kept_drafts.borrow().drafts(&pane), [DRAFT.to_string()]);
}

#[tokio::test]
async fn drafts_flock_cannot_put_back_exactly_still_refuse() {
    for draft in [
        "first line\nsecond line",
        "[Pasted text #1 +40 lines]",
        "see [Image #1]",
    ] {
        let Rig { app, pane, mut pty } = lock_rig(&CLAUDE);
        paint(&app, &CLAUDE, draft);
        assert_eq!(
            app.begin_guarded_injection(
                &pane,
                "hello",
                None,
                Duration::ZERO,
                Instant::now(),
                false,
                true
            )
            .unwrap_err(),
            "input_not_empty",
            "{draft:?}"
        );
        assert!(drain(&mut pty).is_empty(), "{draft:?}");
        assert!(!held(&app));
    }
    // `pane.submit` (a delegate's brief) never moves a draft.
    let Rig { app, pane, mut pty } = lock_rig(&CLAUDE);
    paint(&app, &CLAUDE, DRAFT);
    assert_eq!(
        app.begin_guarded_submit(&pane, "hello", None, Duration::ZERO, Instant::now(), false)
            .unwrap_err(),
        "input_not_empty"
    );
    assert!(drain(&mut pty).is_empty());
}

#[tokio::test]
async fn an_inbox_wake_moves_a_draft_aside_too() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = lock_rig(&CLAUDE);
    paint(&app, &CLAUDE, DRAFT);
    send(&mut app, &pane, "c-1", MsgIntent::NeedsReply);
    assert_eq!(drain(&mut pty), vec![clear_keys_for(&app, DRAFT)]);
    assert!(held(&app));
}

#[tokio::test]
async fn an_inbox_read_mid_flight_releases_the_pane() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = lock_rig(&CLAUDE);
    send(&mut app, &pane, "c-1", MsgIntent::NeedsReply);
    assert_eq!(drain(&mut pty).len(), 1, "sentence typed");
    assert!(held(&app));
    runtime(&app)
        .try_send_bytes(Bytes::from_static(b"z"))
        .unwrap();
    // The agent reads its inbox before the wake concludes: the tracker drops
    // the flight, and the pane must not stay held until the bound.
    read_inbox(&mut app, &pane);
    assert!(!held(&app));
    assert_eq!(drain(&mut pty), vec![b"z".to_vec()]);
}
