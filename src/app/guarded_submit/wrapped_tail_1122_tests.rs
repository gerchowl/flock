use super::*;

const CLAUDE_16_COLS: &str =
    include_str!("../../../tests/fixtures/wrapped-composer-1126/claude-16-cols.txt");

const SENT: &str = "Please review the pull request for the narrow pane fix and report back \
    with findings, then check https://github.com/gerchowl/flock/pull/1120 for the \
    follow-up discussion about guarded submit confirmation on wrapped composers.";

/// The fixture pane's width, less the two-column prompt.
const ROW_WIDTH: usize = 14;

fn editor(screen: &str) -> Vec<&str> {
    let (rows, _, _) = composer_rows(Agent::Claude, screen, screen).expect("live composer");
    rows.into_iter().filter(|row| !row.is_empty()).collect()
}

#[test]
fn a_captured_narrow_composer_shows_only_the_wrapped_tail() {
    let rows = editor(CLAUDE_16_COLS);
    assert_eq!(rows.first(), Some(&"findings,"));
    assert_eq!(rows.last(), Some(&"composers."));
    // The exact match cannot see it: the head of the text is off screen.
    assert!(!matches_rows(&rows, SENT));
    assert_eq!(
        composer(Agent::Claude, CLAUDE_16_COLS, SENT),
        Composer::Other
    );
    assert!(matches_wrapped_tail(&rows, SENT, ROW_WIDTH));
}

#[test]
fn only_a_tail_of_the_sent_text_counts() {
    let rows = editor(CLAUDE_16_COLS);
    assert!(!matches_wrapped_tail(
        &rows,
        &SENT.replace("composers.", "panes."),
        ROW_WIDTH
    ));
    assert!(!matches_wrapped_tail(
        &rows,
        "on wrapped composers.",
        ROW_WIDTH
    ));
    // One row is not a wrap: an unwrapped editor must match exactly.
    assert!(!matches_wrapped_tail(&["composers."], SENT, ROW_WIDTH));
    assert!(!matches_wrapped_tail(&["", ""], SENT, ROW_WIDTH));
    // A wrap mid-word and a wrap at a space both join back.
    assert!(matches_wrapped_tail(
        &["https://gith", "ub.com/x and", "more"],
        "see https://github.com/x and more",
        10
    ));
    // Not clipped: the whole text fits in the rows shown, so its start was
    // lost rather than scrolled off.
    assert!(!matches_wrapped_tail(
        &["delta", "epsilon"],
        "gamma delta epsilon",
        ROW_WIDTH
    ));
}
