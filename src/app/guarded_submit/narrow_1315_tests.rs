use super::*;
use crate::terminal::TerminalRuntime;

fn frame(body: &str, status: bool) -> String {
    format!(
        "› {body}\r\n{}  ? for shortcu",
        if status { "  GPT-6.1-Sol …\r\n" } else { "" }
    )
}

#[tokio::test]
async fn narrow_1315_clipped_shortcut_requires_actual_width() {
    for width in 1_u16..17 {
        let footer = &"  ? for shortcuts"[..usize::from(width)];
        let hint: String = "Ask Codex to do anything"
            .chars()
            .take(usize::from(width.saturating_sub(3)))
            .collect();
        let bytes = format!("› \x1b[2m{hint}\x1b[0m\r\n{footer}");
        let runtime = TerminalRuntime::test_with_screen_bytes(width, 31, bytes.as_bytes());
        assert_eq!(
            runtime_composer(Agent::Codex, &runtime, "brief"),
            if width >= 9 {
                Composer::Empty
            } else {
                Composer::Unknown
            }
        );
        let wider = TerminalRuntime::test_with_screen_bytes(52, 31, bytes.as_bytes());
        assert_eq!(
            runtime_composer(Agent::Codex, &wider, "brief"),
            Composer::Unknown
        );
    }
}

#[tokio::test]
async fn narrow_1315_styled_hint_resolves_clipped_model_boundary() {
    let bytes = include_bytes!("../../../tests/fixtures/codex-narrow-1315/15-cols.ansi");
    let runtime = TerminalRuntime::test_with_screen_bytes(15, 31, bytes);
    assert_eq!(
        runtime_composer(Agent::Codex, &runtime, "brief"),
        Composer::Empty
    );
    assert!(editor_is_empty(Agent::Codex, &runtime));
}

#[tokio::test]
async fn narrow_1315_owned_readback_resolves_clipped_model_boundary() {
    let bytes = frame("one two three\r\nfour five", true);
    let runtime = TerminalRuntime::test_with_screen_bytes(15, 31, bytes.as_bytes());
    let text = "one two three four five";
    assert_eq!(
        runtime_composer(Agent::Codex, &runtime, text),
        Composer::Owned
    );
    assert!(editor_shows(Agent::Codex, &runtime, text));
    assert!(!editor_is_empty(Agent::Codex, &runtime));
}

#[tokio::test]
async fn narrow_1315_model_shaped_draft_is_never_empty_or_stashed() {
    // This is the same visible text as a blank editor above truncated status.
    let bytes = frame("\r\n  GPT-6.1-Sol …", false);
    let runtime = TerminalRuntime::test_with_screen_bytes(15, 31, bytes.as_bytes());
    assert!(matches!(
        runtime_composer(Agent::Codex, &runtime, "brief"),
        Composer::Other | Composer::Unknown
    ));
    assert!(stashable_draft(Agent::Codex, &runtime).is_none());
}

#[tokio::test]
async fn narrow_1315_normal_dim_and_mixed_drafts_remain_refused() {
    for body in [
        "Ask Codex to",
        "\x1b[2mreal draft\x1b[0m",
        "\x1b[2mAsk Codex\x1b[0m draft",
    ] {
        for status in [false, true] {
            let bytes = frame(body, status);
            let runtime = TerminalRuntime::test_with_screen_bytes(15, 31, bytes.as_bytes());
            assert!(
                matches!(
                    runtime_composer(Agent::Codex, &runtime, "brief"),
                    Composer::Other | Composer::Unknown
                ),
                "{body:?}"
            );
        }
    }
}

#[tokio::test]
async fn narrow_1315_missing_dialog_and_working_controls_fail_closed() {
    for bytes in [
        "› \r\n  ? for shortc",
        "› \r\n  ? for shortcu\r\nenter to select",
        "› \r\n  ? for shortcu\r\n• Working (1s)",
        "• Working (1s)\r\n› \r\n  ? for shortcu",
        "› \r\n  ? for shortXX",
        "› \r\n  ? for ",
    ] {
        let runtime = TerminalRuntime::test_with_screen_bytes(15, 31, bytes.as_bytes());
        assert_eq!(
            runtime_composer(Agent::Codex, &runtime, "brief"),
            Composer::Unknown,
            "{bytes:?}"
        );
    }
}

#[tokio::test]
async fn narrow_1315_wrapped_owned_tail_resolves_clipped_model_boundary() {
    let bytes = frame("tail of the\r\nknown prompt", true);
    let runtime = TerminalRuntime::test_with_screen_bytes(15, 31, bytes.as_bytes());
    let text = "a much longer introduction before the tail of the known prompt";
    assert!(editor_shows_wrapped_tail(Agent::Codex, &runtime, text));
    assert!(!editor_shows_wrapped_tail(
        Agent::Codex,
        &runtime,
        "a different injection"
    ));
    assert!(stashable_draft(Agent::Codex, &runtime).is_none());
}

#[tokio::test]
async fn narrow_1315_shortcut_shaped_draft_cannot_authorize_enter_or_stash() {
    // No actual footer is visible: these bytes can be a multiline draft.
    let runtime =
        TerminalRuntime::test_with_screen_bytes(15, 31, b"\xe2\x80\xba \r\n  ? for shortcu");
    assert_eq!(
        runtime_composer(Agent::Codex, &runtime, "brief"),
        Composer::Unknown
    );
    assert!(!editor_is_empty(Agent::Codex, &runtime));
    assert!(stashable_draft(Agent::Codex, &runtime).is_none());
}
