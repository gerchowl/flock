use super::*;
use ratatui::{
    backend::TestBackend,
    style::{Color, Modifier},
    Terminal,
};
fn card(seq: u64, content: Content) -> Card {
    Card {
        id: CardId { seq, ordinal: 0 },
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
    assert!(text(&b).contains("\\u{1b}[2J"));
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
    let folds = BTreeSet::from([1, 2, 3].map(|seq| CardId { seq, ordinal: 0 }));
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
        assert_eq!(seq.seq, i as u64 / 2 + 1);
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

#[test]
fn entity_controls_in_commonmark_and_table_shaped_paragraphs_are_display_safe() {
    for (entity, raw, escaped) in [
        ("&#27;", '\u{1b}', "\\u{1b}"),
        ("&#x9b;", '\u{9b}', "\\u{9b}"),
        ("&#x85;", '\u{85}', "\\u{85}"),
        ("&#x202e;", '\u{202e}', "\\u{202e}"),
    ] {
        for source in [
            format!("# {entity}heading"),
            format!("[{entity}link](https://example.com)"),
            format!("![{entity}image](https://example.com/image)"),
            // Tables are disabled: this source is a CommonMark paragraph.
            format!("| cell |\n| --- |\n| {entity}value |"),
        ] {
            escaped_card(Content::Markdown(source), &raw.to_string(), escaped);
        }
        // CommonMark raw HTML preserves references literally, without entity decoding.
        for source in [
            format!("<div>{entity}html</div>\n"),
            format!("inline <span title=\"{entity}\">html</span>"),
        ] {
            escaped_card(Content::Markdown(source), &raw.to_string(), entity);
        }
    }
}
raw_case!(raw_markdown_controls, Content::Markdown);
raw_case!(raw_text_controls, Content::Text);
raw_case!(raw_request_input_controls, |raw| {
    Content::ToolRequest(serde_json::from_value(serde_json::json!({"tool_name": "Bash", "input": {"command": raw}, "tool_use_id": "use-1", "subtype": "can_use_tool"})).unwrap())
});
raw_case!(raw_result_title_controls, |raw| Content::ToolResult {
    tool_use_id: raw,
    text: "result".into(),
    is_error: false
});
#[test]
fn crlf_markdown_preserves_paragraphs_without_literal_carriage_returns() {
    let b = draw(&compute(
        &[card(
            1,
            Content::Markdown("# Heading\r\n\r\nparagraph\r\nnext".into()),
        )],
        &BTreeSet::new(),
        Some(1),
        Rect::new(0, 0, 60, 10),
    ));
    assert!(text(&b).contains("Heading"));
    assert!(text(&b).contains("paragraph"));
    assert!(!text(&b).contains("\\r"));
    assert!(!text(&b).contains('\r'));
}
#[test]
fn tab_indented_code_and_tab_list_markers_keep_markdown_structure() {
    let b = draw(&compute(
        &[card(
            1,
            Content::Markdown("\tlet value = 1;\n\n-\titem".into()),
        )],
        &BTreeSet::new(),
        Some(1),
        Rect::new(0, 0, 60, 10),
    ));
    assert!(text(&b).contains("let value = 1;"));
    assert!(b.content.iter().any(|c| c.fg == Color::Cyan));
    assert!(text(&b).contains("• item"));
    assert!(!text(&b).contains("\\t"));
}

raw_case!(raw_html_controls, |raw| Content::Markdown(format!(
    "<div>{raw}</div>\n"
)));
raw_case!(raw_inline_html_controls, |raw| Content::Markdown(format!(
    "inline <span title=\"{raw}\">html</span>"
)));

#[test]
fn raw_c0_in_html_displays_escape_without_terminal_control() {
    escaped_card(
        Content::Markdown("<div>\u{1b}[2J</div>\n".into()),
        "\u{1b}",
        "\\u{1b}",
    );
}
#[test]
fn raw_c1_in_inline_html_displays_escape_without_terminal_control() {
    escaped_card(
        Content::Markdown("<span title=\"\u{9b}\">x</span>".into()),
        "\u{9b}",
        "\\u{9b}",
    );
}

#[test]
fn sibling_card_ids_fold_independently_and_results_use_observed_tool_identity() {
    let mut tool = card(
        7,
        Content::ToolUse(crate::structured::view::contracts::ObservedToolUse {
            id: "use-7".into(),
            name: "Bash".into(),
            input: serde_json::from_value(serde_json::json!({"command":"check"})).unwrap(),
            raw: "original".into(),
        }),
    );
    tool.id.ordinal = 1;
    let cards = [
        card(7, Content::Thinking("folded thinking".into())),
        tool,
        card(
            8,
            Content::ToolResult {
                tool_use_id: "use-7".into(),
                text: "- old\n+ new".into(),
                is_error: false,
            },
        ),
    ];
    let view = compute(
        &cards,
        &BTreeSet::from([cards[0].id]),
        Some(7),
        Rect::new(0, 0, 60, 15),
    );
    let b = draw(&view);
    assert!(!text(&b).contains("folded thinking"));
    for expected in ["Tool: Bash (use-7)", "check", "Tool result: Bash (use-7)"] {
        assert!(text(&b).contains(expected));
    }
    assert!(view
        .hits
        .iter()
        .any(|(_, a)| matches!(a, Action::InspectCard(id) if *id == cards[1].id)));
}

#[test]
fn result_without_retained_invocation_uses_original_tool_id() {
    let cards = [card(
        3,
        Content::ToolResult {
            tool_use_id: "original-use".into(),
            text: "ok".into(),
            is_error: false,
        },
    )];
    let b = draw(&compute(
        &cards,
        &BTreeSet::new(),
        None,
        Rect::new(0, 0, 60, 3),
    ));
    assert!(text(&b).contains("Tool result: original-use"));
    assert!(!text(&b).contains("unknown tool"));
}
#[test]
fn result_correlates_invocation_beyond_visible_rows() {
    let cards = [
        card(
            1,
            Content::ToolUse(
                serde_json::from_value(serde_json::json!({"id":"use-1","name":"Bash","input":{}}))
                    .unwrap(),
            ),
        ),
        card(
            3,
            Content::ToolResult {
                tool_use_id: "use-1".into(),
                text: "ok".into(),
                is_error: false,
            },
        ),
    ];
    let b = draw(&compute(
        &cards,
        &BTreeSet::new(),
        Some(3),
        Rect::new(0, 0, 60, 3),
    ));
    assert!(text(&b).contains("Tool result: Bash (use-1)"));
    assert!(!text(&b).contains("Tool: Bash"));
}

#[test]
fn diff_output_keeps_escaped_text_and_adds_directional_style() {
    let cards = [card(
        1,
        Content::ToolResult {
            tool_use_id: "use-1".into(),
            text: "- old\n+ new\u{1b}".into(),
            is_error: false,
        },
    )];
    let b = draw(&compute(
        &cards,
        &BTreeSet::new(),
        None,
        Rect::new(0, 0, 60, 5),
    ));
    assert!(text(&b).contains("\\u{1b}"));
    for (symbol, color) in [("-", Color::Red), ("+", Color::Green)] {
        assert!(b
            .content
            .iter()
            .any(|c| c.symbol() == symbol && c.fg == color));
    }
}

#[test]
fn diff_styles_survive_wrapping_without_coloring_other_card_kinds() {
    let cards = [
        card(
            1,
            Content::ToolResult {
                tool_use_id: "use-1".into(),
                text: "+abcdefghijklm\n-old\n?context".into(),
                is_error: false,
            },
        ),
        card(2, Content::Text("+ plain".into())),
    ];
    let b = draw(&compute(
        &cards,
        &BTreeSet::new(),
        Some(1),
        Rect::new(0, 0, 8, 50),
    ));
    for (symbol, color) in [
        ("m", Color::Green),
        ("-", Color::Red),
        ("?", Color::Reset),
        ("+", Color::Reset),
    ] {
        assert!(
            b.content
                .iter()
                .any(|cell| cell.symbol() == symbol && cell.fg == color),
            "missing {symbol} in {color:?}"
        );
    }
}
