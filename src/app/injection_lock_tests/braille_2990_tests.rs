use super::*;
use crate::app::guarded_submit::{runtime_composer, Composer};

const START: &str = include_str!("../../../tests/fixtures/codex-braille-2990/start.txt");
const SEND: &str = include_str!("../../../tests/fixtures/codex-braille-2990/send.txt");

fn observe(app: &mut App, screen: &str, now: Instant) {
    runtime(app).test_process_pty_bytes(
        format!("\x1b[2J\x1b[H{}", screen.replace('\n', "\r\n")).as_bytes(),
    );
    let screen = runtime(app).detection_text();
    let detection = crate::detect::detect_agent(Some(Agent::Codex), &screen);
    for at in [now - settled(), now] {
        terminal(app).set_detected_state_with_screen_signals_at(
            Some(Agent::Codex),
            detection.state,
            detection.visible_blocker,
            detection.visible_idle,
            detection.visible_working,
            false,
            at,
        );
    }
}

fn submits(screen: &str) {
    let Rig {
        mut app,
        pane,
        mut pty,
    } = lock_rig(&CODEX);
    let now = Instant::now();
    observe(&mut app, screen, now);
    let detection =
        crate::detect::detect_agent(Some(Agent::Codex), &runtime(&app).detection_text());
    assert_eq!(detection.state, AgentState::Idle);
    assert!(detection.visible_idle);
    assert!(!detection.visible_working);
    assert_eq!(
        runtime_composer(Agent::Codex, runtime(&app), "hello"),
        Composer::Empty
    );
    let mut attempt = app
        .begin_guarded_submit(&pane, "hello", None, Duration::ZERO, now, false)
        .expect("decorated idle screen authorizes the shared submit path");
    assert_eq!(drain(&mut pty), vec![b"hello".to_vec()]);
    paint(&app, &CODEX, "hello");
    let at = observe_paste_write(&app, &pane, &mut attempt, now) + GAP;
    assert_eq!(app.advance_guarded_submit(&pane, &mut attempt, at), None);
    assert_eq!(drain(&mut pty), vec![b"\r".to_vec()]);
    took_turn(&mut app, Agent::Codex, at);
    assert_eq!(
        app.advance_guarded_submit(&pane, &mut attempt, at),
        Some(Outcome::ObservedAccepted)
    );
}

#[tokio::test]
async fn braille_2990_start_capture_submits() {
    submits(START);
}

#[tokio::test]
async fn braille_2990_send_controls_with_background_terminal_submit() {
    submits(SEND);
}

#[tokio::test]
async fn braille_2990_particle_only_surrounding_rows_do_not_block_submit() {
    submits("  ⠀⠁⢀⣿\n› Ask Codex to do anything\n  ⠀⠁⢀⣿\n  ? for shortcuts\n");
}

#[tokio::test]
async fn braille_2990_real_and_braille_only_drafts_refuse_before_typing() {
    for draft in [
        "real ⠁draft⢀",
        "⠁⢀⣿",
        "⠀",
        "⣿",
        "Ask Codex to do anything⠁ extra",
        "Ask Codex⠁",
    ] {
        let Rig {
            mut app,
            pane,
            mut pty,
        } = lock_rig(&CODEX);
        let now = Instant::now();
        observe(&mut app, &format!("› {draft}\n  ? for shortcuts\n"), now);
        let result = app.begin_guarded_submit(&pane, "hello", None, Duration::ZERO, now, false);
        assert_eq!(result.unwrap_err(), "input_not_empty", "{draft}");
        assert!(drain(&mut pty).is_empty(), "{draft}");
        let (ws, pane_id) = app.parse_pane_id(&pane).unwrap();
        assert_eq!(app.raw_text_refusal(ws, pane_id), Some("input_not_empty"));
    }
}

#[tokio::test]
async fn braille_2990_working_and_dialog_controls_still_refuse() {
    for screen in [
        format!("• Working (3s • esc to interrupt)\n{START}"),
        format!("{START}Enter to select · Esc to cancel\n"),
        START.replace("Ask Codex to do anything", "Ask Codex"),
    ] {
        let Rig {
            mut app,
            pane,
            mut pty,
        } = lock_rig(&CODEX);
        let now = Instant::now();
        observe(&mut app, &screen, now);
        assert!(app
            .begin_guarded_submit(&pane, "hello", None, Duration::ZERO, now, false,)
            .is_err());
        assert!(drain(&mut pty).is_empty());
    }
}

#[test]
fn braille_2990_decorated_composer_keeps_live_turn_confirmation() {
    let screen = format!("› hello\n• Working (3s • esc to interrupt)\n{START}");
    assert!(crate::detect::live_turn_follows_submitted(
        Agent::Codex,
        &screen,
        "hello"
    ));
    let different_echo = screen.replacen("› hello", "› hello\n  ⠁", 1);
    assert!(!crate::detect::live_turn_follows_submitted(
        Agent::Codex,
        &different_echo,
        "hello"
    ));
}
