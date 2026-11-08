use super::*;
use crate::{input::TerminalKey, raw_input::RawInputEvent};
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

async fn fixture() -> (AdmissionFixture, String, u64) {
    let mut f = AdmissionFixture::new("init-socket-null", 5000);
    let path = f.fixture.root.path().join("claude");
    let script = fs::read_to_string(&path).unwrap();
    f.fixture.file(
        "claude",
        format!(
            "{}\n{}",
            script.lines().next().unwrap(),
            include_str!("../../../../../tests/fixtures/structured-public/claude.py")
        )
        .as_bytes(),
        0o700,
    );
    let (id, generation) = f.start().await;
    let wake = f.app.render_notify.clone();
    crate::structured::workers::tests::wait(&wake, || {
        f.app.service_structured_workers(Instant::now());
        assert!(
            f.app.structured_workers.report(&id, generation).is_none(),
            "fake runtime failed before editor readiness"
        );
        f.app.state.native_editors.contains_key(&id)
            && f.app
                .state
                .native_frames
                .get(&id)
                .is_some_and(|frame| frame.is_ok())
    })
    .await;
    f.app.state.mode = crate::app::Mode::Terminal;
    f.app.state.mouse_capture = true;
    (f, id, generation)
}
async fn key(f: &mut AdmissionFixture, code: KeyCode, kind: KeyEventKind) {
    f.app
        .handle_raw_input_event(RawInputEvent::Key(
            TerminalKey::new(code, KeyModifiers::empty()).with_kind(kind),
        ))
        .await;
}
async fn text(f: &mut AdmissionFixture, value: &str) {
    for c in value.chars() {
        key(f, KeyCode::Char(c), KeyEventKind::Press).await;
    }
}
async fn painted(f: &mut AdmissionFixture, id: &str) {
    let wake = f.app.render_notify.clone();
    crate::structured::workers::tests::wait(&wake, || {
        f.app.service_structured_workers(Instant::now());
        f.app.state.native_frames[id].as_ref().is_ok_and(|s| {
            s.composer.text_revision == f.app.state.native_editors[id].cut.composer.text_revision
                && s.composer.cursor == f.app.state.native_editors[id].cut.composer.cursor
        })
    })
    .await;
    crate::ui::compute_view_with_runtime_registry(
        &mut f.app.state,
        &f.app.terminal_runtimes,
        ratatui::layout::Rect::new(0, 0, 100, 40),
    );
}
async fn committed(f: &mut AdmissionFixture, id: &str) {
    let wake = f.app.render_notify.clone();
    crate::structured::workers::tests::wait(&wake, || {
        f.app.service_structured_workers(Instant::now());
        !f.app.native_submissions.contains_key(id)
            && f.app.state.native_frames[id]
                .as_ref()
                .is_ok_and(|s| !s.queue.is_empty())
    })
    .await;
    painted(f, id).await;
}
fn draft<'a>(
    f: &'a AdmissionFixture,
    id: &str,
) -> &'a crate::structured::view::contracts::Composer {
    &f.app.state.native_editors[id].cut.composer
}
#[tokio::test]
async fn d3a2_unicode_submit_without_paint_is_exactly_once() {
    let _guard = crate::config::test_config_env_guard();
    let (mut f, id, _) = fixture().await;
    let body = "界e\u{301}👩\u{200d}👩🇨🇭";
    text(&mut f, body).await;
    assert_eq!(draft(&f, &id).text, body);
    key(&mut f, KeyCode::Left, KeyEventKind::Press).await;
    key(&mut f, KeyCode::Right, KeyEventKind::Press).await;
    key(&mut f, KeyCode::Enter, KeyEventKind::Press).await;
    committed(&mut f, &id).await;
    let frame = f.app.state.native_frames[&id].as_ref().unwrap();
    assert_eq!(frame.queue.len(), 1);
    assert_eq!(frame.queue[0].body, body);
    assert!(draft(&f, &id).text.is_empty());
    assert!(f.app.state.terminals.is_empty());
}
#[tokio::test]
async fn d3a2_enter_hold_ack_left_release_clears() {
    let _guard = crate::config::test_config_env_guard();
    let (mut f, id, generation) = fixture().await;
    text(&mut f, "hello").await;
    assert_eq!(draft(&f, &id).text, "hello");
    key(&mut f, KeyCode::Enter, KeyEventKind::Press).await;
    let (ack, release) = hold_native_ack(&mut f, &id, generation).await;
    key(&mut f, KeyCode::Left, KeyEventKind::Press).await;
    assert_eq!(draft(&f, &id).cursor, 4);
    release.send(ack).unwrap();
    committed(&mut f, &id).await;
    assert!(draft(&f, &id).text.is_empty());
    assert_eq!(draft(&f, &id).cursor, 0);
    key(&mut f, KeyCode::Enter, KeyEventKind::Press).await;
    assert_eq!(
        f.app.state.toast.as_ref().unwrap().context,
        "invalid_params"
    );
    assert_eq!(
        f.app.state.native_frames[&id].as_ref().unwrap().queue.len(),
        1
    );
}
#[tokio::test]
async fn d3a2_raw_aba_preserves_new_revision_after_ack() {
    let _guard = crate::config::test_config_env_guard();
    let (mut f, id, generation) = fixture().await;
    text(&mut f, "hello").await;
    assert_eq!(draft(&f, &id).text, "hello");
    let old = draft(&f, &id).text_revision;
    key(&mut f, KeyCode::Enter, KeyEventKind::Press).await;
    let (ack, release) = hold_native_ack(&mut f, &id, generation).await;
    text(&mut f, "!").await;
    key(&mut f, KeyCode::Backspace, KeyEventKind::Press).await;
    let new = draft(&f, &id).text_revision;
    assert!(new > old);
    release.send(ack).unwrap();
    committed(&mut f, &id).await;
    assert_eq!(draft(&f, &id).text, "hello");
    assert_eq!(draft(&f, &id).text_revision, new);
}
#[tokio::test]
async fn d3a2_repeat_enter_never_submits_even_a_fresh_draft() {
    let _guard = crate::config::test_config_env_guard();
    let (mut f, id, generation) = fixture().await;
    seed_native_draft(&mut f, &id, generation, "hello", 5);
    key(&mut f, KeyCode::Enter, KeyEventKind::Repeat).await;
    assert!(
        f.app.native_submissions.is_empty(),
        "Repeat must be swallowed before admission"
    );
    painted(&mut f, &id).await;
    let pane = f.app.state.workspaces[0].focused_pane_id().unwrap();
    let p = &f.app.state.view.structured[&pane];
    let send = p
        .pane
        .as_ref()
        .unwrap()
        .positions()
        .iter()
        .position(|&(x, y)| {
            matches!(
                p.pane.as_ref().unwrap().hit(x, y),
                Some(crate::structured::view::contracts::Action::Send(_))
            )
        })
        .unwrap();
    f.app.state.native_control = Some((p.target.clone(), send));
    key(&mut f, KeyCode::Enter, KeyEventKind::Repeat).await;
    assert!(
        f.app.native_submissions.is_empty(),
        "Repeat must not activate selected Send"
    );
    f.app.state.native_control = None;
    key(&mut f, KeyCode::Enter, KeyEventKind::Press).await;
    committed(&mut f, &id).await;
    text(&mut f, "next").await;
    key(&mut f, KeyCode::Enter, KeyEventKind::Repeat).await;
    key(&mut f, KeyCode::Enter, KeyEventKind::Release).await;
    assert!(f.app.native_submissions.is_empty());
    assert_eq!(draft(&f, &id).text, "next");
    assert_eq!(
        f.app.state.native_frames[&id].as_ref().unwrap().queue.len(),
        1
    );
}
#[tokio::test]
async fn d3a2_native_paste_refuses_without_mutation() {
    let _guard = crate::config::test_config_env_guard();
    let (mut f, id, generation) = fixture().await;
    seed_native_draft(&mut f, &id, generation, "hello", 5);
    let revision = draft(&f, &id).text_revision;
    // A visible float owns paste even when the focused layout pane is native.
    let terminal = crate::terminal::TerminalId::alloc();
    let (runtime, mut bytes) = crate::terminal::TerminalRuntime::test_with_channel(60, 12);
    f.app.terminal_runtimes.insert(terminal.clone(), runtime);
    f.app.state.terminals.insert(
        terminal.clone(),
        crate::terminal::TerminalState::new(
            terminal.clone(),
            f.fixture.root.path().join("project"),
        ),
    );
    f.app.state.register_float(
        f.app.state.workspaces[0].id.clone(),
        crate::app::float::FloatPane {
            pane_id: crate::layout::PaneId::from_raw(777_777),
            terminal_id: terminal,
            visible: true,
        },
    );
    f.app
        .handle_raw_input_event(RawInputEvent::Paste("float-route".into()))
        .await;
    assert_eq!(bytes.try_recv().unwrap().as_ref(), b"float-route");
    assert!(f.app.state.toast.is_none());
    assert_eq!(draft(&f, &id).text_revision, revision);
    f.app.state.hide_active_float();
    f.app
        .handle_raw_input_event(RawInputEvent::Paste("accidental\nturn".into()))
        .await;
    assert_eq!(
        f.app.state.toast.as_ref().map(|t| t.context.as_str()),
        Some("invalid_params")
    );
    assert_eq!(draft(&f, &id).text, "hello");
    assert_eq!(draft(&f, &id).cursor, 5);
    assert_eq!(draft(&f, &id).text_revision, revision);
    assert!(f.app.native_submissions.is_empty());
    painted(&mut f, &id).await;
    assert!(f.app.state.native_frames[&id]
        .as_ref()
        .unwrap()
        .queue
        .is_empty());
}
#[tokio::test]
async fn d3a2_old_ack_cannot_settle_reattached_editor() {
    let _guard = crate::config::test_config_env_guard();
    let (mut f, id, generation) = fixture().await;
    text(&mut f, "hello").await;
    assert_eq!(draft(&f, &id).text, "hello");
    let before = draft(&f, &id).text_revision;
    key(&mut f, KeyCode::Enter, KeyEventKind::Press).await;
    let (ack, release) = hold_native_ack(&mut f, &id, generation).await;
    f.app.state.close_pane();
    f.app.sync_structured_views();
    assert!(!f.app.state.native_editors.contains_key(&id));
    let response = f
        .request(Method::AgentFocus(crate::api::schema::AgentTarget {
            target: id.clone(),
        }))
        .await;
    assert!(response["result"].is_object(), "{response}");
    f.app.state.mode = crate::app::Mode::Terminal;
    text(&mut f, "hello").await;
    let revision = draft(&f, &id).text_revision;
    assert!(revision > before);
    assert_eq!(
        f.app.state.native_editors[&id].target.generation,
        generation
    );
    assert!(
        release.send(ack).is_err(),
        "old driver ticket is gone; its ack must not reach the new editor"
    );
    let wake = f.app.render_notify.clone();
    crate::structured::workers::tests::wait(&wake, || {
        f.app.service_structured_workers(Instant::now());
        f.app.state.native_frames[&id]
            .as_ref()
            .is_ok_and(|s| !s.queue.is_empty() && s.composer.text_revision == revision)
    })
    .await;
    assert_eq!(draft(&f, &id).text, "hello");
    assert_eq!(draft(&f, &id).text_revision, revision);
    assert!(f.app.state.native_editors[&id].cut.receipt.is_none());
    assert!(f.app.native_submissions.is_empty());
}
fn hit(
    f: &AdmissionFixture,
    wants: impl Fn(&crate::structured::view::contracts::Action) -> bool,
) -> (u16, u16) {
    let pane = f.app.state.workspaces[0].focused_pane_id().unwrap();
    let p = f.app.state.view.structured[&pane].pane.as_ref().unwrap();
    p.positions()
        .into_iter()
        .find(|&(x, y)| p.hit(x, y).is_some_and(&wants))
        .unwrap()
}
async fn click(f: &mut AdmissionFixture, at: (u16, u16)) {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    f.app
        .handle_raw_input_event(RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: at.0,
            row: at.1,
            modifiers: KeyModifiers::empty(),
        }))
        .await;
}
#[tokio::test]
async fn d3a2_painted_send_accepts_cursor_but_refuses_stale_text() {
    use crate::structured::view::contracts::Action;
    let _guard = crate::config::test_config_env_guard();
    let (mut f, id, _) = fixture().await;
    text(&mut f, "hello").await;
    assert_eq!(draft(&f, &id).text, "hello");
    painted(&mut f, &id).await;
    let send = hit(&f, |a| matches!(a, Action::Send(_)));
    key(&mut f, KeyCode::Left, KeyEventKind::Press).await;
    click(&mut f, send).await;
    assert!(f.app.native_submissions.contains_key(&id));
    committed(&mut f, &id).await;
    text(&mut f, "hello").await;
    painted(&mut f, &id).await;
    let send = hit(&f, |a| matches!(a, Action::Send(_)));
    text(&mut f, "!").await;
    click(&mut f, send).await;
    assert_eq!(
        f.app.state.toast.as_ref().unwrap().context,
        "invalid_params"
    );
    assert!(f.app.native_submissions.is_empty());
    assert_eq!(draft(&f, &id).text, "hello!");
    assert_eq!(
        f.app.state.native_frames[&id].as_ref().unwrap().queue.len(),
        1
    );
}
#[tokio::test]
async fn d3a2_composer_click_clears_control_selection() {
    use crate::structured::view::contracts::Action;
    let _guard = crate::config::test_config_env_guard();
    let (mut f, id, generation) = fixture().await;
    seed_native_draft(&mut f, &id, generation, "hello", 5);
    painted(&mut f, &id).await;
    let at = hit(&f, |a| matches!(a, Action::EditComposer(_)));
    let target = f.app.state.native_editors[&id].target.clone();
    f.app.state.native_control = Some((target, 0));
    let revision = draft(&f, &id).text_revision;
    click(&mut f, at).await;
    assert!(f.app.state.native_control.is_none());
    assert_eq!(draft(&f, &id).text_revision, revision);
    assert_eq!(draft(&f, &id).text, "hello");
    assert!(f.app.native_submissions.is_empty());
}
