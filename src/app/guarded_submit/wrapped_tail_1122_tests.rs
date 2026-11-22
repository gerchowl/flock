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
fn a_captured_tail_requires_non_whitespace_evidence_of_clipping() {
    let rows = editor(CLAUDE_16_COLS);
    assert_eq!(rows.first(), Some(&"findings,"));
    assert_eq!(rows.last(), Some(&"composers."));
    // The exact match cannot see it: the head of the text is off screen.
    assert!(!matches_rows(&rows, SENT));
    assert_eq!(
        composer(Agent::Claude, CLAUDE_16_COLS, SENT),
        Composer::Other
    );
    // The capture really is clipped, but its normalized text could fit in
    // this row capacity. That uncertainty must not authorize Enter.
    assert!(SENT.chars().filter(|c| !c.is_whitespace()).count() <= rows.len() * ROW_WIDTH);
    assert!(!matches_wrapped_tail(&rows, SENT, ROW_WIDTH));
    let certainly_clipped = format!("{} {SENT}", "x".repeat(rows.len() * ROW_WIDTH));
    assert!(matches_wrapped_tail(&rows, &certainly_clipped, ROW_WIDTH));
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
    let wrapped = ["https://gith", "ub.com/x and", "more"];
    assert!(wrapped.iter().all(|row| row.len() <= 12));
    assert!(matches_wrapped_tail(
        &wrapped,
        "ThisNonWhitespacePrefixCannotFit see https://github.com/x and more",
        12
    ));
    // Not clipped: the whole text fits in the rows shown, so its start was
    // lost rather than scrolled off.
    assert!(!matches_wrapped_tail(
        &["delta", "epsilon"],
        "gamma delta epsilon",
        ROW_WIDTH
    ));
}

#[test]
fn packed_visible_rows_do_not_gain_clipping_slack_from_whitespace() {
    let rows = ["aaaaaa bbbbbbb", "cccccc ddddddd"];
    assert!(rows.iter().all(|row| row.len() == ROW_WIDTH));
    for separator in [" ", "\n", "\t"] {
        let text = format!("X{separator}{}{separator}{}", rows[0], rows[1]);
        assert!(!matches_rows(&rows, &text));
        assert!(!matches_wrapped_tail(&rows, &text, ROW_WIDTH), "{text:?}");
    }
    // Real clipping still qualifies once even the non-whitespace content
    // cannot fit in the visible rows.
    let clipped = format!("PREFIX {} {}", rows[0], rows[1]);
    assert!(matches_wrapped_tail(&rows, &clipped, ROW_WIDTH));
}
