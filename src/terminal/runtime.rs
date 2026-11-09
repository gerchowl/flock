use std::sync::{atomic::AtomicBool, Arc};

use bytes::Bytes;
use ratatui::{layout::Rect, Frame};
use tokio::sync::{mpsc, Notify};

use crate::events::AppEvent;
use crate::layout::PaneId;

/// Live runtime for a server-owned terminal.
///
/// The PTY implementation still delegates to the legacy pane runtime while the
/// migration proceeds, but production code now depends on this terminal-layer
/// type instead of the pane module's implementation detail.
pub struct TerminalRuntime(crate::pane::PaneRuntime);

impl TerminalRuntime {
    pub fn shutdown(self) {
        self.0.shutdown();
    }

    pub(crate) fn shutdown_for_restart(
        self,
        pids: Vec<u32>,
        grace: std::time::Duration,
    ) -> tokio::sync::oneshot::Receiver<bool> {
        self.0.shutdown_for_restart(pids, grace)
    }

    pub(crate) fn was_spawned_with_default_shell(&self) -> bool {
        self.0.was_spawned_with_default_shell()
    }

    /// Whether the pane is a plain shell with nothing in front of it, `None`
    /// when it cannot tell (ADR-0028 §2, #1176).
    pub(crate) fn foreground_is_own_shell(&self) -> Option<bool> {
        self.0.foreground_is_own_shell()
    }

    /// Whether any input has reached this terminal, or its history is unknown.
    pub(crate) fn has_received_input(&self) -> bool {
        self.0.has_received_input()
    }

    /// PID of the pane's direct child process, when alive.
    pub fn child_pid(&self) -> Option<u32> {
        self.0.child_pid()
    }

    /// Birth captured at spawn or imported with the owning pane runtime.
    pub(crate) fn child_birth(&self) -> Option<u64> {
        self.0.child_birth()
    }
    /// The pane child's exit slot, where one exists (#178).
    pub fn child_exit(&self) -> Option<&crate::pane::ChildExit> {
        self.0.child_exit()
    }

    #[cfg(unix)]
    pub fn duplicate_handoff_fd(&self) -> std::io::Result<std::os::fd::RawFd> {
        self.0.duplicate_handoff_fd()
    }

    #[cfg(unix)]
    pub fn preserve_for_handoff(self) {
        self.0.preserve_for_handoff()
    }

    #[cfg(unix)]
    pub fn assume_handoff_ownership(&mut self) {
        self.0.assume_handoff_ownership();
    }

    #[cfg(unix)]
    pub fn set_handoff_reader_paused(&self, paused: bool) {
        self.0.set_handoff_reader_paused(paused);
    }

    #[cfg(unix)]
    pub fn start_handoff_pause(
        &self,
        timeout: std::time::Duration,
        drop_undrained: bool,
    ) -> std::io::Result<crate::pane::PendingPaneHandoff> {
        self.0.start_handoff_pause(timeout, drop_undrained)
    }

    #[cfg(unix)]
    pub fn handoff_runtime_state(
        &self,
        pane_id: u32,
    ) -> crate::handoff_runtime::HandoffRuntimeState {
        self.0.handoff_runtime_state(pane_id)
    }

    #[cfg(unix)]
    pub fn handoff_history_ansi(&self) -> Option<String> {
        self.0.handoff_history_ansi()
    }

    #[cfg(unix)]
    pub fn from_handoff_fd(
        import: crate::handoff_runtime::ImportedHandoffRuntime,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::from_handoff_fd(
            import,
            scrollback_limit_bytes,
            host_terminal_theme,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self)
    }

    pub fn spawn(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::spawn(
            pane_id,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self)
    }

    pub fn spawn_with_initial_history(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        initial_history_ansi: Option<&str>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::spawn_with_initial_history(
            pane_id,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            initial_history_ansi,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self)
    }

    pub fn spawn_shell_command(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        command: &str,
        extra_env: &[(String, String)],
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::spawn_shell_command(
            pane_id,
            rows,
            cols,
            cwd,
            command,
            extra_env,
            scrollback_limit_bytes,
            host_terminal_theme,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self)
    }

    pub fn spawn_argv_command(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        argv: &[String],
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::spawn_argv_command(
            pane_id,
            rows,
            cols,
            cwd,
            argv,
            scrollback_limit_bytes,
            host_terminal_theme,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self)
    }

    pub fn apply_host_terminal_theme(&self, theme: crate::terminal_theme::TerminalTheme) {
        self.0.apply_host_terminal_theme(theme);
    }

    /// Host terminal theme last written into this pane's emulator.
    #[cfg(test)]
    pub fn test_host_terminal_theme(&self) -> crate::terminal_theme::TerminalTheme {
        self.0.test_host_terminal_theme()
    }

    pub fn begin_graceful_release(&self, agent: crate::detect::Agent) {
        self.0.begin_graceful_release(agent);
    }

    pub fn resize(&self, rows: u16, cols: u16, cell_width_px: u32, cell_height_px: u32) {
        self.0.resize(rows, cols, cell_width_px, cell_height_px);
    }

    pub fn nudge_child_redraw_after_handoff(&self) {
        self.0.nudge_child_redraw_after_handoff();
    }

    pub fn scroll_up(&self, lines: usize) {
        self.0.scroll_up(lines);
    }

    pub fn scroll_down(&self, lines: usize) {
        self.0.scroll_down(lines);
    }

    pub fn scroll_reset(&self) {
        self.0.scroll_reset();
    }

    pub fn set_scroll_offset_from_bottom(&self, lines: usize) {
        self.0.set_scroll_offset_from_bottom(lines);
    }

    pub fn scroll_metrics(&self) -> Option<crate::pane::ScrollMetrics> {
        self.0.scroll_metrics()
    }

    pub fn input_state(&self) -> Option<crate::pane::InputState> {
        self.0.input_state()
    }

    pub fn cursor_state(
        &self,
        area: Rect,
        show_cursor: bool,
    ) -> Option<crate::pane::TerminalCursorState> {
        self.0.cursor_state(area, show_cursor)
    }

    pub fn visible_text(&self) -> String {
        self.0.visible_text()
    }

    pub fn visible_ansi(&self) -> String {
        self.0.visible_ansi()
    }

    pub fn detection_text(&self) -> String {
        self.0.detection_text()
    }

    pub fn detection_ansi(&self) -> String {
        self.0.detection_ansi()
    }

    pub fn detection_text_and_unfaint(&self) -> (String, String) {
        self.0.detection_text_and_unfaint()
    }

    pub fn recent_text(&self, lines: usize) -> String {
        self.0.recent_text(lines)
    }

    pub fn recent_ansi(&self, lines: usize) -> String {
        self.0.recent_ansi(lines)
    }

    pub fn recent_unwrapped_text(&self, lines: usize) -> String {
        self.0.recent_unwrapped_text(lines)
    }

    pub fn recent_unwrapped_ansi(&self, lines: usize) -> String {
        self.0.recent_unwrapped_ansi(lines)
    }

    pub fn snapshot_history(&self) -> Option<String> {
        self.0.snapshot_history()
    }

    /// Capture or extend content identity under the parser lock (ADR-0040 §1).
    pub fn pin_selection(
        &self,
        selection: &mut crate::selection::Selection,
        drag: Option<(u16, u16, Rect)>,
    ) {
        self.0.pin_selection(selection, drag);
    }
    /// Resolve current endpoint coordinates, refusing discarded content.
    pub fn resolve_selection(
        &self,
        selection: &crate::selection::Selection,
    ) -> Option<crate::selection::Selection> {
        self.0.resolve_selection(selection)
    }

    pub fn extract_selection(&self, selection: &crate::selection::Selection) -> Option<String> {
        self.0.extract_selection(selection)
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, show_cursor: bool) {
        self.0.render(frame, area, show_cursor);
    }

    pub(crate) fn collect_dirty_patch(
        &self,
        area_width: u16,
        area_height: u16,
    ) -> crate::pane::TerminalDirtyPatchOutcome {
        self.0.collect_dirty_patch(area_width, area_height)
    }

    pub fn visible_hyperlinks(&self, area: Rect) -> Vec<((u16, u16), String, String)> {
        self.0.visible_hyperlinks(area)
    }

    pub fn kitty_image_placements_with_data_filter<F>(
        &self,
        needs_data: F,
    ) -> Vec<crate::ghostty::KittyImagePlacement>
    where
        F: FnMut(crate::ghostty::KittyImageDescriptor) -> bool,
    {
        self.0.kitty_image_placements_with_data_filter(needs_data)
    }

    pub fn keyboard_protocol(&self) -> crate::input::KeyboardProtocol {
        self.0.keyboard_protocol()
    }

    pub fn encode_terminal_key(&self, key: crate::input::TerminalKey) -> Vec<u8> {
        self.0.encode_terminal_key(key)
    }

    pub async fn send_bytes(&self, bytes: Bytes) -> Result<(), mpsc::error::SendError<Bytes>> {
        self.0.send_bytes(bytes).await
    }

    /// Retain a write ticket through an operator lease and any channel backlog.
    pub(crate) fn try_send_tracked(
        &self,
        bytes: Bytes,
        deadline: std::time::Instant,
        operator_interrupt: bool,
    ) -> crate::pty::write_receipt::WriteReceipt {
        self.0.try_send_tracked(bytes, deadline, operator_interrupt)
    }

    pub fn try_send_bytes(&self, bytes: Bytes) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        self.0.try_send_bytes(bytes)
    }

    /// Write flock-authored bytes without counting them as operator input
    /// (ADR-0018 §2). The idle wake is the only caller.
    pub fn try_send_flock_authored(
        &self,
        bytes: Bytes,
    ) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        self.0.try_send_flock_authored(bytes)
    }

    pub(crate) fn try_send_flock_authored_tracked(
        &self,
        bytes: Bytes,
        deadline: std::time::Instant,
    ) -> Result<Option<crate::pty::write_receipt::WriteReceipt>, mpsc::error::TrySendError<Bytes>>
    {
        self.0.try_send_flock_authored_tracked(bytes, deadline)
    }

    /// When operator input last reached this pane, if it ever has.
    pub fn last_operator_input_at(&self) -> Option<std::time::Instant> {
        self.0.last_operator_input_at()
    }

    pub(crate) fn begin_injection_lease(
        &self,
        token: &str,
        until: std::time::Instant,
        now: std::time::Instant,
    ) -> bool {
        self.0.begin_injection_lease(token, until, now)
    }

    pub(crate) fn end_injection_lease(&self, token: &str) -> bool {
        self.0.end_injection_lease(token)
    }

    pub(crate) fn expire_injection_lease(&self, now: std::time::Instant) -> bool {
        self.0.expire_injection_lease(now)
    }

    pub(crate) fn end_all_injection_leases(&self) {
        self.0.end_all_injection_leases();
    }

    pub(crate) fn drain_held_input(&self) -> crate::pane::HeldInputDrain {
        self.0.drain_held_input()
    }

    pub(crate) fn has_held_backlog(&self) -> bool {
        self.0.has_held_backlog()
    }

    pub(crate) fn injection_lease_until(&self) -> Option<std::time::Instant> {
        self.0.injection_lease_until()
    }

    pub(crate) fn holds_injection_lease(&self, token: &str, now: std::time::Instant) -> bool {
        self.0.holds_injection_lease(token, now)
    }

    #[cfg(test)]
    pub(crate) fn test_stamp_operator_input_at(&self, at: std::time::Instant) {
        self.0.test_stamp_operator_input_at(at);
    }

    pub async fn send_paste(&self, text: String) -> Result<(), mpsc::error::SendError<Bytes>> {
        self.0.send_paste(text).await
    }

    pub fn try_send_focus_event(&self, event: crate::ghostty::FocusEvent) -> bool {
        self.0.try_send_focus_event(event)
    }

    pub fn wheel_routing(&self) -> Option<crate::pane::WheelRouting> {
        self.0.wheel_routing()
    }

    pub fn encode_mouse_button(
        &self,
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.0.encode_mouse_button(kind, column, row, modifiers)
    }

    pub fn encode_mouse_motion(
        &self,
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.0.encode_mouse_motion(kind, column, row, modifiers)
    }

    pub fn encode_mouse_wheel(
        &self,
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.0.encode_mouse_wheel(kind, column, row, modifiers)
    }

    pub fn encode_alternate_scroll(
        &self,
        kind: crossterm::event::MouseEventKind,
    ) -> Option<Vec<u8>> {
        self.0.encode_alternate_scroll(kind)
    }

    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        self.0.cwd()
    }

    pub fn foreground_cwd(&self) -> Option<std::path::PathBuf> {
        self.0.foreground_cwd()
    }

    pub(crate) fn current_size(&self) -> (u16, u16) {
        self.0.current_size()
    }
}

#[cfg(test)]
impl TerminalRuntime {
    pub(crate) fn test_with_channel(cols: u16, rows: u16) -> (Self, mpsc::Receiver<Bytes>) {
        let (runtime, rx) = crate::pane::PaneRuntime::test_with_channel(cols, rows);
        (Self(runtime), rx)
    }

    pub(crate) fn test_with_channel_capacity(
        cols: u16,
        rows: u16,
        capacity: usize,
    ) -> (Self, mpsc::Receiver<Bytes>) {
        let (runtime, rx) =
            crate::pane::PaneRuntime::test_with_channel_capacity(cols, rows, capacity);
        (Self(runtime), rx)
    }

    pub(crate) fn test_with_screen_bytes(cols: u16, rows: u16, bytes: &[u8]) -> Self {
        Self(crate::pane::PaneRuntime::test_with_screen_bytes(
            cols, rows, bytes,
        ))
    }

    pub(crate) fn test_process_pty_bytes(&self, bytes: &[u8]) {
        self.0.test_process_pty_bytes(bytes);
    }

    pub(crate) fn test_with_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
    ) -> Self {
        Self(crate::pane::PaneRuntime::test_with_scrollback_bytes(
            cols,
            rows,
            scrollback_limit_bytes,
            bytes,
        ))
    }

    pub(crate) fn test_with_channel_and_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
        channel_capacity: usize,
    ) -> (Self, mpsc::Receiver<Bytes>) {
        let (runtime, rx) = crate::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            cols,
            rows,
            scrollback_limit_bytes,
            bytes,
            channel_capacity,
        );
        (Self(runtime), rx)
    }
}

#[cfg(test)]
mod write_ack_tests {
    use super::*;
    use std::{
        io::Read,
        sync::mpsc as reply,
        time::{Duration, Instant},
    };

    #[tokio::test]
    async fn s3b_api_to_native_actor_waits_for_final_write_and_notifies_once() {
        native_api_write("pane.write_input", false).await;
    }

    #[tokio::test]
    async fn s3c_native_text_write_precedes_encoded_paste_observation() {
        for method in ["pane.send_text", "pane.write_text", "agent.send"] {
            native_api_write(method, true).await;
        }
    }

    async fn native_api_write(method: &str, paste: bool) {
        let (api_tx, api_rx) = mpsc::unbounded_channel();
        let mut app = crate::app::App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let (event_tx, mut events) = mpsc::channel(8);
        app.event_tx = event_tx;
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("write-ack")];
        app.state.ensure_test_terminals();
        let pane = app.state.workspaces[0].tabs[0].root_pane;
        let public = app.state.public_pane_id(0, pane).unwrap();
        let (actor, mut peer) = crate::pty::actor::PtyIoActorHandle::test_spawn_on_socket_pair();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let runtime = TerminalRuntime(crate::pane::PaneRuntime::test_with_actor(actor));
        if paste {
            runtime.test_process_pty_bytes(b"\x1b[?2004h");
        }
        let now = Instant::now();
        assert!(runtime.begin_injection_lease("held", now + Duration::from_secs(2), now));
        app.state.workspaces[0].tabs[0]
            .runtimes
            .insert(pane, runtime);
        let (respond_to, response) = reply::channel();
        api_tx.send(crate::api::ApiRequestMessage {
            scope: None,
            request: serde_json::from_value(serde_json::json!({"id":"native","method":method,"params":{"pane_id":public,"target":public,"text":"é","keys":["Enter"],"raw":true,"write_timeout_ms":200}})).unwrap(),
            respond_to, peer_pid:Some(std::process::id()), peer_birth:crate::platform::process_start_time(std::process::id()), peer_uid:Some(crate::platform::peer::effective_uid()),
        }).unwrap();
        app.drain_api_requests();
        assert!(matches!(
            response.try_recv(),
            Err(reply::TryRecvError::Empty)
        ));
        tokio::time::timeout(Duration::from_secs(2), app.input_writes.notify.notified())
            .await
            .unwrap();
        assert!(app.state.workspaces[0].tabs[0].runtimes[&pane].end_injection_lease("held"));
        let expected = if paste {
            "\x1b[200~é\x1b[201~"
        } else {
            "é\r"
        };
        let mut observed = vec![0; expected.len()];
        peer.read_exact(&mut observed).unwrap();
        assert_eq!(observed, expected.as_bytes());
        tokio::time::timeout(Duration::from_secs(2), app.input_writes.notify.notified())
            .await
            .unwrap();
        if paste {
            let deadline = app.input_writes.next_deadline();
            tokio::time::timeout(
                Duration::from_secs(2),
                tokio::time::sleep_until(tokio::time::Instant::from_std(deadline.unwrap())),
            )
            .await
            .unwrap();
        }
        app.service_input_writes();
        if paste {
            assert!(
                response.try_recv().is_err(),
                "completed writing still needs observation"
            );
            app.state.workspaces[0].tabs[0].runtimes[&pane]
                .test_process_pty_bytes("\x1b[2J\x1b[Hé".as_bytes());
            let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap();
            app.handle_internal_event(event);
        }
        let value: serde_json::Value = serde_json::from_str(&response.try_recv().unwrap()).unwrap();
        assert!(value.get("result").is_some(), "{value}");
        if paste {
            assert_eq!(value["result"]["type"], "paste");
            assert_eq!(value["result"]["outcome"], "delivered");
            assert_eq!(value["result"]["evidence"]["level"], "text_matched");
        }
        app.service_input_writes();
        assert!(response.try_recv().is_err());
    }
}
