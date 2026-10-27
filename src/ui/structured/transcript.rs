//! Source-anchored transcript compute/paint; no subscriptions, file reads or authority.
use super::markdown::{lines, safe, wrap, DisplayLine};
use crate::structured::view::contracts::{Action, Card, CardId, Content};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::Paragraph,
    Frame,
};
use std::collections::{BTreeMap, BTreeSet};
use unicode_width::UnicodeWidthStr;

/// Visible physical rows and exact button rectangles, computed together before painting.
pub(crate) struct Transcript {
    area: Rect,
    rows: Vec<DisplayLine>,
    hits: Vec<(Rect, Action)>,
}
impl Transcript {
    /// Borrow a display intent; the controller revalidates its snapshot target generation.
    pub fn hit(&self, x: u16, y: u16) -> Option<&Action> {
        self.hits
            .iter()
            .find(|(r, _)| r.contains((x, y).into()))
            .map(|(_, a)| a)
    }
    /// Transfer the exact painted hit map to the pane's consuming intent adapter.
    pub(super) fn into_hits(self) -> impl Iterator<Item = (Rect, Action)> {
        self.hits.into_iter()
    }
    pub(super) fn positions(&self) -> impl Iterator<Item = (u16, u16)> + '_ {
        self.hits.iter().map(|(r, _)| (r.x, r.y))
    }
    /// Paint only the prepared rows. The source snapshot and computed geometry stay immutable.
    pub fn render(&self, frame: &mut Frame) {
        // Per-frame row cloning is tracked in #2590.
        let rows = self
            .rows
            .iter()
            .map(|r| r.line().clone())
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(rows), self.area);
    }
}

fn content(card: &Card, tools: &BTreeMap<&str, &str>) -> (String, Vec<Line<'static>>) {
    let (title, body) = match &card.content {
        Content::Markdown(t) => return ("Assistant".into(), lines(t)),
        Content::Text(t) => ("Assistant".into(), safe(t)),
        Content::Thinking(t) => ("Thinking".into(), safe(t)),
        Content::RawJson { raw, .. } => ("Unknown / extra JSON".into(), safe(raw)),
        Content::ToolRequest(r) => (
            format!("Permission request: {} ({})", r.tool_name, r.tool_use_id),
            safe(&serde_json::Value::Object(r.input.clone()).to_string()),
        ),
        Content::ToolUse(t) => (
            format!("Tool: {} ({})", t.name, t.id),
            safe(&serde_json::Value::Object(t.input.clone()).to_string()),
        ),
        Content::ToolResult {
            tool_use_id,
            text,
            is_error,
        } => (
            format!(
                "Tool result: {}{}",
                tools
                    .get(tool_use_id.as_str())
                    .map(|name| format!("{name} ({tool_use_id})"))
                    .unwrap_or_else(|| tool_use_id.clone()),
                if *is_error { " [error]" } else { "" }
            ),
            safe(text),
        ),
        Content::Attachment(r) => ("Attachment reference".into(), safe(&format!("{r:?}"))),
        Content::Gap { from, to, reason } => {
            ("History gap".into(), format!("{from}..{to}: {reason:?}"))
        }
    };
    (
        safe(&title).replace('\n', "\\n"),
        body.lines()
            .map(|s| {
                // Literal prefix styling only; it neither parses nor executes tool output.
                let color = match (&card.content, s.as_bytes().first()) {
                    (Content::ToolResult { .. }, Some(b'+')) => Color::Green,
                    (Content::ToolResult { .. }, Some(b'-')) => Color::Red,
                    _ => Color::Reset,
                };
                Line::styled(s.to_owned(), Style::default().fg(color))
            })
            .collect(),
    )
}
/// Consume the frozen snapshot's fields without another projection or parser provenance guess.
/// The controller seeds thinking/JSON folds and owns `collapsed`; absence means expanded.
/// A paused anchor selects the first surviving card at/after it (last card if beyond the cut).
/// Hard wrapping preserves graphemes; a cluster wider than the whole area displays `�`.
/// Work scales with the supplied bounded cut. Only visible rows are retained for painting.
pub(crate) fn compute(
    cards: &[Card],
    collapsed: &BTreeSet<CardId>,
    anchor: Option<u64>,
    area: Rect,
) -> Transcript {
    let mut rows = vec![];
    let mut starts = vec![];
    let mut buttons = vec![];
    let tools = cards
        .iter()
        .filter_map(|c| match &c.content {
            Content::ToolUse(t) => Some((t.id.as_str(), t.name.as_str())),
            _ => None,
        })
        .collect();
    for card in cards {
        starts.push((card.seq, rows.len()));
        let (title, body) = content(card, &tools);
        rows.extend(wrap(
            vec![Line::from(format!(
                "{title}{}",
                if card.streaming { " [streaming]" } else { "" }
            ))],
            area.width,
        ));
        let folded = collapsed.contains(&card.id);
        let toggle = if folded { "[Expand]" } else { "[Collapse]" };
        let mut x: u16 = 0;
        let mut controls = String::new();
        for (label, action) in [
            (toggle, Action::ToggleFold(card.id)),
            ("[Details]", Action::InspectCard(card.id)),
        ] {
            let Ok(width) = u16::try_from(label.width()) else {
                continue;
            };
            if x.checked_add(width)
                .is_some_and(|right| right <= area.width)
            {
                buttons.push((rows.len(), x, width, action));
                controls.push_str(label);
                controls.push(' ');
                x = x.saturating_add(width.saturating_add(1));
            }
        }
        rows.extend(wrap(
            vec![Line::from(controls.trim_end().to_owned())],
            area.width,
        ));
        if !folded {
            rows.extend(wrap(body, area.width));
        }
    }
    let start = anchor
        .and_then(|a| {
            starts
                .iter()
                .find(|(seq, _)| *seq >= a)
                .or_else(|| starts.last())
                .map(|(_, row)| *row)
        })
        .unwrap_or_else(|| rows.len().saturating_sub(usize::from(area.height)));
    let end = rows.len().min(start + usize::from(area.height));
    let hits = buttons
        .into_iter()
        .filter(|(row, _, _, _)| *row >= start && *row < end)
        .map(|(row, x, width, action)| {
            (
                Rect::new(
                    area.x.saturating_add(x),
                    area.y.saturating_add((row - start) as u16),
                    width,
                    1,
                ),
                action,
            )
        })
        .collect();
    Transcript {
        area,
        rows: rows.drain(start..end).collect(),
        hits,
    }
}
#[cfg(test)]
mod tests;
