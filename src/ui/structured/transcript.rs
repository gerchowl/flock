//! Source-anchored transcript compute/paint; no subscriptions, file reads or authority.
use super::markdown::{lines, safe};
use crate::structured::view::contracts::{Action, Card, Content};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use std::collections::BTreeSet;
use unicode_width::UnicodeWidthStr;

/// Visible physical rows and exact button rectangles, computed together before painting.
pub(crate) struct Transcript {
    area: Rect,
    rows: Vec<Line<'static>>,
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
    /// Paint only the prepared rows. The source snapshot and computed geometry stay immutable.
    pub fn render(&self, frame: &mut Frame) {
        frame.render_widget(Paragraph::new(self.rows.clone()), self.area);
    }
}
fn wrap(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let mut rows = vec![];
    if width == 0 {
        return rows;
    }
    for line in lines {
        let mut row = Line::default();
        let mut used = 0;
        for g in line.styled_graphemes(Style::default()) {
            let (symbol, w) = if g.symbol.width() > usize::from(width) {
                ("�", 1)
            } else {
                (g.symbol, g.symbol.width())
            };
            if used + w > usize::from(width) {
                rows.push(row);
                row = Line::default();
                used = 0;
            }
            row.spans.push(Span::styled(symbol.to_owned(), g.style));
            used += w;
        }
        rows.push(row);
    }
    rows
}
fn content(card: &Card) -> (String, Vec<Line<'static>>) {
    let (title, body) = match &card.content {
        Content::Markdown(t) => return ("Assistant".into(), lines(t)),
        Content::Text(t) => ("Assistant".into(), safe(t)),
        Content::Thinking(t) => ("Thinking".into(), safe(t)),
        Content::RawJson { raw, .. } => ("Unknown / extra JSON".into(), safe(raw)),
        Content::ToolRequest(r) => (
            format!("Tool: {} ({})", r.tool_name, r.tool_use_id),
            safe(&serde_json::Value::Object(r.input.clone()).to_string()),
        ),
        Content::ToolResult {
            tool_use_id,
            text,
            is_error,
        } => (
            format!(
                "Tool result: {tool_use_id}{}",
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
        body.lines().map(|s| Line::from(s.to_owned())).collect(),
    )
}
/// Consume the frozen snapshot's fields without another projection or parser provenance guess.
/// The controller seeds thinking/JSON folds and owns `collapsed`; absence means expanded.
/// A paused anchor selects the first surviving card at/after it (last card if beyond the cut).
/// Hard wrapping preserves graphemes; a cluster wider than the whole area displays `�`.
/// Work scales with the supplied bounded cut. Only visible rows are retained for painting.
pub(crate) fn compute(
    cards: &[Card],
    collapsed: &BTreeSet<u64>,
    anchor: Option<u64>,
    area: Rect,
) -> Transcript {
    let mut rows = vec![];
    let mut starts = vec![];
    let mut buttons = vec![];
    for card in cards {
        starts.push((card.seq, rows.len()));
        let (title, body) = content(card);
        rows.extend(wrap(
            vec![Line::from(format!(
                "{title}{}",
                if card.streaming { " [streaming]" } else { "" }
            ))],
            area.width,
        ));
        let folded = collapsed.contains(&card.seq);
        let toggle = if folded { "[Expand]" } else { "[Collapse]" };
        let mut x = 0;
        let mut controls = String::new();
        for (label, action) in [
            (toggle, Action::ToggleFold(card.seq)),
            ("[Details]", Action::InspectCard(card.seq)),
        ] {
            let width = label.len() as u16;
            if x + width <= area.width {
                buttons.push((rows.len(), x, width, action));
                controls.push_str(label);
                controls.push(' ');
                x += width + 1;
            }
        }
        rows.push(Line::from(controls));
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
                Rect::new(area.x + x, area.y + (row - start) as u16, width, 1),
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
