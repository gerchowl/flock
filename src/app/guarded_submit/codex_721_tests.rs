use super::*;

const INLINE_IDLE: &str = include_str!("../../../tests/fixtures/codex-submit-721/inline-idle.txt");
const ALT_IDLE: &str = include_str!("../../../tests/fixtures/codex-submit-721/alt-idle.txt");

#[test]
fn codex_721_inline_empty_composer() {
    assert_eq!(
        composer(Agent::Codex, INLINE_IDLE, "hello"),
        Composer::Empty
    );
}

#[test]
fn codex_721_alt_empty_composer() {
    assert_eq!(composer(Agent::Codex, ALT_IDLE, "hello"), Composer::Empty);
}

#[test]
fn codex_721_owned_draft_requires_exact_text() {
    for (screen, text) in [
        (
            include_str!("../../../tests/fixtures/codex-submit-721/inline-owned.txt"),
            "Reply exactly INLINE_CAPTURE_721. Do not use tools.",
        ),
        (
            include_str!("../../../tests/fixtures/codex-submit-721/alt-owned.txt"),
            "Reply exactly ALT_CAPTURE_721. Do not use tools.",
        ),
    ] {
        assert_eq!(composer(Agent::Codex, screen, text), Composer::Owned);
        assert_eq!(composer(Agent::Codex, screen, "unrelated"), Composer::Other);
    }
}

#[test]
fn codex_721_working_and_model_picker_cannot_authorize_submit() {
    for screen in [
        include_str!("../../../tests/fixtures/codex-submit-721/inline-working.txt"),
        include_str!("../../../tests/fixtures/codex-submit-721/alt-working.txt"),
        include_str!("../../../tests/fixtures/codex-submit-721/alt-model.txt"),
    ] {
        assert_eq!(composer(Agent::Codex, screen, "hello"), Composer::Unknown);
    }
}

#[test]
fn codex_721_status_text_without_prompt_or_complete_footer_is_unknown() {
    for screen in [
        INLINE_IDLE.replace("› Ask Codex to do anything", "Ask Codex to do anything"),
        INLINE_IDLE.replace("GPT-6.1-Sol default · ", ""),
        INLINE_IDLE.replace(
            "GPT-6.1-Sol default · /fixture/codex-721/…",
            "GPT-6.1-Sol default",
        ),
    ] {
        assert_eq!(composer(Agent::Codex, &screen, "hello"), Composer::Unknown);
    }
}

#[test]
fn codex_721_retained_composer_does_not_override_dialog_controls() {
    for screen in [INLINE_IDLE, ALT_IDLE] {
        let dialog = format!("{screen}\nEnter to select · Esc to cancel");
        assert_eq!(composer(Agent::Codex, &dialog, "hello"), Composer::Unknown);
    }
}

#[test]
fn codex_721_ansi_capture_text_has_the_same_composer() {
    for screen in [
        include_str!("../../../tests/fixtures/codex-submit-721/inline-idle.ansi"),
        include_str!("../../../tests/fixtures/codex-submit-721/alt-idle.ansi"),
    ] {
        let text = crate::control_bytes::strip(screen);
        assert_eq!(composer(Agent::Codex, &text, "hello"), Composer::Empty);
    }
}

#[test]
fn codex_721_legacy_controls_still_bound_the_editor() {
    for footer in ["? for shortcuts", "100% context left"] {
        let screen = format!("› Ask Codex to do anything\n\n{footer}");
        assert_eq!(composer(Agent::Codex, &screen, "hello"), Composer::Empty);
        assert_eq!(
            composer(
                Agent::Codex,
                &screen.replace("Ask Codex to do anything", "hello"),
                "hello"
            ),
            Composer::Owned
        );
    }
}

#[test]
fn codex_721_multiline_draft_keeps_bullets_as_input() {
    let text = "Review these items:\n• tests\n• docs";
    for screen in [INLINE_IDLE, ALT_IDLE] {
        let screen = screen.replace("Ask Codex to do anything", text);
        assert_eq!(composer(Agent::Codex, &screen, text), Composer::Owned);
        assert_eq!(
            composer(Agent::Codex, &screen, "Review these items:"),
            Composer::Other
        );
    }
}
