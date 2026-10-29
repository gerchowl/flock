use super::*;
use crate::structured::view::{contracts::*, model::Model, Target};
use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};
fn card(seq: u64, content: Content) -> Card {
    Card {
        id: CardId { seq, ordinal: 0 },
        seq,
        content,
        streaming: false,
    }
}
fn snapshot() -> Snapshot {
    let agent = serde_json::from_value(serde_json::json!({"runtime":"structured","agent_id":"fixture-agent","name":"demo","harness":"claude","lifecycle":"live","status":"unknown","state":crate::api::agent_state::AgentState::unknown(),"session_id":"fixture-session","cwd":".","model":"requested","lease_state":"absent"})).unwrap();
    let mut s = Model::new(
        Target {
            agent_id: "fixture-agent".into(),
            generation: 1,
        },
        agent,
        Default::default(),
        vec![crate::structured::queue::tests::item("queued")],
    )
    .snapshot;
    s.reported_model = Some("observed".into());
    s.reported_permissions = Some("observed-mode".into());
    s.agent.state.phase = crate::api::agent_state::Phase::NeedsHuman;
    s.agent.state.held_ms = Some(150);
    s.usage = Some(Usage {
        observed_seq: 14,
        input_tokens: Some(7),
        output_tokens: Some(3),
        cache_read_tokens: None,
        cache_write_tokens: None,
    });
    s.applied_cursor = 14;
    s.replay = Replay::Synced { through: 14 };
    s.earlier_history = Some(9);
    s.scroll_anchor = Some(10);
    s.new_output = true;
    s.transcript = vec![
        card(10, Content::Text("Ready".into())),
        card(11, Content::Thinking("folded thinking".into())),
        card(
            12,
            Content::ToolUse(
                serde_json::from_value(
                    serde_json::json!({"id":"tool-12","name":"Bash","input":{"command":"check"}}),
                )
                .unwrap(),
            ),
        ),
        card(
            13,
            Content::ToolResult {
                tool_use_id: "tool-12".into(),
                text: "- old\n+ new".into(),
                is_error: false,
            },
        ),
        card(
            14,
            Content::RawJson {
                known: None,
                shape: None,
                raw: "{\"future\":true}".into(),
            },
        ),
    ];
    s.collapsed = s
        .transcript
        .iter()
        .filter(|c| [11, 12, 14].contains(&c.seq))
        .map(|c| c.id)
        .collect();
    s.permissions = vec![serde_json::from_value(serde_json::json!({"type":"flk_permission_request","request_id":"permission","request_seq":9,"state":"open","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-12","input":{"command":"check ".repeat(100)}}})).unwrap()];
    s.composer.text = "Please check\nthen report.".into();
    s.composer.cursor = s.composer.text.len();
    s
}
fn draw(s: &Snapshot, width: u16, height: u16) -> (Buffer, Pane) {
    let p = Pane::compute(s, Rect::new(0, 0, width, height), Style::default());
    let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
    t.draw(|f| p.render(f)).unwrap();
    let b = t.backend().buffer().clone();
    t.draw(|f| p.render(f)).unwrap();
    assert_eq!(&b, t.backend().buffer(), "repeated render changed pixels");
    (b, p)
}
fn image(b: &Buffer) -> String {
    b.content
        .chunks(usize::from(b.area.width))
        .map(|r| r.iter().map(|c| c.symbol()).collect::<String>() + "\n")
        .collect()
}
#[test]
fn exact_seven_region_layout_snapshots() {
    let s = snapshot();
    for (w, h, expected) in [
        (100, 30, include_str!("100x30.txt")),
        (60, 20, include_str!("60x20.txt")),
    ] {
        assert_eq!(image(&draw(&s, w, h).0), expected, "layout {w}x{h}");
    }
}
#[test]
fn consent_and_composer_hits_match_paint_without_mutating_source() {
    let s = snapshot();
    let permissions = s.permissions.clone();
    for (w, h) in [(100, 30), (60, 20)] {
        let (b, p) = draw(&s, w, h);
        assert!(
            matches!(p.hit(1,h-8),Some(Action::InspectPermission(handle)) if handle.request_id=="permission" && handle.request_seq==9)
        );
        assert!(
            matches!(p.hit(1,h-7),Some(Action::Decide { handle, decision: crate::structured_wire::PermissionDecision::Deny, .. }) if handle.request_seq==9)
        );
        assert!(
            matches!(p.hit(1,h-3),Some(Action::Send(d)) if d.text==s.composer.text && d.cursor==s.composer.cursor)
        );
        for (y, label) in [
            (h - 8, "[Full input]"),
            (h - 7, "[Deny]"),
            (h - 3, "[Send at idle]"),
        ] {
            assert_eq!(
                (1..=label.len() as u16)
                    .map(|x| b[(x, y)].symbol())
                    .collect::<String>(),
                label
            );
        }
        assert_eq!(s.permissions, permissions);
        assert_eq!(s.composer.text, "Please check\nthen report.");
        assert_eq!(s.applied_cursor, 14);
    }
}
#[test]
fn paused_transcript_does_not_move_when_more_output_arrives() {
    let mut s = snapshot();
    let before = draw(&s, 100, 30).0;
    s.transcript
        .push(card(15, Content::Text("late output".repeat(100))));
    s.applied_cursor = 15;
    let after = draw(&s, 100, 30).0;
    for y in 5..18 {
        for x in 1..99 {
            assert_eq!(before[(x, y)], after[(x, y)]);
        }
    }
    assert!(image(&after).contains("New output"));
}
#[test]
fn metadata_controls_are_escaped_at_the_same_display_boundary() {
    let mut s = snapshot();
    s.agent.name = Some("name\u{1b}".into());
    s.reported_model = Some("model\u{202e}".into());
    s.reported_permissions = Some("mode\u{9b}".into());
    let rendered = image(&draw(&s, 100, 30).0);
    for (raw, escaped) in [
        ("\u{1b}", "\\u{1b}"),
        ("\u{202e}", "\\u{202e}"),
        ("\u{9b}", "\\u{9b}"),
    ] {
        assert!(!rendered.contains(raw));
        assert!(rendered.contains(escaped));
    }
}
