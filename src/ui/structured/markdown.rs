//! CommonMark display adapter; reparsing the coherent card handles incomplete deltas.
use pulldown_cmark::{Event, Parser, Tag};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// Escape terminal controls before parsing or displaying untrusted content.
pub(super) fn safe(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c.is_control() && c != '\n' {
                c.escape_debug().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}
/// Styling comes only from parser events; HTML remains literal, never terminal markup.
pub(super) fn lines(text: &str) -> Vec<Line<'static>> {
    let text = safe(text);
    let mut rows = vec![Line::default()];
    let mut styles = vec![Style::default()];
    for event in Parser::new(&text) {
        let style = styles.last().copied().unwrap_or_default();
        match event {
            Event::Start(tag) => {
                let next = match tag {
                    Tag::Heading { .. } | Tag::Strong => style.add_modifier(Modifier::BOLD),
                    Tag::Emphasis => style.add_modifier(Modifier::ITALIC),
                    Tag::CodeBlock(_) => style.fg(Color::Cyan),
                    Tag::Item => {
                        if let Some(row) = rows.last_mut() {
                            row.spans.push(Span::raw("• "));
                        }
                        style
                    }
                    _ => style,
                };
                styles.push(next);
            }
            Event::End(tag) => {
                styles.pop();
                if matches!(
                    tag,
                    pulldown_cmark::TagEnd::Paragraph
                        | pulldown_cmark::TagEnd::Heading(_)
                        | pulldown_cmark::TagEnd::Item
                        | pulldown_cmark::TagEnd::CodeBlock
                ) && rows.last().is_some_and(|r| !r.spans.is_empty())
                {
                    rows.push(Line::default());
                }
            }
            Event::Code(t) => {
                if let Some(row) = rows.last_mut() {
                    row.spans
                        .push(Span::styled(t.into_string(), style.fg(Color::Cyan)));
                }
            }
            Event::Text(t) | Event::Html(t) | Event::InlineHtml(t) => {
                for (i, part) in t.split('\n').enumerate() {
                    if i > 0 {
                        rows.push(Line::default());
                    }
                    if let Some(row) = rows.last_mut() {
                        row.spans.push(Span::styled(part.to_owned(), style));
                    }
                }
            }
            Event::SoftBreak | Event::HardBreak => rows.push(Line::default()),
            _ => {}
        }
    }
    rows
}
