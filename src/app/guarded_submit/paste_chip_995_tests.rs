use super::*;

const CLAUDE_CHIP: &str = include_str!("../../../tests/fixtures/paste-chip-995/claude-chip.txt");
const CODEX_CHIP: &str = include_str!("../../../tests/fixtures/paste-chip-995/codex-chip.txt");

/// The editor row of a captured screen, read the way the guarded submit reads it.
fn editor(agent: Agent, screen: &str) -> Vec<String> {
    let (rows, _, _) = composer_rows(agent, screen, screen).expect("live composer");
    rows.into_iter()
        .filter(|row| !row.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn captured_chips_read_as_one_chip_row_in_an_idle_composer() {
    let text = ("Reply exactly CHIP_CAPTURE_995. Do not use tools. ".repeat(22))
        .trim()
        .to_string();
    for (agent, screen, chip) in [
        (Agent::Claude, CLAUDE_CHIP, "[Pasted text #1]"),
        (Agent::Codex, CODEX_CHIP, "[Pasted Content 1099 chars]"),
    ] {
        assert_eq!(editor(agent, screen), vec![chip.to_string()]);
        assert!(is_paste_chip(agent, chip));
        // Idle and not the sent text: Other, which the chip turns into ours.
        assert_eq!(composer(agent, screen, &text), Composer::Other);
    }
}

#[test]
fn only_exact_chips_stand_in_for_the_text() {
    for row in ["[Pasted text #1]", "[Pasted text #12 +40 lines]"] {
        assert!(is_paste_chip(Agent::Claude, row), "{row}");
        assert!(!is_paste_chip(Agent::Codex, row), "{row}");
    }
    for row in ["[Pasted Content]", "[Pasted Content 4000 chars]"] {
        assert!(is_paste_chip(Agent::Codex, row), "{row}");
        assert!(!is_paste_chip(Agent::Claude, row), "{row}");
    }
    for row in [
        "[Pasted text #1] and more",
        "see [Pasted text #1]",
        "[Pasted text #]",
        "[Pasted text #1 +x lines]",
        "[Pasted text #1 +3]",
        "[Image #1]",
    ] {
        assert!(!is_paste_chip(Agent::Claude, row), "{row}");
    }
    for row in ["[Pasted Content x chars]", "[Pasted Contents]"] {
        assert!(!is_paste_chip(Agent::Codex, row), "{row}");
    }
    assert!(!is_paste_chip(Agent::OpenCode, "[Pasted text #1]"));
}
