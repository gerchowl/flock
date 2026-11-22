use super::*;
use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};
fn snapshot() -> impl std::ops::DerefMut<Target = Snapshot> {
    let agent = serde_json::from_value(serde_json::json!({"runtime":"structured","agent_id":"fixture-agent","harness":"claude","lifecycle":"offline","status":"unknown","state":crate::api::agent_state::AgentState::unknown(),"session_id":"fixture-session","cwd":".","model":null,"lease_state":"absent"})).unwrap();
    crate::structured::view::model::Model::new(
        crate::structured::view::Target {
            agent_id: "fixture-agent".into(),
            generation: 1,
        },
        agent,
        Default::default(),
        vec![],
    )
    .snapshot
}
fn request(seq: u64, input: &str) -> crate::structured_wire::PermissionRecord {
    serde_json::from_value(serde_json::json!({"type":"flk_permission_request","request_id":"request","request_seq":seq,"state":"open","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"use-1","input":{"command":input}}})).unwrap()
}
fn draw(p: &Panel) -> Buffer {
    let mut t = Terminal::new(TestBackend::new(100, 20)).unwrap();
    t.draw(|f| p.render(f)).unwrap();
    t.backend().buffer().clone()
}
fn text(b: &Buffer) -> String {
    b.content.iter().map(|c| c.symbol()).collect()
}
#[test]
fn exact_handles_long_input_controls_and_paint_are_coherent() {
    let mut s = snapshot();
    s.permissions = vec![
        request(1, "first"),
        request(2, &format!("\u{202e}{}", "long input".repeat(100))),
    ];
    s.selected_permission = Some(PermissionHandle {
        request_id: "request".into(),
        request_seq: 2,
    });
    let original = s.permissions.clone();
    let area = Rect::new(2, 3, 58, 4);
    let p = permission(&s, area);
    let b = draw(&p);
    assert!(text(&b).contains("\\u{202e}"));
    assert!(!text(&b).contains('\u{202e}'));
    for (r, action) in &p.hits {
        assert!(area.contains((r.x, r.y).into()) && r.right() <= area.right());
        let label = match action {
            Action::InspectPermission(h) => {
                assert_eq!(h.request_seq, 2);
                "[Full input]"
            }
            Action::SelectPermission(h) => {
                assert_eq!(h.request_seq, 1);
                "[Next]"
            }
            Action::Decide {
                handle, decision, ..
            } => {
                assert_eq!(handle.request_seq, 2);
                match decision {
                    PermissionDecision::AllowOnce => "[Allow once]",
                    PermissionDecision::AllowAlways => "[Always (session)...]",
                    PermissionDecision::Deny => "[Deny]",
                }
            }
            _ => panic!("unexpected intent"),
        };
        assert_eq!(
            (0..r.width)
                .map(|x| b[(r.x + x, r.y)].symbol())
                .collect::<String>(),
            label
        );
        assert!(std::ptr::eq(p.hit(r.x, r.y).unwrap(), action));
    }
    assert_eq!(p.hits.len(), 5);
    assert_eq!(b[(59, 6)].symbol(), " ");
    assert_eq!(b, draw(&p));
    assert_eq!(s.permissions, original);
}
#[test]
fn closed_selection_never_decides_a_different_open_handle() {
    let mut s = snapshot();
    s.permissions = vec![request(1, "first"), request(2, "second")];
    let mut closed = s.permissions[0].clone();
    closed.state =
        serde_json::from_value(serde_json::json!({"state":"closed","reason":"result"})).unwrap();
    s.permissions.push(closed);
    s.selected_permission = Some(PermissionHandle {
        request_id: "request".into(),
        request_seq: 1,
    });
    let p = permission(&s, Rect::new(0, 0, 58, 4));
    assert!(matches!(p.hit(0,2), Some(Action::SelectPermission(h)) if h.request_seq == 2));
    assert!(!p
        .hits
        .iter()
        .any(|(_, a)| matches!(a, Action::Decide { .. } | Action::InspectPermission(_))));
    assert!(permission(&s, Rect::new(0, 0, 4, 1)).hits.is_empty());
}
#[test]
fn composer_preserves_utf8_draft_and_disables_uncertain_send() {
    let mut s = snapshot();
    s.composer.text = "界e\u{301}\nsecond\u{202e}".into();
    s.composer.cursor = s.composer.text.len();
    let area = Rect::new(2, 3, 58, 4);
    let p = composer(&s, area);
    assert!(
        matches!(p.hit(2,5), Some(Action::Send(d)) if d.text == s.composer.text && d.cursor == s.composer.cursor)
    );
    let b = draw(&p);
    assert_eq!(b[(4, 3)].symbol(), "界");
    assert_eq!(b[(6, 3)].symbol(), "e\u{301}");
    assert!(text(&b).contains("second\\u{202e}"));
    assert_eq!(b, draw(&p));
    s.admission = Admission::Refused(crate::api::error_code::ErrorCode::SessionBusy);
    assert!(
        matches!(composer(&s, area).hit(2,5), Some(Action::Send(d)) if d.text == s.composer.text)
    );
    s.admission = Admission::Pending;
    let p = composer(&s, area);
    assert!(!p.hits.iter().any(|(_, a)| matches!(a, Action::Send(_))));
    assert!(text(&draw(&p)).contains("pending; retain draft"));
    assert_eq!(s.composer.text, "界e\u{301}\nsecond\u{202e}");
}

macro_rules! narrow_consent {
    ($test:ident, $width:expr) => {
        #[test]
        fn $test() {
            let mut s = snapshot();
            s.permissions = vec![request(1, "inspect this request")];
            let area = Rect::new(2, 3, $width, 4);
            let p = permission(&s, area);
            let has = |decision| p.hits.iter().any(|(_, a)| matches!(a, Action::Decide { decision: d, .. } if *d == decision));
            let approval = has(PermissionDecision::AllowOnce) || has(PermissionDecision::AllowAlways);
            assert!(!approval || has(PermissionDecision::Deny), "approval without Deny");
            assert!(matches!(p.hit(2, 6), Some(Action::Decide { decision: PermissionDecision::Deny, .. })));
            assert!(!approval, "partial decision row must offer inspection instead");
            assert!(p.hits.iter().any(|(_, a)| matches!(a, Action::InspectPermission(h) if h.request_seq == 1)));
            assert!(text(&draw(&p)).contains("[Deny]"));
        }
    };
}
narrow_consent!(consent_width_12_keeps_deny_and_inspection, 12);
narrow_consent!(consent_width_18_keeps_deny_and_inspection, 18);
narrow_consent!(consent_width_34_keeps_deny_and_inspection, 34);
narrow_consent!(consent_width_40_keeps_deny_and_inspection, 40);
#[test]
fn button_geometry_measures_display_columns() {
    let mut p = Panel::default();
    p.buttons(Rect::new(2, 3, 3, 1), vec![("界é", Action::Close)]);
    assert_eq!(p.hits.len(), 1);
    assert_eq!(p.hits[0].0.width, 3);
    assert!(matches!(p.hit(4, 3), Some(Action::Close)));
    let b = draw(&p);
    assert_eq!(b[(2, 3)].symbol(), "界");
    assert_eq!(b[(4, 3)].symbol(), "é");
}
#[test]
fn button_coordinates_refuse_overflow_without_wrapping() {
    let mut p = Panel::default();
    let area = Rect {
        x: u16::MAX - 1,
        y: 0,
        width: 4,
        height: 1,
    };
    p.buttons(area, vec![("a", Action::Close), ("b", Action::Stop)]);
    assert_eq!(p.hits.len(), 1);
    assert_eq!(p.hits[0].0.x, area.x);
}
