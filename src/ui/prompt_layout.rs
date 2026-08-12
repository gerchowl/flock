//! Row layout for the prompt-history panel — the single source of truth for
//! "which rows does this history occupy at this width".
//!
//! Three consumers need that answer and, until this module existed, each
//! derived it independently:
//!
//! * the renderer (`ui::panes::build_prompt_history_lines`) built styled lines
//! * scroll bounds (`AppState::prompt_history_max_offset_for`) summed
//!   `PromptHistoryEntry::rendered_line_count`
//! * copy (`ui::panes::extract_prompt_panel_selection`) rebuilt the lines and
//!   sliced them by column
//!
//! They agreed only by construction: truncation keeps one rendered row per
//! logical line, so a width-independent count happened to match a
//! width-dependent render. Wrapping (#254) breaks that identity, and three
//! implementations converging under feature pressure is how "copy returns text
//! that is not what you highlighted" ships. So row identity lives here, and the
//! UI layer only decides how a row is *painted*.
//!
//! [`PromptRow::text`] is the exact text of the row, prefix included. A styler
//! may split it into spans, but it must not change its length — the column
//! coordinates copy uses index into this string.

use crate::terminal::{PromptHistoryEntry, PromptHistoryKind};
use std::time::Instant;

/// Which part of an entry a row came from. The renderer paints chrome and body
/// differently; copy and scroll math treat them alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptRowKind {
    /// The `· prompt · 12s ago` header line that opens every entry.
    Chrome,
    /// One line of the entry's body.
    Body,
}

/// One laid-out row of the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptRow {
    pub kind: PromptRowKind,
    /// Exact row text including its prefix. Never re-truncate or re-pad this.
    pub text: String,
    /// Which entry in the source history this row belongs to. Content-addressed
    /// scroll and search jumps resolve through this rather than through a row
    /// index, which no relayout preserves (#254).
    pub entry: usize,
}

/// Columns reserved by every row's prefix (`· ` for chrome, two spaces of
/// indent for body). Body text is truncated to the remaining width.
const PREFIX_COLS: usize = 2;

/// Lay out one entry at `width` columns.
///
/// Row shape, which the renderer relies on: exactly one [`PromptRowKind::Chrome`]
/// row followed by one [`PromptRowKind::Body`] row per non-empty logical line.
/// An entry with an empty body contributes the chrome row alone — matching
/// [`PromptHistoryEntry::rendered_line_count`], which the retention cap uses.
pub(crate) fn layout_entry(
    entry: &PromptHistoryEntry,
    index: usize,
    width: u16,
    now: Instant,
    out: &mut Vec<PromptRow>,
) {
    let width = usize::from(width);
    let body_width = width.saturating_sub(PREFIX_COLS);

    let kind_label = match entry.kind {
        PromptHistoryKind::Prompt => "prompt",
        PromptHistoryKind::Reply => "reply",
        PromptHistoryKind::Recap => "recap",
    };
    let chrome = truncate_to_cols(
        &format!("{kind_label} \u{b7} {}", entry.relative_age(now)),
        body_width,
    );
    out.push(PromptRow {
        kind: PromptRowKind::Chrome,
        text: format!("\u{b7} {chrome}"),
        entry: index,
    });

    for body in trimmed_body_lines(&entry.text) {
        out.push(PromptRow {
            kind: PromptRowKind::Body,
            text: format!("  {}", truncate_to_cols(body, body_width)),
            entry: index,
        });
    }
}

/// Lay out a whole history, oldest entry first.
pub(crate) fn layout_history(
    history: &[PromptHistoryEntry],
    width: u16,
    now: Instant,
) -> Vec<PromptRow> {
    let mut rows = Vec::new();
    for (index, entry) in history.iter().enumerate() {
        layout_entry(entry, index, width, now, &mut rows);
    }
    rows
}

/// Total rows a history occupies at `width` — the scroll-bound input.
///
/// Goes through the same layout as the renderer rather than re-deriving a
/// count, so the two cannot disagree.
pub(crate) fn layout_row_count(history: &[PromptHistoryEntry], width: u16, now: Instant) -> usize {
    history
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let mut rows = Vec::new();
            layout_entry(entry, index, width, now, &mut rows);
            rows.len()
        })
        .sum()
}

/// Body lines with leading and trailing blank lines dropped, matching
/// [`PromptHistoryEntry::rendered_line_count`]'s trimming.
fn trimmed_body_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .skip_while(|line| line.is_empty())
        .collect();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

/// Truncate to `max_cols` characters, marking elision with `…`.
///
/// Columns index the char stream, so wide/zero-width characters count as one —
/// the same approximation the panel's column slicing makes.
pub(crate) fn truncate_to_cols(text: &str, max_cols: usize) -> String {
    if text.chars().count() <= max_cols {
        return text.to_string();
    }
    if max_cols == 0 {
        return String::new();
    }
    if max_cols == 1 {
        return "\u{2026}".to_string();
    }
    let prefix: String = text.chars().take(max_cols.saturating_sub(1)).collect();
    format!("{prefix}\u{2026}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn entry(kind: PromptHistoryKind, text: &str) -> PromptHistoryEntry {
        PromptHistoryEntry {
            kind,
            text: text.to_string(),
            recorded_at: Instant::now(),
            wall_clock: None,
        }
    }

    /// The invariant this module exists to hold: what scroll math counts is
    /// exactly what the renderer draws and what copy can slice. If these three
    /// ever disagree, the panel scrolls past content or copies the wrong rows.
    #[test]
    fn row_count_matches_the_rows_laid_out_at_every_width() {
        let history = vec![
            entry(PromptHistoryKind::Prompt, "short"),
            entry(
                PromptHistoryKind::Reply,
                "a considerably longer body line that will be truncated\nsecond line",
            ),
            entry(PromptHistoryKind::Recap, "\u{203b} recap: did the thing"),
            entry(PromptHistoryKind::Prompt, ""),
        ];
        let now = Instant::now();
        for width in [4u16, 10, 24, 80, 200] {
            let rows = layout_history(&history, width, now);
            assert_eq!(
                layout_row_count(&history, width, now),
                rows.len(),
                "count and layout disagreed at width {width}"
            );
        }
    }

    /// The retention cap (`append_with_cap`) budgets in `rendered_line_count`,
    /// so the layout must produce that many rows or the cap silently drifts
    /// from what is on screen.
    #[test]
    fn layout_agrees_with_rendered_line_count() {
        let cases = [
            entry(PromptHistoryKind::Prompt, "one line"),
            entry(PromptHistoryKind::Reply, "a\nb\nc"),
            entry(PromptHistoryKind::Recap, "\n\npadded\n\n"),
            entry(PromptHistoryKind::Prompt, ""),
        ];
        let now = Instant::now();
        for case in &cases {
            let mut rows = Vec::new();
            layout_entry(case, 0, 80, now, &mut rows);
            assert_eq!(
                rows.len(),
                case.rendered_line_count(),
                "layout and rendered_line_count disagreed for {:?}",
                case.text
            );
        }
    }

    #[test]
    fn every_entry_opens_with_exactly_one_chrome_row() {
        let history = vec![
            entry(PromptHistoryKind::Prompt, "a\nb"),
            entry(PromptHistoryKind::Reply, ""),
        ];
        let rows = layout_history(&history, 40, Instant::now());
        let chrome: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.kind == PromptRowKind::Chrome)
            .map(|(idx, _)| idx)
            .collect();
        assert_eq!(chrome, vec![0, 3], "one chrome row per entry, at its head");
        assert!(rows[0].text.starts_with('\u{b7}'));
        assert!(rows[1].text.starts_with("  "), "body rows are indented");
    }

    /// Rows carry their source entry so scroll anchors and search matches can
    /// address content rather than row numbers, which no relayout preserves.
    #[test]
    fn rows_carry_their_source_entry_index() {
        let history = vec![
            entry(PromptHistoryKind::Prompt, "first\nsecond"),
            entry(PromptHistoryKind::Reply, "third"),
        ];
        let rows = layout_history(&history, 40, Instant::now());
        assert_eq!(
            rows.iter().map(|row| row.entry).collect::<Vec<_>>(),
            vec![0, 0, 0, 1, 1]
        );
    }

    #[test]
    fn truncation_never_exceeds_the_budget_and_marks_elision() {
        assert_eq!(truncate_to_cols("abcdef", 4), "abc\u{2026}");
        assert_eq!(truncate_to_cols("abc", 3), "abc");
        assert_eq!(truncate_to_cols("abc", 1), "\u{2026}");
        assert_eq!(truncate_to_cols("abc", 0), "");
        // Multi-byte input must truncate on a char boundary, not a byte one.
        assert_eq!(truncate_to_cols("ééééé", 3), "éé\u{2026}");
    }

    /// A width narrower than the prefix must still produce rows rather than
    /// panicking on the subtraction — the panel can be dragged to 4 columns.
    #[test]
    fn degenerate_widths_still_lay_out() {
        let history = vec![entry(PromptHistoryKind::Prompt, "body")];
        for width in [0u16, 1, 2, 3] {
            let rows = layout_history(&history, width, Instant::now());
            assert_eq!(rows.len(), 2, "chrome + one body row at width {width}");
        }
    }
}
