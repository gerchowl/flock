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
                idle.replace("Ask Codex to do anything", &typed.replace('\n', "\n  "))
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
            let typed = typed.replace('\n', "\n┃  ");
            format!("┃\n┃  {typed}\n┃\n┃  Build test-model\n╹\ntab agents ctrl+p commands\n")
        },
    };
}

/// A pane wide enough for the widest fixture, showing `harness` idle.
fn lock_rig(harness: &Harness) -> Rig {
    lock_rig_with(
        harness,
        crate::terminal::TerminalRuntime::test_with_channel(160, 40),
    )
}

fn lock_rig_with(
    harness: &Harness,
    (runtime, pty): (crate::terminal::TerminalRuntime, mpsc::Receiver<Bytes>),
) -> Rig {
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
        // A raw send: a guarded one refuses a held pane (#1162).
        method: Method::PaneSendText(PaneSendTextParams {
            raw: true,
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
    let now = observe_paste_write(&app, &pane, &mut attempt, now);
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
    let Rig {
        mut app,
        pane,
        mut pty,
    } = lock_rig(&CLAUDE);
    let _attempt = inject(&app, &pane, Instant::now());
    drain(&mut pty);
    runtime(&app)
        .try_send_bytes(Bytes::from_static(b"z"))
        .unwrap();
    assert!(drain(&mut pty).is_empty());
    app.settle_injections_for_exit();
    assert_eq!(drain(&mut pty), vec![b"z".to_vec()]);
    assert!(!held(&app));
}

/// A handoff while a draft is mid-clear: the attempt dies with the old server,
/// so the draft must ride the manifest into the new server's stash.
#[tokio::test]
async fn a_handoff_mid_clear_carries_the_draft_to_the_new_server() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = lock_rig(&CLAUDE);
    paint(&app, &CLAUDE, DRAFT);
    let now = Instant::now();
    let mut attempt = inject(&app, &pane, now);
    drain(&mut pty);
    paint(&app, &CLAUDE, "");
    runtime(&app)
        .try_send_bytes(Bytes::from_static(b"z"))
        .unwrap();

    app.settle_injections_for_exit();
    assert_eq!(drain(&mut pty), vec![b"z".to_vec()]);
    assert_eq!(app.kept_drafts.borrow().drafts(&pane), [DRAFT.to_string()]);
    let notice = app
        .state
        .notifications
        .newest_first()
        .next()
        .expect("filed");
    assert_eq!(notice.pane_id.as_deref(), Some(pane.as_str()));

    // The manifest's copy lands in the importing server's stash. A real
    // import restores the same public ids from the snapshot; this fixture's
    // second app numbers its workspaces afresh, so the key is carried over.
    let carried = app.kept_drafts.borrow().export();
    assert_eq!(carried.keys().collect::<Vec<_>>(), [&pane]);
    let Rig {
        app: mut next,
        pane: next_pane,
        ..
    } = lock_rig(&CLAUDE);
    next.kept_drafts.borrow_mut().import(
        carried
            .into_values()
            .map(|drafts| (next_pane.clone(), drafts))
            .collect(),
    );
    let shown: serde_json::Value = serde_json::from_str(&next.handle_api_request(Request {
        id: "stash".into(),
        method: Method::PaneStashShow(crate::api::schema::PaneTarget {
            pane_id: next_pane.clone(),
        }),
    }))
    .unwrap();
    assert_eq!(shown["result"]["drafts"], serde_json::json!([DRAFT]));

    // A rolled-back handoff lets the old attempt finish: it does not keep the
    // same draft twice.
    let outcome = app.advance_guarded_submit(&pane, &mut attempt, now + STEP);
    assert!(outcome.is_some());
    assert_eq!(app.kept_drafts.borrow().drafts(&pane).len(), 1);
}

#[tokio::test]
async fn esc_or_ctrl_c_interrupts_the_injection_at_once() {
    for key in [&b"\x1b"[..], b"\x03"] {
        // No draft: the key goes through with what was held, and the
        // injection stops short of its Enter.
        let Rig { app, pane, mut pty } = lock_rig(&CLAUDE);
        let now = Instant::now();
        let mut attempt = app
            .begin_guarded_submit(&pane, "hello", None, Duration::ZERO, now, false)
            .unwrap();
        drain(&mut pty);
        runtime(&app)
            .try_send_bytes(Bytes::from_static(b"a"))
            .unwrap();
        runtime(&app)
            .try_send_bytes(Bytes::copy_from_slice(key))
            .unwrap();
        assert_eq!(drain(&mut pty), vec![[b"a", key].concat()], "{key:?}");
        assert!(!held(&app));
        paint(&app, &CLAUDE, "hello");
        assert_eq!(
            app.advance_guarded_submit(&pane, &mut attempt, now + GAP),
            Some(Outcome::Unconfirmed("paste_written"))
        );
        assert!(drain(&mut pty).is_empty(), "{key:?}: no Enter");

        // A draft moved aside and not yet back is kept, not lost.
        let Rig { app, pane, mut pty } = lock_rig(&CLAUDE);
        paint(&app, &CLAUDE, DRAFT);
        let now = Instant::now();
        let mut attempt = inject(&app, &pane, now);
        paint(&app, &CLAUDE, "");
        assert_eq!(
            app.advance_guarded_submit(&pane, &mut attempt, now + POLL),
            None
        );
        drain(&mut pty);
        runtime(&app)
            .try_send_bytes(Bytes::copy_from_slice(key))
            .unwrap();
        assert_eq!(drain(&mut pty), vec![key.to_vec()]);
        assert_eq!(
            app.advance_guarded_submit(&pane, &mut attempt, now + POLL + GAP),
            Some(Outcome::Unconfirmed("paste_written"))
        );
        assert_eq!(app.kept_drafts.borrow().drafts(&pane), [DRAFT.to_string()]);
    }
}

/// ADR-0029 §1: interruption after a completed paste leaves its effect unknown.
/// The caller must inspect the composer before retrying.
/// The test owns the advancement gate: no app-loop tick runs until Esc has
/// synchronously traversed the real client-input path and reached the PTY.
#[tokio::test]
async fn guarded_submit_operator_before_enter_preserves_interruption_evidence() {
    for lock in [false, true] {
        let Rig {
            mut app,
            pane,
            mut pty,
        } = lock_rig(&CODEX);
        app.state.config.ui.injection_lock.enabled = lock;
        app.state.active = Some(1);
        app.state.selected = 1;
        app.state.mode = crate::app::Mode::Terminal;
        let now = Instant::now();
        let mut attempt = app
            .begin_guarded_submit(&pane, "failure prompt", None, Duration::ZERO, now, false)
            .expect("submit begins");
        let mut writes = drain(&mut pty);
        assert!(
            !writes.is_empty(),
            "the prompt was typed before interruption"
        );
        paint(&app, &CODEX, "failure prompt");

        app.route_client_events(
            vec![crate::raw_input::RawInputEvent::Key(
                crate::input::TerminalKey::new(
                    crossterm::event::KeyCode::Esc,
                    crossterm::event::KeyModifiers::empty(),
                ),
            )],
            false,
        );
        let interrupted = drain(&mut pty);
        assert_eq!(
            interrupted,
            vec![b"\x1b".to_vec()],
            "Esc passes through with lock={lock}"
        );
        writes.extend(interrupted);
        // Only the test can release this gate by calling advancement, even
        // when the host delays us beyond the production first-Enter timer.
        assert_eq!(
            app.advance_guarded_submit(&pane, &mut attempt, now + GAP),
            Some(Outcome::Unconfirmed("paste_written"))
        );
        writes.extend(drain(&mut pty));
        assert!(
            !writes.iter().any(|write| write.contains(&b'\r')),
            "{writes:?}"
        );
        assert!(!attempt.evidence.attempt_id.is_empty());
        assert_eq!(attempt.evidence.state, "unconfirmed");
        assert_eq!(attempt.evidence.reason.as_deref(), Some("paste_written"));
        assert!(attempt.evidence.submit_sent_at_ms.is_none());
        assert!(attempt.evidence.finished_at_ms.is_some());
        assert!(!attempt.evidence.retried);
        assert_eq!(app.delivery_attempts().last(), Some(&attempt.evidence));
    }
}

/// #1011 review: when the pane refuses the replay of held operator keys, the
/// attempt's outcome says they were not written.
#[tokio::test]
async fn a_refused_replay_of_held_keys_is_in_the_attempt_outcome() {
    let Rig { app, pane, mut pty } = lock_rig(&CLAUDE);
    let now = Instant::now();
    let mut attempt = app
        .begin_guarded_submit(&pane, "hello", None, Duration::ZERO, now, false)
        .unwrap();
    drain(&mut pty);
    runtime(&app)
        .try_send_bytes(Bytes::from_static(b"a"))
        .unwrap();
    drop(pty);
    paint(&app, &CLAUDE, "something else");

    assert_eq!(
        app.advance_guarded_submit(
            &pane,
            &mut attempt,
            now + crate::app::guarded_submit::CONFIRM_WINDOW
        ),
        Some(Outcome::Unconfirmed("owned_composer_not_visible"))
    );

    assert!(attempt.evidence.held_input_not_written);
    assert!(!held(&app));
}

#[tokio::test]
async fn held_keys_wait_out_a_full_channel_in_order() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = lock_rig_with(
        &CLAUDE,
        crate::terminal::TerminalRuntime::test_with_channel_capacity(160, 40, 1),
    );
    let now = Instant::now();
    let _attempt = inject(&app, &pane, now);
    // The sentence fills the one slot; the PTY reader has not come round.
    runtime(&app)
        .try_send_bytes(Bytes::from_static(b"a"))
        .unwrap();
    assert!(app.tick_injection_leases(now + INJECTION_LEASE));
    assert!(runtime(&app).has_held_backlog(), "kept, not dropped");
    assert!(
        app.next_injection_lease_deadline().is_some(),
        "the loop retries"
    );
    assert_eq!(drain(&mut pty), vec![b"hello".to_vec()]);
    // A later key queues behind the backlog rather than overtaking it.
    runtime(&app)
        .try_send_bytes(Bytes::from_static(b"b"))
        .unwrap();
    assert_eq!(drain(&mut pty), vec![b"ab".to_vec()]);
    assert!(!runtime(&app).has_held_backlog());
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
async fn an_inbox_wake_moves_a_draft_aside_and_back() {
    for read_first in [false, true] {
        let Rig {
            mut app,
            pane,
            mut pty,
        } = lock_rig(&CLAUDE);
        paint(&app, &CLAUDE, DRAFT);
        let now = Instant::now();
        send(&mut app, &pane, "c-1", MsgIntent::NeedsReply);
        assert_eq!(drain(&mut pty), vec![clear_keys_for(&app, DRAFT)]);
        assert!(held(&app));
        let sentence = super::super::idle_wake_text(1);

        paint(&app, &CLAUDE, "");
        let at = now + GAP + POLL;
        app.tick_idle_wakes(at);
        assert_eq!(drain(&mut pty), vec![sentence.clone().into_bytes()]);
        paint(&app, &CLAUDE, &sentence);
        let at = at + GAP + POLL;
        app.tick_idle_wakes(at);
        assert_eq!(drain(&mut pty), vec![b"\r".to_vec()]);
        took_turn(&mut app, Agent::Claude, at);
        paint(&app, &CLAUDE, "");
        let at = at + POLL;
        app.tick_idle_wakes(at);
        assert_eq!(drain(&mut pty), vec![DRAFT.as_bytes().to_vec()]);

        if read_first {
            // The agent reads its inbox before the draft reads back: the wake
            // ends there, so the draft goes to the stash and is announced.
            read_inbox(&mut app, &pane);
            assert_eq!(app.kept_drafts.borrow().drafts(&pane), [DRAFT.to_string()]);
            let notice = app
                .state
                .notifications
                .newest_first()
                .next()
                .expect("filed");
            assert_eq!(notice.pane_id.as_deref(), Some(pane.as_str()));
        } else {
            paint(&app, &CLAUDE, DRAFT);
            app.tick_idle_wakes(at + POLL);
            assert!(app.kept_drafts.borrow().drafts(&pane).is_empty());
            assert!(app
                .delivery_attempts()
                .iter()
                .any(|attempt| attempt.draft.as_deref() == Some("restored")));
        }
        assert!(!held(&app), "read_first={read_first}");
    }
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

#[tokio::test]
async fn a_paste_held_by_an_injection_claims_no_evidence() {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = lock_rig(&CLAUDE);
    let _attempt = inject(&app, &pane, Instant::now());
    drain(&mut pty);
    let response: serde_json::Value = serde_json::from_str(&app.handle_api_request(Request {
        id: "paste".into(),
        // A raw send: a guarded one refuses a held pane (#1162).
        method: Method::AgentSend(crate::api::schema::AgentSendParams {
            raw: true,
            target: pane.clone(),
            text: "note".into(),
            submit: false,
        }),
    }))
    .unwrap();
    assert_eq!(response["result"]["outcome"], "unconfirmed", "{response}");
    assert_eq!(response["result"]["reason"], "held_by_injection");
    assert!(response["result"]["evidence"]["level"].is_null());
    assert!(app.pending_paste.is_none(), "nothing left to observe");
    assert!(drain(&mut pty).is_empty(), "held behind the injection");
}

/// An operator's draft that wraps onto a second editor row.
const MULTI_ROW: &str = "first line of a thought\nand its second line";

/// The verbs that type without submitting through the guard (#1162), each
/// aimed at `pane` the way its CLI sends it.
fn typing_verbs(pane: &str, raw: bool) -> Vec<(&'static str, Method)> {
    vec![
        (
            "pane send-text",
            Method::PaneSendText(PaneSendTextParams {
                pane_id: pane.into(),
                text: "note".into(),
                raw,
            }),
        ),
        (
            "pane run",
            Method::PaneSendInput(crate::api::schema::PaneSendInputParams {
                write_timeout_ms: None,
                pane_id: pane.into(),
                text: "note".into(),
                keys: Vec::new(),
                raw,
            }),
        ),
        (
            "agent send --no-submit",
            Method::AgentSend(crate::api::schema::AgentSendParams {
                target: pane.into(),
                text: "note".into(),
                submit: false,
                raw,
            }),
        ),
    ]
}

fn call(app: &mut App, method: Method) -> serde_json::Value {
    serde_json::from_str(&app.handle_api_request(Request {
        id: "req".into(),
        method,
    }))
    .unwrap()
}

#[tokio::test]
async fn typing_verbs_refuse_an_operator_draft_before_typing_in_every_harness() {
    for harness in [&CLAUDE, &CODEX, &OPENCODE] {
        for draft in [DRAFT, MULTI_ROW] {
            for index in 0..typing_verbs("", false).len() {
                let Rig {
                    mut app,
                    pane,
                    mut pty,
                } = lock_rig(harness);
                let (verb, method) = typing_verbs(&pane, false).swap_remove(index);
                paint(&app, harness, draft);
                let response = call(&mut app, method);
                let case = format!("{:?} {verb} onto {draft:?}", harness.agent);
                assert_eq!(response["error"]["code"], "input_not_empty", "{case}");
                assert!(
                    response["error"]["data"]["composer"]["visible_bytes"]
                        .as_u64()
                        .is_some_and(|n| n > 0),
                    "{response}"
                );
                assert_eq!(
                    response["error"]["data"]["remedy"]["retry_when"],
                    "composer_empty"
                );
                assert_eq!(response["error"]["data"]["remedy"]["follow_up_issue"], 1586);
                assert!(response["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("agent UI"));
                assert!(
                    !response.to_string().contains(draft),
                    "draft contents must not leak"
                );
                assert!(drain(&mut pty).is_empty(), "{case}: nothing typed");
            }
        }
    }
}

#[tokio::test]
async fn draft_refusal_reports_visible_utf8_bytes_without_returning_content() {
    let draft = "opaque-token-é";
    for harness in [&CLAUDE, &CODEX, &OPENCODE] {
        let Rig {
            mut app,
            pane,
            mut pty,
        } = lock_rig(harness);
        paint(&app, harness, draft);
        let (_, method) = typing_verbs(&pane, false).swap_remove(2);
        let response = call(&mut app, method);
        assert_eq!(
            response["error"]["data"]["composer"]["visible_bytes"],
            draft.len()
        );
        assert!(!response.to_string().contains(draft));
        let legacy: crate::api::schema::ErrorResponse = serde_json::from_value(response).unwrap();
        assert_eq!(legacy.error.code, "input_not_empty");
        assert!(drain(&mut pty).is_empty());
    }
}

#[tokio::test]
async fn typing_verbs_type_into_an_empty_composer_and_raw_types_onto_a_draft() {
    for harness in [&CLAUDE, &CODEX, &OPENCODE] {
        for (draft, raw) in [("", false), (DRAFT, true), (MULTI_ROW, true)] {
            for index in 0..typing_verbs("", raw).len() {
                let Rig {
                    mut app,
                    pane,
                    mut pty,
                } = lock_rig(harness);
                let (verb, method) = typing_verbs(&pane, raw).swap_remove(index);
                paint(&app, harness, draft);
                let response = call(&mut app, method);
                let case = format!("{:?} {verb} raw={raw} onto {draft:?}", harness.agent);
                assert!(response.get("error").is_none(), "{case}: {response}");
                assert_eq!(drain(&mut pty).len(), 1, "{case}: typed");
            }
        }
    }
}

#[tokio::test]
async fn a_shell_pane_is_typed_into_whatever_its_prompt_shows() {
    for index in 0..typing_verbs("", false).len() {
        let Rig {
            mut app,
            pane,
            mut pty,
        } = lock_rig(&CLAUDE);
        let (verb, method) = typing_verbs(&pane, false).swap_remove(index);
        terminal(&mut app).detected_agent = None;
        paint(&app, &CLAUDE, DRAFT);
        let response = call(&mut app, method);
        assert!(response.get("error").is_none(), "{verb}: {response}");
        assert_eq!(drain(&mut pty).len(), 1, "{verb}: typed");
    }
}

#[tokio::test]
async fn typing_verbs_refuse_a_pane_an_injection_holds() {
    for index in 0..typing_verbs("", false).len() {
        let Rig {
            mut app,
            pane,
            mut pty,
        } = lock_rig(&CLAUDE);
        let (verb, method) = typing_verbs(&pane, false).swap_remove(index);
        paint(&app, &CLAUDE, DRAFT);
        let _attempt = inject(&app, &pane, Instant::now());
        drain(&mut pty);
        let response = call(&mut app, method);
        assert_eq!(
            response["error"]["code"], "injection_pending",
            "{verb}: held text would replay onto the restored draft"
        );
        runtime(&app).end_injection_lease(&app.delivery_attempts()[0].attempt_id);
        assert!(drain(&mut pty).is_empty(), "{verb}: nothing was held");
    }
}

/// `agent send` into an agent pane (#1162): a one-row draft moves aside and
/// comes back, a multi-row draft refuses before typing. `pane run` keeps its
/// typing and refuses any draft instead (`typing_verbs_…`).
#[tokio::test]
async fn a_submit_into_an_agent_pane_stashes_one_row_and_refuses_several() {
    for harness in [&CLAUDE, &CODEX, &OPENCODE] {
        let agent = harness.agent;
        let send_submit = |pane: &str| {
            Method::AgentSend(crate::api::schema::AgentSendParams {
                target: pane.into(),
                text: "hello".into(),
                submit: true,
                raw: false,
            })
        };

        let Rig {
            mut app,
            pane,
            mut pty,
        } = lock_rig(harness);
        paint(&app, harness, MULTI_ROW);
        let response = call(&mut app, send_submit(&pane));
        assert_eq!(response["error"]["code"], "input_not_empty", "{agent:?}");
        assert!(
            response["error"]["data"]["composer"]["visible_bytes"]
                .as_u64()
                .is_some_and(|n| n > 0),
            "{response}"
        );
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no safe CLI command"));
        assert!(drain(&mut pty).is_empty(), "{agent:?}: nothing typed");

        let Rig {
            mut app,
            pane,
            mut pty,
        } = lock_rig(harness);
        paint(&app, harness, DRAFT);
        let response = call(&mut app, send_submit(&pane));
        assert!(response.get("error").is_none(), "{agent:?}: {response}");
        let (_, _, mut attempt) = app.pending_agent_submit.take().expect("parked");
        assert_eq!(
            drain(&mut pty),
            vec![clear_keys_for(&app, DRAFT)],
            "{agent:?}: the draft is cleared, nothing typed beside it"
        );
        paint(&app, harness, "");
        let now = Instant::now() + POLL;
        assert_eq!(app.advance_guarded_submit(&pane, &mut attempt, now), None);
        assert_eq!(drain(&mut pty), vec![b"hello".to_vec()], "{agent:?}");
        let at = now + GAP;
        submit(&mut app, harness, &pane, &mut attempt, at);
        assert_eq!(drain(&mut pty), vec![b"\r".to_vec()], "{agent:?}");
        paint(&app, harness, "");
        assert_eq!(app.advance_guarded_submit(&pane, &mut attempt, at), None);
        assert_eq!(
            drain(&mut pty),
            vec![DRAFT.as_bytes().to_vec()],
            "{agent:?}"
        );
        paint(&app, harness, DRAFT);
        assert_eq!(
            app.advance_guarded_submit(&pane, &mut attempt, at + POLL),
            Some(Outcome::ObservedAccepted),
            "{agent:?}"
        );
        assert_eq!(
            app.delivery_attempts()[0].draft.as_deref(),
            Some("restored")
        );
    }
}
