use super::*;
use crate::detect::{detect_agent, Agent};

const INLINE: &str = include_str!("../../../../tests/fixtures/codex-submit-721/inline-idle.txt");
const ALT: &str = include_str!("../../../../tests/fixtures/codex-submit-721/alt-idle.txt");

#[test]
fn codex_721_classifier_shares_live_composer_boundaries() {
    for screen in [INLINE, ALT] {
        let (prompt, footer) = composer_region(screen).expect("captured composer");
        let lines: Vec<_> = screen.lines().collect();
        assert_eq!(lines[prompt].trim(), "› Ask Codex to do anything");
        assert!(lines[footer].trim().starts_with("GPT-6.1-Sol default · "));
        let detection = detect_agent(Some(Agent::Codex), screen);
        assert_eq!(detection.state, AgentState::Idle);
        assert!(detection.visible_idle);
        assert!(!detection.visible_working);
        assert!(!detection.visible_blocker);
    }
}

#[test]
fn codex_721_indentation_tolerance_is_local_to_composer_bounds() {
    for screen in [INLINE, ALT] {
        let padded = screen
            .lines()
            .map(|line| format!("  {line}\n"))
            .collect::<String>();
        assert_eq!(composer_region(&padded), composer_region(screen));
        assert_eq!(
            composer_region(&format!("{padded}\n  • Working (1s • esc to interrupt)")),
            None
        );
    }
    assert!(!codex_prompt_line("  › pasted transcript"));
    assert!(!codex_block_marker_line(
        "  • Working (1s • esc to interrupt)"
    ));
}

#[test]
fn codex_721_classifier_needs_prompt_and_complete_footer_below_it() {
    let footer = "GPT-6.1-Sol default · /fixture/codex-721";
    assert_eq!(composer_region(footer), None);
    assert_eq!(composer_region(&format!("{footer}\n› ")), None);
    assert_eq!(composer_region("› \nGPT-6.1-Sol default"), None);
    assert_eq!(composer_region("› \nGPT-6.1-Sol · /fixture"), None);
    assert_eq!(
        composer_region("› \nGPT-6.1-Sol default · description"),
        None
    );
    assert_eq!(
        composer_region(&format!("› \n{footer}\n• Working (1s • esc to interrupt)")),
        None
    );
    assert_eq!(
        composer_region(include_str!(
            "../../../../tests/fixtures/codex-submit-721/alt-model.txt"
        )),
        None
    );
}
