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
//! They agreed only by construction: truncation kept one rendered row per
//! logical line, so a width-independent count happened to match a
//! width-dependent render. Wrapping (#254) breaks that identity, and three
//! implementations converging under feature pressure is how "copy returns text
//! that is not what you highlighted" ships. So row identity lives here, and the
//! UI layer only decides how a row is *painted*.
//!
//! [`PromptRow::text`] is the exact text of the row, prefix included. A styler
//! may split it into spans, but it must not change its length — the column
//! coordinates copy uses index into this string.
//!
//! # Wrapping
//!
//! Bodies wrap rather than truncate, which is the whole point of reading the
//! transcript instead of the pane: the parser hands us unwrapped logical text,
//! so the panel can lay it out at whatever width it currently has (#246).
//!
//! The rules match `wrap-ansi` (MIT), which is what Claude Code's own Ink-based
//! TUI wraps with, so the panel breaks text where the agent's output does:
//! break on whitespace, hard-split any single token longer than the line, and
//! keep the source line's own indentation on continuation rows so code and
//! lists stay aligned. Claude Code itself is a minified proprietary bundle with
//! no source release, so this is a reimplementation of published behaviour, not
//! a port of its code.
//!
//! # Addressing
//!
//! Rows carry `(entry, body_offset)` — the entry they came from and the byte
//! offset into that entry's text. Row indices do not survive a relayout, so
//! anything that must outlive one (the scroll anchor, a search match) addresses
//! content through these instead.

use crate::terminal::{PromptHistoryEntry, PromptHistoryKind};
use std::borrow::Cow;
use std::time::Instant;
use unicode_width::UnicodeWidthChar;

/// Which part of an entry a row came from. The renderer paints chrome and body
/// differently; copy and scroll math treat them alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptRowKind {
    /// The `· prompt · 12s ago` header line that opens every entry.
    Chrome,
    /// One laid-out line of the entry's body.
    Body,
}

/// One laid-out row of the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptRow {
    pub kind: PromptRowKind,
    /// Exact row text including its prefix. Never re-truncate or re-pad this.
    pub text: String,
    /// Index of the source entry in the history this row was laid out from.
    pub entry: usize,
    /// Byte offset into the source entry's `text` where this row's content
    /// begins. Zero for [`PromptRowKind::Chrome`], which has no source text.
    pub body_offset: usize,
}

/// Columns reserved by every row's prefix (`· ` for chrome, two spaces of
/// indent for body).
const PREFIX_COLS: usize = 2;

/// Tab stop used when expanding tabs before measuring. Transcript text is
/// mostly agent prose and code; tabs are rare but must not measure as zero.
const TAB_COLS: usize = 4;

/// Lay out one entry at `width` columns, appending its rows to `out`.
///
/// Row shape, which the renderer relies on: exactly one [`PromptRowKind::Chrome`]
/// row, followed by one or more [`PromptRowKind::Body`] rows per non-empty
/// logical line. An entry with an empty body contributes the chrome row alone.
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
    // Chrome is a fixed one-line summary, so it truncates rather than wraps —
    // wrapping "reply · 3h ago" onto a second row would be noise. The prefix
    // itself is subject to the width: at 1 column even `· ` does not fit.
    let chrome = truncate_to_cols(
        &format!("{kind_label} \u{b7} {}", entry.relative_age(now)),
        body_width,
    );
    out.push(PromptRow {
        kind: PromptRowKind::Chrome,
        text: truncate_to_cols(&format!("\u{b7} {chrome}"), width),
        entry: index,
        body_offset: 0,
    });

    for (line_offset, line) in body_lines(&entry.text) {
        // With no columns left for text, one truncated row per logical line
        // keeps the row COUNT stable and the panel non-overflowing. Wrapping
        // into a zero-width budget would either emit the line unwrapped (an
        // overlong row punching through the border) or one row per character.
        if body_width == 0 {
            out.push(PromptRow {
                kind: PromptRowKind::Body,
                text: truncate_to_cols("  ", width),
                entry: index,
                body_offset: line_offset,
            });
            continue;
        }
        for (segment_offset, segment) in wrap_line(line, body_width) {
            out.push(PromptRow {
                kind: PromptRowKind::Body,
                // Last-resort clamp: a single character wider than the whole
                // budget (a CJK glyph at 1 column) cannot be split any further,
                // so the row is elided rather than allowed through the border.
                text: truncate_to_cols(&format!("  {segment}"), width),
                entry: index,
                body_offset: line_offset + segment_offset,
            });
        }
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

/// Body lines paired with their byte offset into the entry text, with leading
/// and trailing blank lines dropped (matching `rendered_line_count`'s trimming).
fn body_lines(text: &str) -> Vec<(usize, &str)> {
    let mut lines: Vec<(usize, &str)> = Vec::new();
    let mut offset = 0usize;
    for raw in text.split('\n') {
        lines.push((offset, raw.trim_end()));
        // `+ 1` for the '\n' that `split` consumed. The final segment has no
        // trailing newline, but nothing reads past it.
        offset += raw.len() + 1;
    }
    let first_content = lines.iter().position(|(_, line)| !line.is_empty());
    let Some(first) = first_content else {
        return Vec::new();
    };
    let last = lines
        .iter()
        .rposition(|(_, line)| !line.is_empty())
        .unwrap_or(first);
    lines[first..=last].to_vec()
}

/// Wrap one logical line to `width` columns, yielding `(byte offset in line,
/// text)` per row.
///
/// Empty input yields a single empty row so interior blank lines survive as
/// paragraph breaks. A `width` of zero yields the line unwrapped — the panel is
/// unusable at that size and inventing rows would only churn scroll bounds.
fn wrap_line(line: &str, width: usize) -> Vec<(usize, String)> {
    if line.is_empty() || width == 0 {
        return vec![(0, line.to_string())];
    }
    // Every row carries the source line's indentation, so wrapped code and list
    // items stay aligned under their first row. An indent that leaves no room
    // for text is dropped rather than emitting rows that are all indent.
    let indent_bytes = line.len() - line.trim_start().len();
    let indent = expand_tabs(&line[..indent_bytes]);
    let indent = if display_width(&indent) + 1 >= width {
        String::new()
    } else {
        indent
    };
    let indent_cols = display_width(&indent);
    let budget = width.saturating_sub(indent_cols).max(1);
    let body = &line[indent_bytes..];

    let mut rows: Vec<(usize, String)> = Vec::new();
    let mut current = String::new();
    let mut current_cols = 0usize;
    let mut current_start: Option<usize> = None;

    for (token_offset, token) in tokens(body) {
        let offset = indent_bytes + token_offset;
        let token_cols = display_width(token);

        // Whitespace at a row break is dropped, as `wrap-ansi` does — a wrapped
        // row must not begin with the space that caused the wrap. But dropping
        // it must also END the row: a run that does not fit is itself the wrap
        // point, and skipping it silently would let a shorter following word
        // fit onto the current row and fuse onto the previous one with no
        // separator ("| Alpha    | Beta |" → "| Alpha|"), putting text on
        // screen the agent never wrote — and into the clipboard.
        if token.chars().all(char::is_whitespace) {
            if let Some(start) = current_start {
                if current_cols + token_cols <= budget {
                    current.push_str(&expand_tabs(token));
                    current_cols += token_cols;
                } else {
                    rows.push((start, format!("{indent}{}", std::mem::take(&mut current))));
                    current_start = None;
                    current_cols = 0;
                }
            }
            continue;
        }

        if current_start.is_some() && current_cols + token_cols > budget {
            let start = current_start.take().unwrap_or(offset);
            rows.push((start, format!("{indent}{}", std::mem::take(&mut current))));
            current_cols = 0;
        }

        if token_cols > budget {
            // A single token longer than the row (a path, a URL, a hash) is
            // hard-split rather than allowed to overflow the panel. The flush
            // above already emptied `current`.
            for (chunk_offset, chunk) in hard_split(token, budget) {
                rows.push((offset + chunk_offset, format!("{indent}{chunk}")));
            }
            continue;
        }

        current_start.get_or_insert(offset);
        current.push_str(&expand_tabs(token));
        current_cols += token_cols;
    }

    if let Some(start) = current_start {
        rows.push((start, format!("{indent}{current}")));
    }
    if rows.is_empty() {
        rows.push((0, indent));
    }
    rows
}

/// Split a line into alternating whitespace and non-whitespace runs, each with
/// its byte offset.
fn tokens(line: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut in_space: Option<bool> = None;
    for (idx, ch) in line.char_indices() {
        let is_space = ch.is_whitespace();
        match in_space {
            Some(previous) if previous != is_space => {
                out.push((start, &line[start..idx]));
                start = idx;
                in_space = Some(is_space);
            }
            None => in_space = Some(is_space),
            _ => {}
        }
    }
    if start < line.len() {
        out.push((start, &line[start..]));
    }
    out
}

/// Break an over-long token into `width`-column chunks on char boundaries.
fn hard_split(token: &str, width: usize) -> Vec<(usize, String)> {
    let mut chunks = Vec::new();
    if width == 0 {
        return vec![(0, token.to_string())];
    }
    let mut chunk = String::new();
    let mut chunk_cols = 0usize;
    let mut chunk_start = 0usize;
    for (idx, ch) in token.char_indices() {
        let ch_cols = char_width(ch);
        if chunk_cols + ch_cols > width && !chunk.is_empty() {
            chunks.push((chunk_start, std::mem::take(&mut chunk)));
            chunk_cols = 0;
            chunk_start = idx;
        }
        chunk.push(ch);
        chunk_cols += ch_cols;
    }
    if !chunk.is_empty() {
        chunks.push((chunk_start, chunk));
    }
    chunks
}

fn expand_tabs(text: &str) -> String {
    if !text.contains('\t') {
        return text.to_string();
    }
    text.replace('\t', &" ".repeat(TAB_COLS))
}

fn char_width(ch: char) -> usize {
    if ch == '\t' {
        return TAB_COLS;
    }
    ch.width().unwrap_or(0)
}

/// Display width in terminal columns, counting tabs as [`TAB_COLS`].
pub(crate) fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// Truncate to `max_cols` display columns, marking elision with `…`.
pub(crate) fn truncate_to_cols(text: &str, max_cols: usize) -> String {
    if display_width(text) <= max_cols {
        return text.to_string();
    }
    if max_cols == 0 {
        return String::new();
    }
    if max_cols == 1 {
        return "\u{2026}".to_string();
    }
    let mut out = String::new();
    let mut cols = 0usize;
    for ch in text.chars() {
        let ch_cols = char_width(ch);
        if cols + ch_cols > max_cols - 1 {
            break;
        }
        out.push(ch);
        cols += ch_cols;
    }
    out.push('\u{2026}');
    out
}

/// Cached layout for the one open prompt-history panel.
///
/// Only a single panel can be open at a time (`AppState::expanded_prompt_pane`
/// is one `Option`), so this holds exactly one pane's rows rather than a map.
/// It is refreshed from the pre-render pass, where `&mut AppState` is
/// available, and read during render and from input routing.
///
/// A miss is not an error: callers fall back to laying out inline, so the cache
/// is purely an optimisation and tests need no render pass to get correct
/// numbers.
#[derive(Debug, Default, Clone)]
pub(crate) struct PromptLayoutCache {
    key: Option<PromptLayoutKey>,
    rows: Vec<PromptRow>,
}

/// What the cached rows were laid out from. Any change invalidates them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PromptLayoutKey {
    pane: crate::layout::PaneId,
    width: u16,
    /// Bumped by `TerminalState` on every history mutation.
    generation: u64,
}

impl PromptLayoutCache {
    /// Rows for `history`, from cache when the key matches and laid out inline
    /// when it does not.
    pub(crate) fn rows<'a>(
        &'a self,
        pane: crate::layout::PaneId,
        width: u16,
        generation: u64,
        history: &[PromptHistoryEntry],
        now: Instant,
    ) -> Cow<'a, [PromptRow]> {
        let key = PromptLayoutKey {
            pane,
            width,
            generation,
        };
        if self.key == Some(key) {
            return Cow::Borrowed(&self.rows);
        }
        Cow::Owned(layout_history(history, width, now))
    }

    /// Re-lay out `history` if the key moved. Called from the pre-render pass.
    pub(crate) fn refresh(
        &mut self,
        pane: crate::layout::PaneId,
        width: u16,
        generation: u64,
        history: &[PromptHistoryEntry],
        now: Instant,
    ) {
        let key = PromptLayoutKey {
            pane,
            width,
            generation,
        };
        if self.key == Some(key) {
            return;
        }
        self.rows = layout_history(history, width, now);
        self.key = Some(key);
    }

    /// Drop cached rows — the panel closed, or its pane went away.
    pub(crate) fn clear(&mut self) {
        self.key = None;
        self.rows.clear();
    }
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

    /// Body rows with the panel's own 2-column indent stripped, leaving the
    /// content as laid out — that prefix is chrome, not part of the text.
    fn body_text(rows: &[PromptRow]) -> Vec<String> {
        rows.iter()
            .filter(|row| row.kind == PromptRowKind::Body)
            .map(|row| {
                row.text
                    .strip_prefix("  ")
                    .unwrap_or(&row.text)
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// Layout is deterministic at a given width — the weak precondition for
    /// the three consumers agreeing. The invariant that actually matters (the
    /// scroll-bounds path, the paint window, and copy all seeing the SAME
    /// rows) is pinned in `app::actions::tests`, where those three call sites
    /// live; asserting it here could only compare this function to itself.
    #[test]
    fn layout_is_deterministic_at_a_given_width() {
        let history = vec![
            entry(PromptHistoryKind::Prompt, "short"),
            entry(
                PromptHistoryKind::Reply,
                "a considerably longer body line that will need wrapping\nsecond line",
            ),
            entry(PromptHistoryKind::Recap, "\u{203b} recap: did the thing"),
            entry(PromptHistoryKind::Prompt, ""),
        ];
        let now = Instant::now();
        for width in [4u16, 10, 24, 80, 200] {
            assert_eq!(
                layout_history(&history, width, now),
                layout_history(&history, width, now),
                "layout must not vary between calls at width {width}"
            );
        }
    }

    /// No row may exceed the panel width at ANY width the panel can reach.
    /// The pane can be dragged down to an inner width of 4, which leaves zero
    /// columns for body text after the row prefix — a naive "nothing fits, so
    /// emit the line unwrapped" would push overlong rows out past the border.
    #[test]
    fn no_row_overflows_the_panel_at_any_width() {
        let history = vec![
            entry(
                PromptHistoryKind::Reply,
                "a line of prose that is far too long",
            ),
            entry(
                PromptHistoryKind::Prompt,
                "    indented and also rather long",
            ),
            entry(
                PromptHistoryKind::Reply,
                "unbreakable/path/with/no/spaces/at/all",
            ),
            entry(PromptHistoryKind::Reply, "日本語のテキストです"),
        ];
        let now = Instant::now();
        for width in 1u16..=40 {
            for row in layout_history(&history, width, now) {
                assert!(
                    display_width(&row.text) <= usize::from(width),
                    "row {:?} is {} cols at width {width}",
                    row.text,
                    display_width(&row.text)
                );
            }
        }
    }

    /// A whitespace run that does not fit is itself the wrap point. Dropping it
    /// without ending the row lets a shorter following word fit onto the
    /// current row and fuse onto the previous one — putting text on screen the
    /// agent never wrote, and into the clipboard when that row is copied.
    #[test]
    fn a_gap_too_wide_to_fit_breaks_the_row_instead_of_fusing_words() {
        // Alignment runs like this are everywhere in agent output: tables,
        // aligned assignments, padded columns.
        let history = vec![entry(PromptHistoryKind::Reply, "| Alpha    | Beta |")];
        let rows = layout_history(&history, 12, Instant::now());
        let bodies = body_text(&rows);
        for row in &bodies {
            assert!(
                !row.contains("Alpha|"),
                "words fused across a dropped gap: {row:?}"
            );
        }
        let words: Vec<&str> = bodies
            .iter()
            .flat_map(|row| row.split_whitespace())
            .collect();
        assert_eq!(words, vec!["|", "Alpha", "|", "Beta", "|"]);
    }

    /// The same hazard with tabs, which expand to several columns and so are
    /// far more likely to exceed a remaining budget than a single space.
    #[test]
    fn a_tab_gap_that_does_not_fit_also_breaks_the_row() {
        let history = vec![entry(PromptHistoryKind::Reply, "world\t\thello")];
        let rows = layout_history(&history, 12, Instant::now());
        for row in body_text(&rows) {
            assert!(
                !row.contains("worldhello"),
                "tab run was dropped without ending the row: {row:?}"
            );
        }
    }

    /// Wrapping, not truncation: no content may be dropped, and no row may
    /// exceed the panel width. This is the defect #254 exists to fix.
    #[test]
    fn long_lines_wrap_instead_of_being_truncated() {
        let body = "the quick brown fox jumps over the lazy dog and keeps running";
        let history = vec![entry(PromptHistoryKind::Reply, body)];
        let rows = layout_history(&history, 24, Instant::now());
        let bodies = body_text(&rows);
        assert!(bodies.len() > 1, "long line must occupy several rows");
        for row in &rows {
            assert!(
                display_width(&row.text) <= 24,
                "row overflowed the panel: {:?}",
                row.text
            );
            assert!(!row.text.contains('\u{2026}'), "body must not be elided");
        }
        let rejoined = bodies
            .iter()
            .map(|row| row.trim_start().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(rejoined, body, "wrapping must preserve every word");
    }

    /// Re-wrapping at a new width is the whole premise: the same content must
    /// lay out differently, and still lose nothing.
    #[test]
    fn the_same_entry_relayouts_at_a_new_width() {
        let body = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
        let history = vec![entry(PromptHistoryKind::Reply, body)];
        let now = Instant::now();
        let narrow = layout_history(&history, 20, now);
        let wide = layout_history(&history, 200, now);
        assert!(narrow.len() > wide.len());
        assert_eq!(body_text(&wide).len(), 1, "it all fits on one wide row");
        for rows in [&narrow, &wide] {
            let rejoined = body_text(rows)
                .iter()
                .map(|row| row.trim_start().to_string())
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(rejoined, body);
        }
    }

    /// A path or URL with no spaces cannot be broken on whitespace, and must
    /// not be allowed to overflow the panel instead.
    #[test]
    fn an_overlong_token_is_hard_split() {
        let token = "src/ui/panes.rs::render_prompt_history_panel::inner::deeply::nested";
        let history = vec![entry(PromptHistoryKind::Reply, token)];
        let rows = layout_history(&history, 20, Instant::now());
        let bodies = body_text(&rows);
        assert!(bodies.len() > 1, "unbreakable token must still be split");
        for row in &rows {
            assert!(display_width(&row.text) <= 20);
        }
        assert_eq!(
            bodies.join(""),
            token,
            "a hard split must lose no characters"
        );
    }

    /// Wrapped code and list items stay readable only if continuation rows keep
    /// the source line's indentation.
    #[test]
    fn continuation_rows_keep_the_source_indentation() {
        let history = vec![entry(
            PromptHistoryKind::Reply,
            "    indented code that runs long enough to wrap twice over",
        )];
        let rows = layout_history(&history, 24, Instant::now());
        let bodies = body_text(&rows);
        assert!(bodies.len() > 1);
        for row in &bodies[1..] {
            assert!(
                row.starts_with("    "),
                "continuation lost the source indent: {row:?}"
            );
        }
    }

    /// Interior blank lines are paragraph breaks in agent prose and must not be
    /// collapsed away, while leading and trailing padding is trimmed.
    #[test]
    fn interior_blank_lines_survive_and_border_blanks_are_trimmed() {
        let history = vec![entry(PromptHistoryKind::Reply, "\n\nfirst\n\nsecond\n\n")];
        let rows = layout_history(&history, 40, Instant::now());
        assert_eq!(body_text(&rows), vec!["first", "", "second"]);
    }

    /// Rows carry their source entry and byte offset so scroll anchors and
    /// search matches address content, not row numbers, which no relayout
    /// preserves.
    #[test]
    fn rows_carry_addressable_source_coordinates() {
        let history = vec![
            entry(PromptHistoryKind::Prompt, "first\nsecond"),
            entry(PromptHistoryKind::Reply, "third"),
        ];
        let rows = layout_history(&history, 40, Instant::now());
        assert_eq!(
            rows.iter().map(|row| row.entry).collect::<Vec<_>>(),
            vec![0, 0, 0, 1, 1]
        );
        // "first" at 0, "second" after the newline at 6.
        let offsets: Vec<usize> = rows
            .iter()
            .filter(|row| row.entry == 0 && row.kind == PromptRowKind::Body)
            .map(|row| row.body_offset)
            .collect();
        assert_eq!(offsets, vec![0, 6]);
    }

    /// Offsets must stay valid indices into the source text at every width, or
    /// an anchor resolved after a resize slices mid-character.
    #[test]
    fn body_offsets_are_char_boundaries_in_the_source_text() {
        let text = "ααα βββ γγγ δδδ εεε ζζζ ηηη θθθ";
        let history = vec![entry(PromptHistoryKind::Reply, text)];
        for width in [6u16, 12, 40] {
            for row in layout_history(&history, width, Instant::now()) {
                if row.kind != PromptRowKind::Body {
                    continue;
                }
                assert!(
                    text.is_char_boundary(row.body_offset),
                    "offset {} is not a char boundary at width {width}",
                    row.body_offset
                );
            }
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

    /// Wide characters must be measured in columns, not chars, or CJK content
    /// overflows the panel by up to 2x.
    #[test]
    fn wide_characters_are_measured_in_columns() {
        let history = vec![entry(PromptHistoryKind::Reply, "日本語のテキストです")];
        let rows = layout_history(&history, 14, Instant::now());
        for row in &rows {
            assert!(
                display_width(&row.text) <= 14,
                "wide row overflowed: {:?} = {} cols",
                row.text,
                display_width(&row.text)
            );
        }
    }

    #[test]
    fn truncation_never_exceeds_the_budget_and_marks_elision() {
        assert_eq!(truncate_to_cols("abcdef", 4), "abc\u{2026}");
        assert_eq!(truncate_to_cols("abc", 3), "abc");
        assert_eq!(truncate_to_cols("abc", 1), "\u{2026}");
        assert_eq!(truncate_to_cols("abc", 0), "");
        assert_eq!(truncate_to_cols("ééééé", 3), "éé\u{2026}");
    }

    /// The panel can be dragged to a few columns; layout must degrade rather
    /// than panic on the prefix subtraction.
    #[test]
    fn degenerate_widths_still_lay_out() {
        let history = vec![entry(PromptHistoryKind::Prompt, "body text here")];
        for width in [0u16, 1, 2, 3] {
            let rows = layout_history(&history, width, Instant::now());
            assert!(!rows.is_empty(), "width {width} produced no rows");
            assert_eq!(rows[0].kind, PromptRowKind::Chrome);
        }
    }

    /// The cache must be indistinguishable from laying out inline — it is an
    /// optimisation, and a stale hit would desynchronise scroll from render.
    #[test]
    fn cache_hits_match_inline_layout_and_misses_fall_back() {
        let pane = crate::layout::PaneId::alloc();
        let other_pane = crate::layout::PaneId::alloc();
        let history = vec![entry(PromptHistoryKind::Reply, "some body text to wrap")];
        let now = Instant::now();
        let mut cache = PromptLayoutCache::default();

        // Miss: no refresh yet, but the answer is still correct.
        let fallback = cache.rows(pane, 20, 1, &history, now);
        assert_eq!(fallback.as_ref(), layout_history(&history, 20, now));
        assert!(matches!(fallback, Cow::Owned(_)));

        cache.refresh(pane, 20, 1, &history, now);
        let hit = cache.rows(pane, 20, 1, &history, now);
        assert!(matches!(hit, Cow::Borrowed(_)), "refresh should have hit");
        assert_eq!(hit.as_ref(), layout_history(&history, 20, now));

        // A width change, a new generation, or another pane must all miss.
        assert!(matches!(
            cache.rows(pane, 21, 1, &history, now),
            Cow::Owned(_)
        ));
        assert!(matches!(
            cache.rows(pane, 20, 2, &history, now),
            Cow::Owned(_)
        ));
        assert!(matches!(
            cache.rows(other_pane, 20, 1, &history, now),
            Cow::Owned(_)
        ));

        cache.clear();
        assert!(matches!(
            cache.rows(pane, 20, 1, &history, now),
            Cow::Owned(_)
        ));
    }
}
