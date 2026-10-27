use super::*;
use ratatui::{
    backend::TestBackend,
    style::{Color, Modifier},
    Terminal,
};
fn card(seq: u64, content: Content) -> Card {
    Card {
        seq,
        content,
        streaming: false,
    }
}
fn draw(t: &Transcript) -> ratatui::buffer::Buffer {
    let mut terminal = Terminal::new(TestBackend::new(t.area.right(), t.area.bottom())).unwrap();
    terminal.draw(|f| t.render(f)).unwrap();
    terminal.backend().buffer().clone()
}
fn text(b: &ratatui::buffer::Buffer) -> String {
    b.content.iter().map(|c| c.symbol()).collect()
}
#[test]
fn streaming_commonmark_and_controls_survive_fragment_boundaries() {
    let mut cards = vec![card(1, Content::Markdown(String::new()))];
    for fragment in [
        "# Heading\n\n*empha",
        "sis* and **bold**\n\n- list\n\n```rust\nlet x = 1",
        ";\n```\n\n`inline`\n\n\u{1b}[2J\r\t",
    ] {
        let Content::Markdown(md) = &mut cards[0].content else {
            unreachable!()
        };
        md.push_str(fragment);
        cards[0].streaming = true;
        let view = compute(&cards, &BTreeSet::new(), Some(1), Rect::new(0, 0, 60, 20));
        let b = draw(&view);
        assert!(text(&b).contains("Heading"));
        assert!(b
            .content
            .iter()
            .any(|c| c.modifier.contains(Modifier::BOLD)));
        if fragment.contains("let x") {
            assert!(text(&b).contains("let x = 1"));
        }
    }
    let b = draw(&compute(
        &cards,
        &BTreeSet::new(),
        Some(1),
        Rect::new(0, 0, 60, 20),
    ));
    assert!(text(&b).contains("• list"));
    assert!(b
        .content
        .iter()
        .any(|c| c.modifier.contains(Modifier::ITALIC)));
    assert!(b.content.iter().any(|c| c.fg == Color::Cyan));
    assert!(!text(&b).contains('\u{1b}'));
    assert!(text(&b).contains("\\u{1b}[2J\\r\\t"));
}
#[test]
fn folds_inspection_and_paused_anchor_share_the_drawn_geometry() {
    let mut cards = vec![
        card(1, Content::Thinking("secret thinking".into())),
        card(
            2,
            Content::RawJson {
                known: None,
                shape: None,
                raw: "{\"extra\":1}".into(),
            },
        ),
        card(
            3,
            Content::ToolResult {
                tool_use_id: "use-1".into(),
                text: "- old\n+ new".into(),
                is_error: false,
            },
        ),
    ];
    let folds = BTreeSet::from([1, 2, 3]);
    let area = Rect::new(2, 3, 58, 6);
    let before = compute(&cards, &folds, Some(1), area);
    let picture = draw(&before);
    assert!(!text(&picture).contains("secret thinking"));
    assert!(!text(&picture).contains("extra\""));
    assert!(!text(&picture).contains("+ new"));
    for (i, (rect, _)) in before.hits.iter().enumerate() {
        let action = before.hit(rect.x, rect.y).unwrap();
        let seq = match action {
            Action::ToggleFold(seq) | Action::InspectCard(seq) => seq,
            _ => panic!("unexpected hit"),
        };
        assert_eq!(*seq, i as u64 / 2 + 1);
        let label = if matches!(action, Action::ToggleFold(_)) {
            "[Expand]"
        } else {
            "[Details]"
        };
        assert_eq!(
            (0..rect.width)
                .map(|x| picture[(rect.x + x, rect.y)].symbol())
                .collect::<String>(),
            label
        );
    }
    assert_eq!(before.hits.len(), 6);
    cards.push(card(4, Content::Text("late output".repeat(200))));
    assert_eq!(picture, draw(&compute(&cards, &folds, Some(1), area)));
    assert_eq!(picture, draw(&before));
    let expanded = compute(
        &cards[..3],
        &BTreeSet::new(),
        Some(1),
        Rect::new(0, 0, 60, 20),
    );
    let b = draw(&expanded);
    for value in ["secret thinking", "extra", "- old", "+ new", "use-1"] {
        assert!(text(&b).contains(value));
    }
}
#[test]
fn unicode_tail_and_narrow_clipping_use_physical_rows() {
    let cards = vec![card(1, Content::Text("界e\u{301}".repeat(30)))];
    let t = compute(&cards, &BTreeSet::new(), None, Rect::new(0, 0, 6, 2));
    let b = draw(&t);
    assert_eq!(b[(2, 0)].symbol(), "e\u{301}");
    assert!(t.hits.is_empty());
    assert!(t.rows.iter().all(|r| r.line().width() <= 6));
}

fn escaped_card(content: Content, raw: &str, escaped: &str) {
    let b = draw(&compute(
        &[card(1, content)],
        &BTreeSet::new(),
        Some(1),
        Rect::new(0, 0, 100, 20),
    ));
    let rendered = text(&b);
    assert!(rendered.contains(escaped), "missing {escaped:?} in {b:?}");
    for c in raw.chars() {
        assert!(!rendered.contains(c), "raw display control {c:?}");
    }
}
macro_rules! entity_case {
    ($name:ident, $input:expr, $raw:expr, $escaped:expr) => {
        #[test]
        fn $name() {
            escaped_card(Content::Markdown($input.into()), $raw, $escaped);
        }
    };
}
entity_case!(entity_esc, "&#27;[2J", "\u{1b}", "\\u{1b}[2J");
entity_case!(entity_csi, "&#x9b;2J", "\u{9b}", "\\u{9b}2J");
entity_case!(entity_c1, "&#x85;", "\u{85}", "\\u{85}");
entity_case!(entity_rlo, "&#x202e;ABC", "\u{202e}", "\\u{202e}ABC");
fn raw_format_controls(make: impl Fn(String) -> Content) {
    for c in [
        '\u{202a}',
        '\u{202b}',
        '\u{202c}',
        '\u{202d}',
        '\u{202e}',
        '\u{2066}',
        '\u{2067}',
        '\u{2068}',
        '\u{2069}',
        '\u{200e}',
        '\u{200f}',
        '\u{61c}',
        '\u{ad}',
        '\u{200b}',
        '\u{200c}',
        '\u{200d}',
        '\u{2060}',
        '\u{feff}',
        '\u{1343f}',
        '\u{e0001}',
        '\u{e0061}',
        '\u{2028}',
        '\u{2029}',
    ] {
        let raw = c.to_string();
        let escaped = c.escape_unicode().to_string();
        escaped_card(make(raw.clone()), &raw, &escaped);
    }
}
macro_rules! raw_case {
    ($name:ident, $make:expr) => {
        #[test]
        fn $name() {
            raw_format_controls($make);
        }
    };
}
raw_case!(raw_thinking_controls, Content::Thinking);
raw_case!(raw_json_controls, |raw| Content::RawJson {
    known: None,
    shape: None,
    raw
});
raw_case!(raw_tool_controls, |text| Content::ToolResult {
    tool_use_id: "use-1".into(),
    text,
    is_error: false
});
raw_case!(raw_title_controls, |raw| {
    Content::ToolRequest(serde_json::from_value(serde_json::json!({"tool_name": raw, "input": {}, "tool_use_id": "use-1", "subtype": "can_use_tool"})).unwrap())
});
#[test]
fn ordered_and_nested_lists_preserve_numbering_and_indentation() {
    let b = draw(&compute(
        &[card(
            1,
            Content::Markdown("3. parent\n   - child\n     7. nested\n   - next\n4. end".into()),
        )],
        &BTreeSet::new(),
        Some(1),
        Rect::new(0, 0, 60, 20),
    ));
    for expected in [
        "3. parent",
        "  • child",
        "    7. nested",
        "  • next",
        "4. end",
    ] {
        assert!(text(&b).contains(expected), "missing {expected:?} in {b:?}");
    }
}
