use super::*;
use crate::{layout::PaneInfo, structured::view::tests::fixture};
use ratatui::layout::{Direction, Rect};

fn native_app() -> (App, PaneInfo) {
    let mut app = super::super::app_for_mouse_test();
    let (mut state, target) = fixture();
    let id = state
        .attach_native_view(target, 0, 0, Direction::Horizontal)
        .unwrap();
    state.mode = Mode::Terminal;
    state.sidebar_collapsed = false;
    state.view.sidebar_rect = Rect::new(0, 0, 26, 20);
    state.view.terminal_area = Rect::new(26, 0, 80, 20);
    state.view.pane_infos = state.workspaces[0].layout.panes(Rect::new(26, 2, 80, 18));
    state.view.split_borders = state.workspaces[0].layout.splits(Rect::new(26, 2, 80, 18));
    let info = state
        .view
        .pane_infos
        .iter()
        .find(|p| p.id == id)
        .unwrap()
        .clone();
    app.state = state;
    (app, info)
}
fn pointer(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: KeyModifiers::empty(),
    }
}
#[test]
fn native_resize_drag_keeps_sidebar_updates_and_release() {
    let (mut app, info) = native_app();
    let down = pointer(MouseEventKind::Down(MouseButton::Left), 25, 8);
    app.handle_mouse(down);
    assert!(matches!(
        app.state.drag.as_ref().map(|d| &d.target),
        Some(crate::app::state::DragTarget::SidebarDivider)
    ));
    let before = app.state.sidebar_width;
    let x = info.inner_rect.x + 2;
    app.handle_mouse(pointer(MouseEventKind::Drag(MouseButton::Left), x, 8));
    app.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), x, 8));
    assert!(
        app.state.drag.is_none(),
        "divider release over native pane must clear drag"
    );
    assert_ne!(
        app.state.sidebar_width, before,
        "divider must follow across native pane"
    );
}

#[test]
fn native_stale_generation_key_refuses_without_detaching() {
    let (mut app, info) = native_app();
    let target = app.native_target(info.id).unwrap();
    app.state
        .structured_agents
        .begin_launch(&target.agent_id, target.generation)
        .unwrap();
    app.handle_terminal_key_headless(TerminalKey::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
    assert_eq!(app.state.toast.as_ref().unwrap().context, "agent_not_found");
    assert!(app.state.workspaces[0].tabs[0]
        .native_views
        .contains_key(&info.id));
}

async fn permission_app() -> (App, crate::structured::workers::tests::Case, Target) {
    use crate::structured::workers::tests::{wait, Case};
    let mut app = super::super::app_for_mouse_test();
    app.state
        .workspaces
        .push(crate::workspace::Workspace::test_new(
            "native-permission-fixture",
        ));
    let event = serde_json::json!({"type":"control_request", "request_id":"native-request", "request":{"subtype":"can_use_tool", "tool_name":"Bash", "tool_use_id":"fixture-tool", "input":{"command":"fixture input"}}});
    let case = Case::start(
        &mut app.state.structured_agents,
        &mut app.structured_workers,
        2683,
        "state-reducer",
        vec![event],
    );
    let wake = app.render_notify.clone();
    case.human(
        &mut app.state.structured_agents,
        &mut app.structured_workers,
        &wake,
        std::time::Instant::now(),
    )
    .await;
    let target = Target {
        agent_id: case.id.clone(),
        generation: case.generation,
    };
    app.state
        .attach_native_view(target.clone(), 0, 0, Direction::Horizontal)
        .unwrap();
    wait(&wake, || {
        app.service_structured_workers(std::time::Instant::now());
        matches!(app.state.native_frames.get(&case.id),Some(Ok(f)) if !f.permissions.is_empty())
    })
    .await;
    crate::ui::compute_view_with_runtime_registry(
        &mut app.state,
        &app.terminal_runtimes,
        Rect::new(0, 0, 140, 40),
    );
    (app, case, target)
}
#[tokio::test]
async fn native_view_decide_refuses_operator_and_keeps_the_exact_request() {
    let _guard = crate::config::test_config_env_guard();
    let (mut app, case, _) = permission_app().await;
    let before = app
        .structured_workers
        .runtime(&case.id, case.generation)
        .unwrap()
        .waiting_on_human()
        .unwrap();
    let pane = app
        .state
        .view
        .structured
        .values()
        .find_map(|p| p.pane.as_ref().ok())
        .unwrap();
    let (x, y) = pane
        .positions()
        .into_iter()
        .find(|&(x, y)| {
            matches!(
                pane.hit(x, y),
                Some(Action::Decide {
                    decision: crate::structured_wire::PermissionDecision::Deny,
                    ..
                })
            )
        })
        .unwrap();
    app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Left), x, y));
    assert_eq!(app.state.toast.as_ref().unwrap().context, "not_authorized");
    assert_eq!(
        app.structured_workers
            .runtime(&case.id, case.generation)
            .unwrap()
            .waiting_on_human()
            .unwrap(),
        before
    );
}
#[tokio::test]
async fn native_selected_control_highlights_the_whole_label() {
    use ratatui::{backend::TestBackend, style::Modifier, Terminal};
    let _guard = crate::config::test_config_env_guard();
    let (mut app, _case, target) = permission_app().await;
    app.state.native_control = Some((target, 0));
    let pane = app
        .state
        .view
        .structured
        .values()
        .find_map(|p| p.pane.as_ref().ok())
        .unwrap();
    let (x, y) = pane.positions()[0];
    let width = (x..140)
        .take_while(|&col| pane.hit(col, y).is_some())
        .count();
    assert!(width > 1);
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
    terminal
        .draw(|frame| {
            crate::ui::render_with_runtime_registry(&app.state, &app.terminal_runtimes, frame)
        })
        .unwrap();
    for col in x..x + u16::try_from(width).unwrap() {
        assert!(
            terminal.backend().buffer()[(col, y)]
                .modifier
                .contains(Modifier::REVERSED),
            "whole control label must be highlighted"
        );
    }
}

#[test]
fn native_resize_drag_keeps_split_updates_and_release() {
    let (mut app, info) = native_app();
    let border = app.state.view.split_borders[0].clone();
    app.handle_mouse(pointer(
        MouseEventKind::Down(MouseButton::Left),
        border.pos,
        8,
    ));
    assert!(matches!(
        app.state.drag.as_ref().map(|d| &d.target),
        Some(crate::app::state::DragTarget::PaneSplit { .. })
    ));
    let x = info.inner_rect.x + 8;
    app.handle_mouse(pointer(MouseEventKind::Drag(MouseButton::Left), x, 8));
    app.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), x, 8));
    assert!(
        app.state.drag.is_none(),
        "split release over native pane must clear drag"
    );
    assert_ne!(
        app.state.workspaces[0].layout.splits(border.area)[0].pos,
        border.pos
    );
}
