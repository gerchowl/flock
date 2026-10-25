//! Actual PTY writes, actor reads and parsing precede every content assertion.
use super::*;
use crate::{
    app::state::ToastKind,
    layout::{PaneId, PaneInfo},
    terminal::TerminalRuntime,
    workspace::Workspace,
};
use bytes::Bytes;
use crossterm::event::{KeyModifiers, MouseButton};
use ratatui::layout::Rect;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::{mpsc, Notify};

// At 40 columns this model prunes roughly 1151 rows at a time. Keep the
// selection on the newer page, then require metrics to prove older rows were
// discarded. The test fails if the fixture no longer crosses that boundary.
const SCRIPT: &str = r#"
unset FLOCK_SOCKET_PATH FLOCK_PANE_ID FLOCK_AGENT_ID
stty -echo
printf 'READY\r\n'
n=0
while IFS= read -r command; do
    case "$command" in
        seed)
            i=0
            while [ "$i" -lt 2200 ]; do printf 'prefix_%032d\r\n' "$i"; i=$((i+1)); done
            printf 'SELECTA\r\nSELECTB\r\nDONE_seed\r\n'
            ;;
        chunk)
            i=0
            while [ "$i" -lt 20 ]; do printf 'below_%032d\r\n' "$i"; i=$((i+1)); done
            n=$((n+1))
            printf 'DONE_chunk_%s\r\n' "$n"
            ;;
        erase3)
            printf '\033[3J'
            printf 'DONE_erase3\r\n'
            ;;
        erase3semi)
            printf '\033[3;J'
            printf 'DONE_erase3semi\r\n'
            ;;
        erase2semi)
            printf '\033[2;J'
            printf 'DONE_erase2semi\r\n'
            ;;
        cycle47|cycle1047|cycle1049)
            mode=${command#cycle}
            printf '\033[?%sh\033[H' "$mode"
            i=0
            while [ "$i" -lt 20 ]; do printf 'ALT_%s\r\n' "$i"; i=$((i+1)); done
            printf '\033[3J\033[?%slDONE_%s\r\n' "$mode" "$command"
            ;;
        enter47|enter1047|enter1049)
            mode=${command#enter}
            printf '\033[?%sh\033[HSELECTA\r\nSELECTB\r\nDONE_%s\r\n' "$mode" "$command"
            ;;
        alt_history)
            printf '\033[22J\033[6;1HDONE_alt_history'
            ;;
        reset_alt)
            printf '\033c\033[?47hOTHER_A\r\nOTHER_B\r\nDONE_reset_alt\r\n'
            ;;
        reset_restore_alt)
            printf '\033c\033[?1049h\033[?1049s\033[?47l\033[?1049rSELECTA\r\nSELECTB\r\nDONE_reset_alt49\r\n'
            ;;
        alternate)
            printf '\033[?47h\033[HSELECTA\r\nSELECTB\r\nDONE_alternate\r\n'
            ;;
        split_primary)
            printf '\033[?47lDONE_split_primary'
            IFS= read -r release || exit 8
            [ "$release" = finish_primary ] || exit 9
            printf '\r\n'
            ;;
        primary)
            printf '\033[?47l'
            printf 'DONE_primary\r\n'
            ;;
        alternate_return)
            printf '\033[?47h'
            printf 'BACK_ALT\r\n'
            ;;
        alt1047)
            printf '\033[?1047h\033[HSELECTA\r\nSELECTB\r\nDONE_alt1047\r\n'
            ;;
        restore1047)
            printf '\033[?1047r\033[?47h\033[HOTHER_A\r\nOTHER_B\r\nDONE_restore7\r\n'
            ;;
        save1049)
            printf '\033[?1049h\033[?1049s\033[HSELECTA\r\nSELECTB\r\nDONE_saved49\r\n'
            ;;
        restore1049)
            printf '\033[?1049r\033[HOTHER_A\r\nOTHER_B\r\nDONE_restore9\r\n'
            ;;
        reset_restore)
            printf '\033c\033[?1049rSELECTA\r\nSELECTB\r\nDONE_reset\r\n'
            ;;
        top_select)
            printf '\033[2J\033[HSELECTA\r\nSELECTB\r\nDONE_top\r\n'
            ;;
        deccolmh|deccolml|deccolmr)
            final=${command#deccolm}
            printf '\033[?40h\033[?3%s\033[HOTHER_A\r\nOTHER_B\r\nDONE_%s\r\n' "$final" "$command"
            ;;
        decaln)
            printf '\033#8\033[6;1HDONE_decaln'
            ;;
        erase2)
            printf '\033[2J'
            printf 'DONE_erase2\r\n'
            ;;
        evict)
            i=0
            while [ "$i" -lt 5000 ]; do printf 'replacement_%s\r\n' "$i"; i=$((i+1)); done
            printf 'DONE_evict\r\n'
            ;;
        *) exit 7 ;;
    esac
done
"#;

struct Fixture {
    state: AppState,
    pane: PaneId,
    runtimes: TerminalRuntimeRegistry,
    notify: Arc<Notify>,
    dirty: Arc<AtomicBool>,
    _events: mpsc::Receiver<crate::events::AppEvent>,
    _root: crate::structured::launch::SessionConfigDir,
}
impl Fixture {
    async fn new() -> Self {
        Self::with_scrollback(256 * 1024).await
    }
    async fn with_scrollback(capacity: usize) -> Self {
        let root =
            crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let mut state = AppState::test_new();
        let mut ws = Workspace::test_new("selection-content");
        let pane = ws.tabs[0].root_pane;
        let (events, receiver) = mpsc::channel(128);
        let notify = Arc::new(Notify::new());
        let dirty = Arc::new(AtomicBool::new(false));
        let rt = TerminalRuntime::spawn_argv_command(
            pane,
            6,
            40,
            root.path().to_owned(),
            &["/bin/sh".into(), "-c".into(), SCRIPT.into()],
            capacity,
            Default::default(),
            events,
            notify.clone(),
            dirty.clone(),
        )
        .unwrap();
        ws.test_runtimes.insert(pane, rt);
        state.workspaces.push(ws);
        state.active = Some(0);
        state.mode = crate::app::Mode::Terminal;
        state.view.pane_infos.push(PaneInfo {
            id: pane,
            rect: Rect::new(0, 0, 40, 6),
            inner_rect: Rect::new(0, 0, 40, 6),
            scrollbar_rect: None,
            header_rect: None,
            is_focused: true,
        });
        let mut fixture = Self {
            state,
            pane,
            runtimes: Default::default(),
            notify,
            dirty,
            _events: receiver,
            _root: root,
        };
        fixture.wait_for("READY").await;
        fixture.write_and_wait("seed", "DONE_seed").await;
        fixture
    }
    fn runtime(&self) -> &TerminalRuntime {
        self.state.workspaces[0]
            .test_runtimes
            .get(&self.pane)
            .unwrap()
    }
    async fn wait_for(&mut self, marker: &str) {
        self.wait_for_output(marker, true).await;
    }
    async fn wait_for_output(&mut self, marker: &str, line_terminated: bool) {
        // ADR-0040 §1 captures selection coordinates under a separate parser lock.
        // A marker may be visible before its CR/LF scrolls the screen. A line
        // command is complete only with the cursor at the start of the next row.
        // The two unterminated script commands stop at their final marker byte.
        let marker = marker.trim_end_matches('\n');
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            self.dirty.store(false, Ordering::Release);
            let text = self.runtime().visible_text();
            assert!(
                tokio::time::Instant::now() < deadline,
                "producer did not complete {marker}: {text:?}"
            );
            let complete = text.contains(marker)
                && (!line_terminated
                    || self
                        .runtime()
                        .cursor_state(Rect::new(0, 0, u16::MAX, u16::MAX), true)
                        .is_some_and(|cursor| {
                            cursor.x == 0
                                && text.lines().enumerate().any(|(row, line)| {
                                    usize::from(cursor.y) == row + 1 && line.contains(marker)
                                })
                        }));
            if complete {
                return;
            }
            assert!(
                !self.runtime().child_exit().unwrap().is_reaped(),
                "producer exited before {marker}: {text:?}"
            );
            tokio::time::timeout_at(deadline, notified)
                .await
                .unwrap_or_else(|_| panic!("producer did not complete {marker}: {text:?}"));
        }
    }
    async fn write_and_wait(&mut self, command: &str, marker: &str) {
        self.runtime()
            .try_send_bytes(Bytes::from(format!("{command}\n")))
            .unwrap();
        self.wait_for_output(marker, !matches!(command, "alt_history" | "decaln"))
            .await;
    }
    fn mouse(&mut self, kind: MouseEventKind, col: u16, row: u16) {
        self.state.handle_mouse(
            &mut self.runtimes,
            MouseEvent {
                kind,
                column: col,
                row,
                modifiers: KeyModifiers::empty(),
            },
        );
    }
    fn select(&mut self) {
        let text = self.runtime().visible_text();
        let row = text
            .lines()
            .position(|s| s.trim() == "SELECTA")
            .expect("seed text visible") as u16;
        self.mouse(MouseEventKind::Down(MouseButton::Left), 0, row);
        self.mouse(MouseEventKind::Drag(MouseButton::Left), 6, row + 1);
        assert!(self.state.selection.as_ref().unwrap().is_dragging());
    }
    fn copy(&mut self) {
        self.mouse(MouseEventKind::Up(MouseButton::Left), 6, 2);
    }
    fn assert_selected_history(&self) {
        // PageList.scrollbar hides history when max_scrollback=0. Prove the
        // retained rows directly: the viewport no longer shows them, but the
        // live selection still extracts their exact text from screen coordinates.
        assert!(!self.runtime().visible_text().contains("SELECTA"));
        assert_eq!(
            self.runtime()
                .extract_selection(self.state.selection.as_ref().unwrap())
                .as_deref(),
            Some("SELECTA\nSELECTB")
        );
    }
    fn assert_copy(&mut self) {
        self.copy();
        assert_eq!(
            self.state.request_clipboard_write.as_deref(),
            Some(b"SELECTA\nSELECTB".as_slice())
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(runtime) = self.state.workspaces[0].test_runtimes.remove(&self.pane) {
            runtime.shutdown();
        }
    }
}

#[tokio::test]
async fn real_read_selection_survives_output_and_pruning_above_it() {
    let mut fixture = Fixture::new().await;
    fixture.select();
    let mut previous = fixture
        .runtime()
        .scroll_metrics()
        .unwrap()
        .max_offset_from_bottom;
    let mut pruned = false;
    for i in 1..=30 {
        fixture
            .write_and_wait("chunk", &format!("DONE_chunk_{i}\n"))
            .await;
        let current = fixture
            .runtime()
            .scroll_metrics()
            .unwrap()
            .max_offset_from_bottom;
        if current < previous + 21 {
            pruned = true;
            break;
        }
        previous = current;
    }
    assert!(pruned, "fixture must prune prefix rows above the selection");
    assert!(
        fixture.state.selection.is_some(),
        "output must not clear the selection"
    );
    fixture.assert_copy();
}

#[tokio::test]
async fn real_read_evicted_selection_refuses_copy_visibly() {
    let mut fixture = Fixture::new().await;
    fixture.select();
    fixture.write_and_wait("evict", "DONE_evict").await;
    fixture.copy();
    assert!(fixture.state.request_clipboard_write.is_none());
    let toast = fixture.state.toast.as_ref().expect("visible refusal");
    assert_eq!(toast.kind, ToastKind::NeedsAttention);
    assert_eq!(toast.title, "Cannot copy selection");
    assert!(toast.context.contains("evicted"));
}

#[tokio::test]
async fn real_read_selection_survives_resize() {
    let mut fixture = Fixture::new().await;
    fixture.select();
    fixture.runtime().resize(8, 6, 0, 0);
    fixture.assert_copy();
}

async fn assert_erased_selection_refuses(command: &str) {
    let mut fixture = Fixture::new().await;
    fixture.select();
    if command.starts_with("erase3") {
        // Move the anchored rows into history before deleting that history.
        fixture.write_and_wait("chunk", "DONE_chunk_1").await;
    }
    fixture.state.request_clipboard_write = Some(b"previous clipboard".to_vec());
    fixture
        .write_and_wait(command, &format!("DONE_{command}"))
        .await;
    fixture.copy();
    assert_eq!(
        fixture.state.request_clipboard_write.as_deref(),
        Some(b"previous clipboard".as_slice())
    );
    let toast = fixture.state.toast.as_ref().expect("visible erase refusal");
    assert_eq!(toast.kind, ToastKind::NeedsAttention);
    assert_eq!(toast.title, "Cannot copy selection");
    assert!(toast.context.contains("erased"));
}

#[tokio::test]
async fn real_read_scrollback_erase_refuses_copy_visibly() {
    assert_erased_selection_refuses("erase3").await;
}

#[tokio::test]
async fn real_read_display_erase_refuses_copy_visibly() {
    assert_erased_selection_refuses("erase2").await;
}

#[tokio::test]
async fn closed_pane_selection_refuses_with_distinct_reason() {
    let mut fixture = Fixture::new().await;
    fixture.select();
    fixture.state.workspaces[0]
        .test_runtimes
        .remove(&fixture.pane)
        .unwrap()
        .shutdown();
    fixture.state.copy_selection(&fixture.runtimes);
    let toast = fixture
        .state
        .toast
        .as_ref()
        .expect("visible closed pane refusal");
    assert_eq!(toast.context, "Selected pane closed");
}

#[tokio::test]
async fn real_read_scrollback_erase_with_trailing_separator_refuses_copy() {
    assert_erased_selection_refuses("erase3semi").await;
}

#[tokio::test]
async fn real_read_display_erase_with_trailing_separator_refuses_copy() {
    assert_erased_selection_refuses("erase2semi").await;
}

#[tokio::test]
async fn real_read_inactive_alternate_resize_refuses_copy() {
    let mut fixture = Fixture::new().await;
    fixture.write_and_wait("alternate", "DONE_alternate").await;
    fixture.select();
    fixture.write_and_wait("primary", "DONE_primary").await;
    fixture.runtime().resize(3, 20, 0, 0);
    fixture.write_and_wait("alternate_return", "BACK_ALT").await;
    fixture.state.request_clipboard_write = Some(b"previous clipboard".to_vec());
    fixture.copy();
    assert_eq!(
        fixture.state.request_clipboard_write.as_deref(),
        Some(b"previous clipboard".as_slice())
    );
    let toast = fixture
        .state
        .toast
        .as_ref()
        .expect("visible inactive alternate resize refusal");
    assert_eq!(toast.kind, ToastKind::NeedsAttention);
    assert_eq!(toast.title, "Cannot copy selection");
}

fn assert_restored_selection_refused(fixture: &mut Fixture) {
    fixture.copy();
    assert_eq!(
        fixture.state.request_clipboard_write.as_deref(),
        Some(b"previous clipboard".as_slice())
    );
    let toast = fixture
        .state
        .toast
        .as_ref()
        .expect("visible XTRESTORE refusal");
    assert_eq!(toast.kind, ToastKind::NeedsAttention);
    assert_eq!(toast.title, "Cannot copy selection");
}

#[tokio::test]
async fn real_read_xtrestore_unsaved_1047_refuses_replacement_rows() {
    let mut fixture = Fixture::new().await;
    fixture.write_and_wait("alt1047", "DONE_alt1047").await;
    fixture.select();
    fixture.state.request_clipboard_write = Some(b"previous clipboard".to_vec());
    fixture.write_and_wait("restore1047", "DONE_restore7").await;
    assert_restored_selection_refused(&mut fixture);
}

#[tokio::test]
async fn real_read_xtrestore_saved_true_1049_refuses_replacement_rows() {
    let mut fixture = Fixture::new().await;
    fixture.write_and_wait("save1049", "DONE_saved49").await;
    fixture.select();
    fixture.state.request_clipboard_write = Some(b"previous clipboard".to_vec());
    fixture.write_and_wait("restore1049", "DONE_restore9").await;
    assert_restored_selection_refused(&mut fixture);
}

#[tokio::test]
async fn real_read_primary_xtrestore_after_reset_survives_resize() {
    let mut fixture = Fixture::new().await;
    fixture.write_and_wait("reset_restore", "DONE_reset").await;
    fixture.select();
    fixture.runtime().resize(8, 6, 0, 0);
    fixture.assert_copy();
}

async fn assert_full_screen_clear_refuses(command: &str, alternate: bool) {
    let mut fixture = Fixture::new().await;
    if alternate {
        fixture.write_and_wait("alternate", "DONE_alternate").await;
    } else {
        fixture.write_and_wait("top_select", "DONE_top").await;
    }
    fixture.select();
    fixture.state.request_clipboard_write = Some(b"previous clipboard".to_vec());
    fixture
        .write_and_wait(command, &format!("DONE_{command}"))
        .await;
    fixture.copy();
    assert_eq!(
        fixture.state.request_clipboard_write.as_deref(),
        Some(b"previous clipboard".as_slice()),
        "{command}, alternate={alternate}"
    );
    let toast = fixture.state.toast.as_ref().expect("visible clear refusal");
    assert_eq!(toast.kind, ToastKind::NeedsAttention);
    assert_eq!(toast.title, "Cannot copy selection");
    assert!(toast.context.contains("erased"));
}

#[tokio::test]
async fn real_read_deccolm_refuses_copy_on_both_screens() {
    for alternate in [false, true] {
        for command in ["deccolmh", "deccolml", "deccolmr"] {
            assert_full_screen_clear_refuses(command, alternate).await;
        }
    }
}

#[tokio::test]
async fn real_read_decaln_refuses_copy_on_both_screens() {
    for alternate in [false, true] {
        assert_full_screen_clear_refuses("decaln", alternate).await;
    }
}

#[tokio::test]
async fn real_read_primary_after_full_screen_app_survives_resize() {
    for mode in [47, 1047, 1049] {
        let mut fixture = Fixture::new().await;
        let command = format!("cycle{mode}");
        fixture
            .write_and_wait(&command, &format!("DONE_{command}"))
            .await;
        fixture.select();
        fixture.runtime().resize(8, 6, 0, 0);
        fixture.assert_copy();
    }
}

#[tokio::test]
async fn real_read_primary_selection_survives_alternate_output_and_erase() {
    for mode in [47, 1047, 1049] {
        let mut fixture = Fixture::new().await;
        fixture.select();
        let command = format!("cycle{mode}");
        fixture
            .write_and_wait(&command, &format!("DONE_{command}"))
            .await;
        fixture.runtime().resize(8, 6, 0, 0);
        fixture.assert_copy();
    }
}

#[tokio::test]
async fn real_read_primary_no_scrollback_resize_refuses_visibly() {
    let mut fixture = Fixture::with_scrollback(0).await;
    fixture.select();
    fixture.state.request_clipboard_write = Some(b"previous clipboard".to_vec());
    fixture.runtime().resize(8, 3, 0, 0);
    assert_restored_selection_refused(&mut fixture);
}

#[tokio::test]
async fn real_read_alternate_resize_and_teardown_refuse_in_all_screen_modes() {
    for mode in [47, 1047, 1049] {
        for action in [
            "active_resize",
            "inactive_resize",
            "inactive_history",
            "reset_alt",
        ] {
            let mut fixture = Fixture::new().await;
            let command = format!("enter{mode}");
            fixture
                .write_and_wait(&command, &format!("DONE_{command}"))
                .await;
            fixture.select();
            fixture.state.request_clipboard_write = Some(b"previous clipboard".to_vec());
            match action {
                "active_resize" => fixture.runtime().resize(8, 6, 0, 0),
                "inactive_resize" | "inactive_history" => {
                    if action == "inactive_history" {
                        fixture
                            .write_and_wait("alt_history", "DONE_alt_history")
                            .await;
                        fixture.assert_selected_history();
                    }
                    fixture.write_and_wait("primary", "DONE_primary").await;
                    fixture.runtime().resize(3, 20, 0, 0);
                    fixture.write_and_wait("alternate_return", "BACK_ALT").await;
                }
                _ => fixture.write_and_wait(action, "DONE_reset_alt").await,
            }
            assert_restored_selection_refused(&mut fixture);
        }
    }
}

#[tokio::test]
async fn real_read_saved_true_restore_after_reset_still_refuses_alternate_resize() {
    let mut fixture = Fixture::new().await;
    fixture
        .write_and_wait("reset_restore_alt", "DONE_reset_alt49")
        .await;
    fixture.select();
    fixture.state.request_clipboard_write = Some(b"previous clipboard".to_vec());
    fixture.runtime().resize(8, 6, 0, 0);
    assert_restored_selection_refused(&mut fixture);
}

#[tokio::test]
async fn real_read_primary_survives_resize_with_inactive_alternate_history() {
    for mode in [47, 1047, 1049] {
        let mut fixture = Fixture::new().await;
        let command = format!("enter{mode}");
        fixture
            .write_and_wait(&command, &format!("DONE_{command}"))
            .await;
        fixture.select();
        fixture
            .write_and_wait("alt_history", "DONE_alt_history")
            .await;
        fixture.assert_selected_history();
        fixture.write_and_wait("primary", "DONE_primary").await;
        fixture.select();
        fixture.runtime().resize(8, 6, 0, 0);
        fixture.assert_copy();
    }
}

#[tokio::test]
async fn completion_wait_requires_the_final_line_feed_before_selection() {
    use std::{future::Future, task::Poll};
    let mut fixture = Fixture::new().await;
    fixture.write_and_wait("enter47", "DONE_enter47").await;
    fixture.select();
    fixture
        .write_and_wait("alt_history", "DONE_alt_history")
        .await;
    fixture.assert_selected_history();
    fixture
        .runtime()
        .try_send_bytes(Bytes::from_static(b"split_primary\n"))
        .unwrap();
    fixture.wait_for_output("DONE_split_primary", false).await;
    let partial = fixture.runtime().visible_text();
    let cursor = fixture
        .runtime()
        .cursor_state(Rect::new(0, 0, u16::MAX, u16::MAX), true);
    {
        let wait = fixture.wait_for("DONE_split_primary");
        tokio::pin!(wait);
        std::future::poll_fn(|cx| {
            assert!(
                wait.as_mut().poll(cx).is_pending(),
                "marker text cannot complete the frame before its final line feed: {partial:?}, {cursor:?}"
            );
            Poll::Ready(())
        })
        .await;
    }
    fixture
        .runtime()
        .try_send_bytes(Bytes::from_static(b"finish_primary\n"))
        .unwrap();
    fixture.wait_for("DONE_split_primary").await;
    fixture.select();
    fixture.runtime().resize(8, 6, 0, 0);
    fixture.assert_copy();
}
