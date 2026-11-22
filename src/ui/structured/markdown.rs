//! CommonMark display adapter; reparsing the coherent card handles incomplete deltas.
use pulldown_cmark::{Event, Parser, Tag};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

/// Escape Cc except LF, Unicode 17 Cf, and line/paragraph separators.
/// Joiners, soft hyphens, bidi controls and tags deliberately display as escapes.
/// Ordinary combining marks retain their glyph behavior.
/// Cf ranges follow the [Unicode category data](https://www.unicode.org/Public/17.0.0/ucd/extracted/DerivedGeneralCategory.txt).
pub(super) fn safe(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c.is_control() && c != '\n' {
                c.escape_debug().collect::<Vec<_>>()
            } else if format_control(c) || matches!(c, '\u{2028}' | '\u{2029}') {
                c.escape_unicode().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}
fn format_control(c: char) -> bool {
    matches!(c,
        '\u{ad}' | '\u{600}'..='\u{605}' | '\u{61c}' | '\u{6dd}' | '\u{70f}'
        | '\u{890}'..='\u{891}' | '\u{8e2}' | '\u{180e}' | '\u{200b}'..='\u{200f}'
        | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}'
        | '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{110bd}' | '\u{110cd}'
        | '\u{13430}'..='\u{1343f}' | '\u{1bca0}'..='\u{1bca3}'
        | '\u{1d173}'..='\u{1d17a}' | '\u{e0001}' | '\u{e0020}'..='\u{e007f}')
}
/// A prepared row can only be constructed by the output sanitizer in this module.
pub(super) struct DisplayLine(Line<'static>);
impl DisplayLine {
    /// Borrow sanitized glyphs and styles for immutable paint.
    pub fn line(&self) -> &Line<'static> {
        &self.0
    }
}
/// The sole untrusted-text-to-display boundary: sanitize after entity decoding,
/// before measuring graphemes. Every transcript row, including titles and buttons,
/// uses this factory; the private row type prevents bypasses by its consumers.
pub(super) fn wrap(lines: Vec<Line<'static>>, width: u16) -> Vec<DisplayLine> {
    let mut rows = vec![];
    if width == 0 {
        return rows;
    }
    for mut line in lines {
        for span in &mut line.spans {
            span.content = safe(&span.content).into();
        }
        let mut row = Line::default();
        let mut used = 0;
        for g in line.styled_graphemes(Style::default()) {
            let (symbol, w) = if g.symbol.width() > usize::from(width) {
                ("�", 1)
            } else {
                (g.symbol, g.symbol.width())
            };
            if used + w > usize::from(width) {
                rows.push(DisplayLine(row));
                row = Line::default();
                used = 0;
            }
            row.spans.push(Span::styled(symbol.to_owned(), g.style));
            used += w;
        }
        rows.push(DisplayLine(row));
    }
    rows
}
/// Parser events supply styles and raw decoded text; `wrap` sanitizes every output span.
pub(super) fn lines(text: &str) -> Vec<Line<'static>> {
    let mut rows = vec![Line::default()];
    let mut styles = vec![Style::default()];
    let mut lists: Vec<Option<u64>> = vec![];
    for event in Parser::new(text) {
        let style = styles.last().copied().unwrap_or_default();
        match event {
            Event::Start(tag) => {
                let next = match tag {
                    Tag::Heading { .. } | Tag::Strong => style.add_modifier(Modifier::BOLD),
                    Tag::Emphasis => style.add_modifier(Modifier::ITALIC),
                    Tag::CodeBlock(_) => style.fg(Color::Cyan),
                    Tag::List(start) => {
                        if rows.last().is_some_and(|r| !r.spans.is_empty()) {
                            rows.push(Line::default());
                        }
                        lists.push(start);
                        style
                    }
                    Tag::Item => {
                        let indent = "  ".repeat(lists.len().saturating_sub(1));
                        let marker = match lists.last_mut() {
                            Some(Some(n)) => {
                                let marker = format!("{n}. ");
                                *n = n.saturating_add(1);
                                marker
                            }
                            _ => "• ".into(),
                        };
                        if let Some(row) = rows.last_mut() {
                            row.spans.push(Span::raw(format!("{indent}{marker}")));
                        }
                        style
                    }
                    _ => style,
                };
                styles.push(next);
            }
            Event::End(tag) => {
                if matches!(tag, pulldown_cmark::TagEnd::List(_)) {
                    lists.pop();
                }
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
