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
            256 * 1024,
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
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            self.dirty.store(false, Ordering::Release);
            let text = self.runtime().visible_text();
            assert!(
                tokio::time::Instant::now() < deadline,
                "producer did not emit {marker}: {text:?}"
            );
            if text.contains(marker) {
                return;
            }
            assert!(
                !self.runtime().child_exit().unwrap().is_reaped(),
                "producer exited before {marker}: {text:?}"
            );
            tokio::time::timeout_at(deadline, notified)
                .await
                .unwrap_or_else(|_| panic!("producer did not emit {marker}: {text:?}"));
        }
    }
    async fn write_and_wait(&mut self, command: &str, marker: &str) {
        self.runtime()
            .try_send_bytes(Bytes::from(format!("{command}\n")))
            .unwrap();
        self.wait_for(marker).await;
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
    if command == "erase3" {
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
