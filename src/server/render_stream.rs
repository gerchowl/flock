//! Virtual rendering helpers for headless client frame streaming.

use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
use ratatui::layout::{Position, Rect, Size};

use crate::app::state::AppState;
use crate::app::Mode;
use crate::protocol::render_ansi::{BlitEncoder, EncodedBlit};
use crate::protocol::{CursorState, FrameData, RenderEncoding, ServerMessage, TerminalFrame};
use crate::terminal::TerminalRuntimeRegistry;

/// Per-client render baseline for the negotiated render encoding.
pub(crate) enum ClientRenderState {
    /// Semantic clients compare full frame data and skip identical frames.
    Semantic { last_frame: Option<FrameData> },
    /// Terminal-ANSI clients keep a terminal diff encoder and sequence number.
    TerminalAnsi { blit_encoder: BlitEncoder, seq: u64 },
}

impl ClientRenderState {
    pub(crate) fn new(render_encoding: RenderEncoding) -> Self {
        match render_encoding {
            RenderEncoding::SemanticFrame => Self::Semantic { last_frame: None },
            RenderEncoding::TerminalAnsi => Self::TerminalAnsi {
                blit_encoder: BlitEncoder::new(),
                seq: 0,
            },
        }
    }

    pub(crate) fn reset_baseline(&mut self) {
        match self {
            Self::Semantic { last_frame } => *last_frame = None,
            Self::TerminalAnsi { blit_encoder, .. } => *blit_encoder = BlitEncoder::new(),
        }
    }

    pub(crate) fn reset_semantic_input_baseline(&mut self) {
        if let Self::Semantic { last_frame } = self {
            *last_frame = None;
        }
    }

    pub(crate) fn prepare_frame(&mut self, frame: FrameData) -> Option<PreparedRender> {
        match self {
            Self::Semantic { last_frame } => {
                if last_frame.as_ref() == Some(&frame) {
                    crate::render_prof::event("prepare_frame.semantic.skip_current");
                    return None;
                }
                crate::render_prof::event("prepare_frame.semantic.changed");
                Some(PreparedRender::Semantic {
                    message: ServerMessage::Frame(frame),
                })
            }
            Self::TerminalAnsi { blit_encoder, seq } => {
                if blit_encoder.is_current(&frame) {
                    crate::render_prof::event("prepare_frame.ansi.skip_current");
                    return None;
                }
                let mut encoded = blit_encoder.encode(&frame, false);
                crate::render_prof::event("prepare_frame.ansi.changed");
                crate::render_prof::counter("prepare_frame.ansi.bytes", encoded.bytes.len() as u64);
                if encoded.full {
                    crate::render_prof::event("prepare_frame.ansi.full");
                } else {
                    crate::render_prof::event("prepare_frame.ansi.partial");
                }
                insert_graphics_before_sync_end(&mut encoded.bytes, &frame.graphics);
                crate::render_prof::counter(
                    "prepare_frame.graphics.bytes",
                    frame.graphics.len() as u64,
                );
                Some(PreparedRender::TerminalAnsi {
                    message: ServerMessage::Terminal(TerminalFrame {
                        seq: *seq + 1,
                        width: frame.width,
                        height: frame.height,
                        full: encoded.full,
                        bytes: encoded.bytes.clone(),
                    }),
                    frame,
                    encoded: Some(encoded),
                })
            }
        }
    }

    pub(crate) fn last_frame(&self) -> Option<&FrameData> {
        match self {
            Self::Semantic { last_frame } => last_frame.as_ref(),
            Self::TerminalAnsi { blit_encoder, .. } => blit_encoder.last_frame(),
        }
    }

    pub(crate) fn commit_sent_frame(&mut self, prepared: PreparedRender) {
        match (self, prepared) {
            (
                Self::Semantic { last_frame },
                PreparedRender::Semantic {
                    message: ServerMessage::Frame(frame),
                },
            ) => *last_frame = Some(frame),
            (
                Self::TerminalAnsi { blit_encoder, seq },
                PreparedRender::TerminalAnsi {
                    frame,
                    encoded: Some(encoded),
                    ..
                },
            ) => {
                blit_encoder.commit(frame, encoded);
                *seq += 1;
            }
            _ => {}
        }
    }

    #[cfg(test)]
    pub(crate) fn terminal_seq(&self) -> Option<u64> {
        match self {
            Self::Semantic { .. } => None,
            Self::TerminalAnsi { seq, .. } => Some(*seq),
        }
    }
}

const SYNC_OUTPUT_END: &[u8] = b"\x1b[?2026l";

fn insert_graphics_before_sync_end(encoded: &mut Vec<u8>, graphics: &[u8]) {
    if graphics.is_empty() {
        return;
    }

    if let Some(sync_end) = rfind_subslice(encoded, SYNC_OUTPUT_END) {
        encoded.splice(sync_end..sync_end, graphics.iter().copied());
    } else {
        encoded.extend_from_slice(graphics);
    }
}

fn rfind_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }

    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

/// A prepared client render message plus any baseline state needed after send.
pub(crate) enum PreparedRender {
    Semantic {
        message: ServerMessage,
    },
    TerminalAnsi {
        message: ServerMessage,
        frame: FrameData,
        encoded: Option<EncodedBlit>,
    },
}

impl PreparedRender {
    pub(crate) fn message(&self) -> &ServerMessage {
        match self {
            Self::Semantic { message } | Self::TerminalAnsi { message, .. } => message,
        }
    }

    pub(crate) fn into_frame(self) -> Option<FrameData> {
        match self {
            Self::Semantic {
                message: ServerMessage::Frame(frame),
            } => Some(frame),
            Self::TerminalAnsi { frame, .. } => Some(frame),
            _ => None,
        }
    }
}

struct CursorTrackingBackend {
    inner: TestBackend,
    rendered_cursor: Option<Position>,
}

impl CursorTrackingBackend {
    fn new(width: u16, height: u16) -> Self {
        Self {
            inner: TestBackend::new(width, height),
            rendered_cursor: None,
        }
    }

    fn buffer(&self) -> &ratatui::buffer::Buffer {
        self.inner.buffer()
    }

    fn rendered_cursor(&self) -> Option<CursorState> {
        self.rendered_cursor.map(|pos| CursorState {
            x: pos.x,
            y: pos.y,
            visible: true,
            shape: 0,
        })
    }
}

impl Backend for CursorTrackingBackend {
    type Error = std::convert::Infallible;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.inner.draw(content)
    }

    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()?;
        self.rendered_cursor = None;
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        let position = position.into();
        self.inner.set_cursor_position(position)?;
        self.rendered_cursor = Some(position);
        Ok(())
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}

/// Renders the AppState to an in-memory ratatui Buffer.
///
/// This produces the same output as the monolithic binary's terminal draw,
/// but writes to a `Buffer` instead of stdout. Cursor visibility is captured
/// from explicit frame cursor intent rather than incidental backend state.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn render_virtual(
    app_state: &mut AppState,
    area: Rect,
    resize_panes: bool,
) -> (ratatui::buffer::Buffer, Option<CursorState>) {
    let terminal_runtimes = TerminalRuntimeRegistry::new();
    render_virtual_with_runtime_registry(
        app_state,
        &terminal_runtimes,
        area,
        resize_panes,
        crate::kitty_graphics::HostCellSize::default(),
    )
}

pub(crate) fn render_virtual_with_runtime_registry(
    app_state: &mut AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    area: Rect,
    resize_panes: bool,
    cell_size: crate::kitty_graphics::HostCellSize,
) -> (ratatui::buffer::Buffer, Option<CursorState>) {
    if resize_panes {
        crate::ui::compute_view_with_cell_size(app_state, terminal_runtimes, area, cell_size);
    } else {
        crate::ui::compute_view_without_resizing_panes(app_state, terminal_runtimes, area);
    }

    let backend = CursorTrackingBackend::new(area.width, area.height);
    let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend::new should never fail");

    terminal
        .draw(|frame| {
            crate::ui::render_with_runtime_registry(app_state, terminal_runtimes, frame);
        })
        .expect("render to TestBackend should never fail");

    let buffer = terminal.backend().buffer().clone();
    let cursor = focused_terminal_cursor(app_state, terminal_runtimes)
        .or_else(|| terminal.backend().rendered_cursor());

    (buffer, cursor)
}

/// Renders one server-owned terminal directly for `terminal attach` clients.
pub(crate) fn render_terminal_virtual(
    runtime: &crate::terminal::TerminalRuntime,
    area: Rect,
) -> (ratatui::buffer::Buffer, Option<CursorState>) {
    let backend = CursorTrackingBackend::new(area.width, area.height);
    let mut terminal = ratatui::Terminal::new(backend).expect("TestBackend::new should never fail");

    terminal
        .draw(|frame| {
            runtime.render(frame, area, true);
        })
        .expect("render to TestBackend should never fail");

    let buffer = terminal.backend().buffer().clone();
    let cursor = runtime
        .cursor_state(area, true)
        .map(|cursor| CursorState {
            x: cursor.x,
            y: cursor.y,
            visible: cursor.visible && !crate::ui::pane_is_scrolled_back(runtime),
            shape: cursor.shape,
        })
        .or_else(|| terminal.backend().rendered_cursor());

    (buffer, cursor)
}

pub(crate) fn visible_hyperlinks(
    app_state: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> Vec<((u16, u16), String, String)> {
    let Some(ws_idx) = app_state.active else {
        return Vec::new();
    };
    let Some(tab) = app_state
        .workspaces
        .get(ws_idx)
        .and_then(crate::workspace::Workspace::active_tab)
    else {
        return Vec::new();
    };

    let mut links = Vec::new();
    for info in &app_state.view.pane_infos {
        if let Some(runtime) = tab
            .terminal_id(info.id)
            .and_then(|terminal_id| terminal_runtimes.get(terminal_id))
        {
            links.extend(runtime.visible_hyperlinks(info.inner_rect));
        }
    }
    links
}

pub(crate) fn focused_terminal_cursor(
    app_state: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> Option<CursorState> {
    if app_state.mode != Mode::Terminal {
        return None;
    }

    let ws_idx = app_state.active?;
    let info = app_state
        .view
        .pane_infos
        .iter()
        .find(|info| info.is_focused)?;
    let rt = app_state.runtime_for_pane_in_workspace(terminal_runtimes, ws_idx, info.id)?;
    let scrolled_back = crate::ui::pane_is_scrolled_back(rt);

    // Determine whether the IME-anchor reveal applies to this focused pane.
    // The master switch must be on, and either no agent filter is configured
    // (apply to any pane) or the focused pane's detected agent matches the
    // allow-list. A configured list with no valid entries reveals nothing.
    let reveal = app_state.reveal_hidden_cursor_for_cjk_ime
        && (!app_state.cjk_ime_agent_filter_configured || {
            let detected = app_state
                .workspaces
                .get(ws_idx)
                .and_then(|ws| ws.terminal_id(info.id))
                .and_then(|tid| app_state.terminals.get(tid))
                .and_then(|t| t.detected_agent);
            detected.is_some_and(|agent| app_state.cjk_ime_agents.contains(&agent))
        });

    if let Some(cursor) = rt.cursor_state(info.inner_rect, true) {
        // When the reveal applies, expose the cursor anchor regardless of the
        // pane's `?25l` request so macOS IMEs keep tracking the candidate
        // window when TUIs paint their own cursor. Scrollback suppression
        // still applies.
        let visible = if reveal {
            !scrolled_back
        } else {
            cursor.visible && !scrolled_back
        };
        Some(CursorState {
            x: cursor.x,
            y: cursor.y,
            visible,
            shape: if reveal && visible {
                app_state.cjk_ime_cursor_shape
            } else {
                cursor.shape
            },
        })
    } else if reveal && !scrolled_back {
        // cursor_state() returned None — the viewport has no cursor position
        // (can happen with complex TUIs). Fall back to the pane's top-left so
        // the outer terminal still exposes a cursor anchor for IME tracking.
        Some(CursorState {
            x: info.inner_rect.x,
            y: info.inner_rect.y,
            visible: true,
            shape: app_state.cjk_ime_cursor_shape,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the per-client render baseline.
    //!
    //! Covers the inactive-pane render-work-skip behavior ported from
    //! herdr #512: an unchanged pane (which is what an inactive pane looks
    //! like from the render stream's perspective — no new frame deltas) must
    //! short-circuit `prepare_frame` so the headless server skips encode +
    //! send work, while a first-render or a changed frame still produces a
    //! correctly serialized message.
    use super::*;
    use crate::protocol::{CellData, FrameData, RenderEncoding};

    fn cell(symbol: &str) -> CellData {
        CellData {
            symbol: symbol.to_owned(),
            fg: 0,
            bg: 0,
            modifier: 0,
            skip: false,
            hyperlink: None,
        }
    }

    fn frame_with(symbol: &str) -> FrameData {
        FrameData {
            cells: vec![cell(symbol); 4],
            width: 2,
            height: 2,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        }
    }

    #[test]
    fn semantic_inactive_pane_skips_repeated_render_work() {
        let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
        let frame = frame_with("x");

        // First render: client has no baseline, so the frame must be prepared
        // and committed (covers an attaching client / focused pane).
        let prepared = state
            .prepare_frame(frame.clone())
            .expect("first render must produce a frame");
        assert!(matches!(
            prepared.message(),
            ServerMessage::Frame(produced) if produced == &frame
        ));
        state.commit_sent_frame(prepared);
        assert_eq!(state.last_frame(), Some(&frame));

        // Inactive pane: nothing changed between ticks. prepare_frame must
        // return None so the caller skips serialize + send.
        assert!(state.prepare_frame(frame.clone()).is_none());

        // A change still re-renders correctly (active pane unaffected).
        let changed = frame_with("y");
        let prepared = state
            .prepare_frame(changed.clone())
            .expect("changed frame must re-render");
        match prepared.message() {
            ServerMessage::Frame(produced) => assert_eq!(produced, &changed),
            other => panic!("unexpected message variant: {other:?}"),
        }
        state.commit_sent_frame(prepared);
        assert_eq!(state.last_frame(), Some(&changed));
    }

    #[test]
    fn terminal_ansi_inactive_pane_skips_repeated_render_work() {
        let mut state = ClientRenderState::new(RenderEncoding::TerminalAnsi);
        let frame = frame_with("x");

        // First render bumps the seq.
        let prepared = state
            .prepare_frame(frame.clone())
            .expect("first render must produce a frame");
        assert!(matches!(prepared.message(), ServerMessage::Terminal(_)));
        state.commit_sent_frame(prepared);
        assert_eq!(state.terminal_seq(), Some(1));

        // No change: no work, no seq bump.
        assert!(state.prepare_frame(frame.clone()).is_none());
        assert_eq!(state.terminal_seq(), Some(1));

        // Change re-renders and bumps the seq.
        let changed = frame_with("y");
        let prepared = state
            .prepare_frame(changed)
            .expect("changed frame must re-render");
        assert!(matches!(prepared.message(), ServerMessage::Terminal(_)));
        state.commit_sent_frame(prepared);
        assert_eq!(state.terminal_seq(), Some(2));
    }

    #[test]
    fn prepared_render_recovers_owned_frame_for_fallback() {
        // Recovering the owned frame from `PreparedRender::into_frame` is what
        // lets the headless full-render path drop graphics from an oversized
        // frame without re-cloning the entire FrameData per send.
        let mut state = ClientRenderState::new(RenderEncoding::TerminalAnsi);
        let mut frame = frame_with("x");
        frame.graphics = vec![1, 2, 3];

        let prepared = state.prepare_frame(frame.clone()).expect("prepare frame");
        let recovered = prepared.into_frame().expect("frame recoverable");
        assert_eq!(recovered, frame);

        // Semantic variant carries the frame inside the message.
        let mut sem_state = ClientRenderState::new(RenderEncoding::SemanticFrame);
        let prepared = sem_state
            .prepare_frame(frame.clone())
            .expect("semantic prepare");
        let recovered = prepared.into_frame().expect("frame recoverable");
        assert_eq!(recovered, frame);
    }
}
