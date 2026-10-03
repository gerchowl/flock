//! The notification panel's renderer (#516).
//!
//! The modal language is the one the settings overlay already uses — the same
//! centred popup, the same `modal_stack_areas` stack, the same
//! `[↵ primary] [hint verb] [esc close]` button row — because ADR-0016 §6
//! asked for the existing panel language rather than a new screen. Every rect
//! comes from `notifications_panel_stack` / `notifications_panel_button_rects`,
//! which the mouse hit-test calls with the same arguments, so what is drawn and
//! what is clickable cannot drift apart.
//!
//! ## The retention line
//!
//! `NotificationLog` keeps at most `MAX_NOTIFICATIONS` records and evicts
//! **read** records first, then the oldest **unread**, and an eviction emits
//! nothing into the log. So the decisions most worth answering are the ones
//! that can fall off the bottom of this list silently, and "never answered and
//! evicted" is indistinguishable from "never filed".
//!
//! The panel does not fix that — retention is ADR-0016 §3's open question
//! (ADR-0022 §3) — so it *states* it instead: the standing line above the
//! buttons names the cap, dates the window's oldest surviving record, and says
//! outright that older records may already have been evicted once the
//! projection is full. A panel that quietly showed a short list would be the
//! misleading one.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
    Frame,
};

use super::scrollbar::should_show_scrollbar;
use super::widgets::{
    action_button_row_rects, display_width, modal_stack_areas, panel_contrast_fg,
    render_action_button, render_modal_header, render_panel_shell, ActionButtonSpec,
    ModalStackAreas,
};
use crate::api::schema::NotificationRecordKind;
use crate::app::notifications::{now_ms, NotificationEntry, MAX_NOTIFICATIONS};
use crate::app::state::{AppState, Palette};

/// The same `76x22` popup the settings overlay and the keybind help use.
pub(crate) const NOTIFICATIONS_PANEL_MODAL_SIZE: (u16, u16) = (76, 22);
/// Title, then the count subtitle.
pub(crate) const NOTIFICATIONS_PANEL_HEADER_ROWS: u16 = 2;
/// Key hints, the retention line, then the button row.
pub(crate) const NOTIFICATIONS_PANEL_FOOTER_ROWS: u16 = 3;
pub(crate) const HINTS_ROW_OFFSET: u16 = 0;
pub(crate) const RETENTION_ROW_OFFSET: u16 = 1;

/// The panel's layout, as a function of the popup's inner rect.
///
/// Every rect below is derived from *one* split so the renderer and the mouse
/// hit-test cannot place the same row in two different places. The settings
/// overlay keeps its split in one function for the same reason
/// (`settings_content_rect` + `settings_button_rects`); this one is shared
/// rather than duplicated because the panel has more slots to get wrong.
pub(crate) fn notifications_panel_stack(inner: Rect) -> ModalStackAreas {
    modal_stack_areas(
        inner,
        NOTIFICATIONS_PANEL_HEADER_ROWS,
        NOTIFICATIONS_PANEL_FOOTER_ROWS,
        0,
        1,
    )
}

/// The record rows — the content slot minus its last row, which carries the
/// selected record's body.
pub(crate) fn notifications_panel_body_rect(stack: &ModalStackAreas) -> Rect {
    let content = stack.content;
    if content.height < 2 {
        return Rect::default();
    }
    Rect::new(
        content.x,
        content.y,
        content.width,
        content.height.saturating_sub(1),
    )
}

pub(crate) fn notifications_panel_detail_rect(stack: &ModalStackAreas) -> Rect {
    let content = stack.content;
    Rect::new(
        content.x,
        content.y + content.height.saturating_sub(1),
        content.width,
        1,
    )
}

pub(crate) fn notifications_panel_footer_row(stack: &ModalStackAreas, offset: u16) -> Rect {
    let Some(footer) = stack.footer else {
        return Rect::default();
    };
    Rect::new(
        footer.x,
        footer.y + offset.min(footer.height.saturating_sub(1)),
        footer.width,
        1,
    )
}

/// The panel's action buttons, one rect each: acknowledge the selected record,
/// acknowledge every unread one, close.
///
/// `pub(crate)` and used by the mouse hit-test as well as the renderer, so a
/// click and a keypress land on the same target (the settings overlay's
/// `settings_button_rects` does the same).
pub(crate) fn notifications_panel_button_rects(inner: Rect) -> (Rect, Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "ack",
            },
            ActionButtonSpec {
                hint: Some("a"),
                label: "ack unread",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "close",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (rects[0], rects[1], rects[2])
}

pub(super) fn render_notifications_panel(app: &AppState, frame: &mut Frame, area: Rect) {
    let p = &app.palette;
    let Some(popup) = super::centered_popup_rect(
        area,
        NOTIFICATIONS_PANEL_MODAL_SIZE.0,
        NOTIFICATIONS_PANEL_MODAL_SIZE.1,
    ) else {
        return;
    };

    super::dim_background(frame, area);

    let Some(inner) = render_panel_shell(frame, popup, p.accent, p.panel_bg) else {
        return;
    };
    if inner.height < 8 || inner.width < 20 {
        return;
    }
    let stack = notifications_panel_stack(inner);

    render_header(app, frame, stack.header, inner);

    let body = notifications_panel_body_rect(&stack);
    if body.height > 0 {
        let rows = app.notification_panel_rows();
        let start = app.notifications_panel.scroll.min(rows.len());
        let end = rows.len().min(start.saturating_add(body.height as usize));
        for (visible, entry) in rows[start..end].iter().enumerate() {
            render_row(app, frame, body, visible as u16, entry, start + visible);
        }
        render_panel_scrollbar(app, frame, body, rows.len());
    }

    render_detail(app, frame, notifications_panel_detail_rect(&stack));
    render_retention_line(
        app,
        frame,
        notifications_panel_footer_row(&stack, RETENTION_ROW_OFFSET),
    );
    render_hints(
        app,
        frame,
        notifications_panel_footer_row(&stack, HINTS_ROW_OFFSET),
    );

    let (ack, ack_unread, close) = notifications_panel_button_rects(inner);
    let primary = Style::default()
        .fg(panel_contrast_fg(p))
        .bg(p.accent)
        .add_modifier(Modifier::BOLD);
    let secondary = Style::default()
        .fg(p.text)
        .bg(p.surface0)
        .add_modifier(Modifier::BOLD);
    render_action_button(frame, ack, Some("↵"), "ack", primary);
    render_action_button(frame, ack_unread, Some("a"), "ack unread", secondary);
    render_action_button(frame, close, Some("esc"), "close", secondary);
}

/// Title, then the counts. One close affordance, on the button row — a second
/// `esc close` beside the title would be two places to keep in step for one
/// action, and the settings overlay has exactly this shape.
fn render_header(app: &AppState, frame: &mut Frame, header: Rect, inner: Rect) {
    let p = &app.palette;
    render_modal_header(
        frame,
        Rect::new(header.x, header.y, header.width, 1),
        " notifications",
        p,
    );

    let kept = app.notifications.len();
    let unread = app.notifications.unread();
    let mut spans = vec![Span::styled(
        format!(" {kept} kept · {unread} unread"),
        Style::default().fg(p.overlay1),
    )];
    if app.notifications_panel.unread_only {
        spans.push(Span::styled(" · ", Style::default().fg(p.overlay0)));
        spans.push(Span::styled(
            "unread only",
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(inner.x, header.y + 1, inner.width, 1),
    );
}

fn kind_label(kind: NotificationRecordKind) -> &'static str {
    match kind {
        NotificationRecordKind::Attention => "attention",
        NotificationRecordKind::Outcome => "outcome",
        NotificationRecordKind::Notice => "notice",
    }
}

/// `Attention` is "something is waiting on you", so it wears the same red as
/// the blocked state and the needs-attention toast dot.
fn kind_color(kind: NotificationRecordKind, p: &Palette) -> ratatui::style::Color {
    match kind {
        NotificationRecordKind::Attention => p.red,
        NotificationRecordKind::Outcome => p.green,
        NotificationRecordKind::Notice => p.blue,
    }
}

fn render_row(
    app: &AppState,
    frame: &mut Frame,
    body: Rect,
    visible: u16,
    entry: &NotificationEntry,
    index: usize,
) {
    let p = &app.palette;
    let rect = Rect::new(body.x, body.y + visible, body.width, 1);
    let selected = index == app.notifications_panel.selected;
    frame.render_widget(Clear, rect);

    let base = if selected {
        Style::default().bg(p.accent).fg(panel_contrast_fg(p))
    } else {
        Style::default().bg(p.panel_bg).fg(p.text)
    };
    let dim = if selected {
        base
    } else {
        Style::default().fg(p.overlay0).bg(p.panel_bg)
    };
    let title_style = if selected {
        base.add_modifier(Modifier::BOLD)
    } else if entry.seen {
        Style::default().fg(p.subtext0).bg(p.panel_bg)
    } else {
        Style::default().fg(p.text).bg(p.panel_bg)
    };
    // `●` is the dot the menus, the settings tabs and the sidebar already use
    // for "this needs you"; a read record shows its absence, not a second
    // colour, so the eye finds the unread ones first.
    let marker_style = if selected {
        base.add_modifier(Modifier::BOLD)
    } else if entry.seen {
        Style::default().fg(p.surface1).bg(p.panel_bg)
    } else {
        Style::default()
            .fg(p.accent)
            .bg(p.panel_bg)
            .add_modifier(Modifier::BOLD)
    };
    let kind_style = if selected {
        base.add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(kind_color(entry.kind, p))
            .bg(p.panel_bg)
    };

    let marker = if entry.seen { "○" } else { "●" };
    let kind = kind_label(entry.kind);
    let meta = format!(
        "{} · {}",
        entry.origin_host,
        super::format_age(now_ms().saturating_sub(entry.filed_at_ms) / 1000)
    );
    let meta_width = display_width(&meta).saturating_add(1) as usize;
    let fixed_width = 1 + display_width(marker) as usize + 1 + kind.chars().count() + 1;
    let title_budget = (rect.width as usize)
        .saturating_sub(fixed_width)
        .saturating_sub(meta_width);

    let mut spans = vec![
        Span::raw(" "),
        Span::styled(marker, marker_style),
        Span::raw(" "),
        Span::styled(kind, kind_style),
        Span::raw(" "),
        Span::styled(truncate_text(&entry.title, title_budget), title_style),
    ];
    let used: usize = spans
        .iter()
        .map(|span| display_width(span.content.as_ref()) as usize)
        .sum();
    let pad = (rect.width as usize)
        .saturating_sub(used)
        .saturating_sub(meta_width);
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(meta, if selected { base } else { dim }));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(base), rect);
}

fn render_panel_scrollbar(app: &AppState, frame: &mut Frame, body: Rect, rows: usize) {
    if body.width <= 1 || body.height == 0 {
        return;
    }
    let viewport = body.height as usize;
    if rows <= viewport {
        return;
    }
    let metrics = crate::pane::ScrollMetrics {
        viewport_rows: viewport,
        offset_from_bottom: rows
            .saturating_sub(viewport)
            .saturating_sub(app.notifications_panel.scroll),
        max_offset_from_bottom: rows.saturating_sub(viewport),
    };
    if !should_show_scrollbar(metrics) {
        return;
    }
    let track = Rect::new(body.x + body.width - 1, body.y, 1, body.height);
    super::scrollbar::render_scrollbar(
        frame,
        metrics,
        track,
        app.palette.surface_dim,
        app.palette.overlay0,
        "▕",
    );
}

fn render_detail(app: &AppState, frame: &mut Frame, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let Some(entry) = app.selected_notification_panel_entry() else {
        return;
    };
    let Some(body) = entry.body.as_deref().filter(|body| !body.is_empty()) else {
        return;
    };
    let text = truncate_text(body, area.width.saturating_sub(2) as usize);
    frame.render_widget(
        Paragraph::new(format!(" {text}")).style(Style::default().fg(app.palette.overlay1)),
        area,
    );
}

/// What the panel offers in place of a retention fix.
///
/// Always present, so it never implies completeness by omission. When the
/// projection is full it says the thing the operator would otherwise have no
/// way to learn: older records may already have been evicted, and an eviction
/// leaves no trace in the log.
///
/// `now_ms` is a parameter rather than a call to the clock so the text is
/// testable instead of only observable.
pub(crate) fn notifications_retention_line(app: &AppState, now_ms: u64) -> (String, bool) {
    let capped = app.notifications.at_cap();
    let oldest = match app.notifications.oldest_filed_at_ms() {
        Some(filed_at_ms) => format!(
            "{} old",
            super::format_age(now_ms.saturating_sub(filed_at_ms) / 1000)
        ),
        None => "nothing yet".to_string(),
    };
    let mut line = format!("cap {MAX_NOTIFICATIONS} · oldest kept {oldest}");
    if capped {
        line.push_str(" · older records may already be evicted");
    }
    (line, capped)
}

fn render_retention_line(app: &AppState, frame: &mut Frame, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let (line, capped) = notifications_retention_line(app, now_ms());
    let p = &app.palette;
    let style = if capped {
        Style::default().fg(p.yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(p.overlay0)
    };
    let text = truncate_text(&line, area.width.saturating_sub(1) as usize);
    frame.render_widget(Paragraph::new(format!(" {text}")).style(style), area);
}

fn render_hints(app: &AppState, frame: &mut Frame, area: Rect) {
    if area.height == 0 {
        return;
    }
    let p = &app.palette;
    let key = Style::default().fg(p.accent).add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(p.overlay0);
    let line = Line::from(vec![
        Span::styled("↵", key),
        Span::styled(" ack  ", dim),
        Span::styled("a", key),
        Span::styled(" ack unread  ", dim),
        Span::styled("u", key),
        Span::styled(" unread only  ", dim),
        Span::styled("j/k/↑↓", key),
        Span::styled(" move  ", dim),
        Span::styled("wheel", key),
        Span::styled(" scroll  ", dim),
        Span::styled("esc", key),
        Span::styled(" close", dim),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn truncate_text(text: &str, max_width: usize) -> String {
    let len = text.chars().count();
    if len <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let prefix: String = text.chars().take(max_width.saturating_sub(1)).collect();
    format!("{prefix}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::NotificationSource;
    use crate::app::notification_panel::NotificationPanelAction;

    fn entry(id: &str, seen: bool, filed_at_ms: u64) -> NotificationEntry {
        NotificationEntry {
            id: id.to_string(),
            title: format!("{id} finished"),
            body: None,
            kind: NotificationRecordKind::Outcome,
            source: NotificationSource::AgentState,
            workspace_id: None,
            pane_id: None,
            origin_host: "host.invalid".into(),
            filed_at_ms,
            seen,
        }
    }

    /// The line is what the panel offers in place of a retention fix, so the
    /// capped case has to say something the uncapped case does not. A panel
    /// that said the same words either way would be the misleading version of
    /// this screen.
    #[test]
    fn the_retention_line_admits_a_full_projection_is_lossy() {
        let mut app = AppState::test_new();
        app.open_notification_panel();
        app.file_notification(entry("only", false, 1_000));
        let now = 1_000_000 + 3_600_000;

        let (line, capped) = notifications_retention_line(&app, now);
        assert!(!capped);
        assert!(line.contains(&format!("cap {MAX_NOTIFICATIONS}")), "{line}");
        assert!(line.contains("oldest kept 1h old"), "{line}");
        assert!(
            !line.contains("evicted"),
            "nothing has been given up yet, so do not cry wolf: {line}"
        );

        for index in 0..MAX_NOTIFICATIONS {
            app.file_notification(entry(&format!("n-{index}"), false, 1_000));
        }

        let (line, capped) = notifications_retention_line(&app, now);
        assert!(
            capped,
            "the projection is full, so the next record costs a record"
        );
        assert!(
            line.contains("older records may already be evicted"),
            "{line}"
        );
        assert!(line.contains(&format!("cap {MAX_NOTIFICATIONS}")), "{line}");
    }

    /// An empty log has no oldest record, so the line must not invent an age.
    #[test]
    fn the_retention_line_reads_nothing_yet_on_an_empty_log() {
        let app = AppState::test_new();
        let (line, capped) = notifications_retention_line(&app, 1_000);
        assert!(!capped);
        assert!(line.contains("oldest kept nothing yet"), "{line}");
    }

    /// The age comes from the caller's clock, not from `SystemTime` inside the
    /// renderer, so the text is testable rather than only observable.
    #[test]
    fn the_retention_line_ages_the_oldest_surviving_record_from_the_given_clock() {
        let mut app = AppState::test_new();
        app.file_notification(entry("oldest", false, 10_000));
        app.file_notification(entry("newest", false, 90_000));

        let (line, _) = notifications_retention_line(&app, 100_000);
        assert!(line.contains("oldest kept 1m old"), "{line}");
    }

    /// The rendered buffer, not the state: the panel's whole job is what is
    /// on screen, and this is the level a caller actually reads. A record
    /// filed through the projection, then drawn, has to come out as a row with
    /// its title, its host and its age — and the retention line has to be
    /// there whether or not anything has been evicted yet.
    #[test]
    fn the_panel_renders_the_record_it_was_given() {
        let mut app = AppState::test_new();
        app.file_notification(entry("build", false, now_ms()));
        app.open_notification_panel();

        let rendered = render_panel_to_string(&mut app, 100, 30);

        assert!(rendered.contains("notifications"), "{rendered}");
        assert!(rendered.contains("1 kept · 1 unread"), "{rendered}");
        assert!(rendered.contains("build finished"), "{rendered}");
        assert!(rendered.contains("host.invalid"), "{rendered}");
        assert!(rendered.contains("ack unread"), "{rendered}");
        assert!(
            rendered.contains("cap 512 · oldest kept"),
            "the window's edge is stated even when nothing has been evicted: \
             {rendered}"
        );
    }

    /// Reading is not acknowledging, and the screen has to show which is
    /// which: an unread row keeps its title in `text`, a read one recedes to
    /// `subtext0`. Both titles are on the buffer; only one is prominent.
    #[test]
    fn a_read_record_still_lists_but_does_not_read_as_unanswered() {
        let mut app = AppState::test_new();
        app.file_notification(entry("answered", true, now_ms()));
        app.file_notification(entry("waiting", false, now_ms()));
        app.open_notification_panel();

        let rendered = render_panel_to_string(&mut app, 100, 30);

        assert!(rendered.contains("2 kept · 1 unread"), "{rendered}");
        assert!(rendered.contains("answered finished"), "{rendered}");
        assert!(rendered.contains("waiting finished"), "{rendered}");
    }

    /// The count, the cap and the oldest surviving record are the three facts
    /// that make the list honest, and all three have to survive to the screen.
    #[test]
    fn the_rendered_panel_states_what_it_is_not_showing() {
        let mut app = AppState::test_new();
        for index in 0..MAX_NOTIFICATIONS {
            app.file_notification(entry(&format!("n{index}"), false, 1_000));
        }
        app.open_notification_panel();

        let rendered = render_panel_to_string(&mut app, 120, 30);

        assert!(
            rendered.contains(&format!(
                "{MAX_NOTIFICATIONS} kept · {MAX_NOTIFICATIONS} unread"
            )),
            "{rendered}"
        );
        assert!(
            rendered.contains("older records may already be evicted"),
            "{rendered}"
        );
    }

    /// The unread filter is the CLI's `--unread`, so the panel says so rather
    /// than silently hiding half the log.
    #[test]
    fn the_unread_filter_says_it_is_filtering() {
        let mut app = AppState::test_new();
        app.file_notification(entry("read-one", true, now_ms()));
        app.file_notification(entry("unread-one", false, now_ms()));
        app.open_notification_panel();
        app.toggle_notification_panel_unread_only();

        let rendered = render_panel_to_string(&mut app, 100, 30);

        assert!(rendered.contains("unread only"), "{rendered}");
        assert!(rendered.contains("unread-one finished"), "{rendered}");
    }

    /// The record's own body is the part a title cannot carry, so it gets its
    /// own line under the list.
    #[test]
    fn the_selected_records_body_is_shown() {
        let mut app = AppState::test_new();
        let mut filed = entry("needs-you", false, now_ms());
        filed.body = Some("the pane that asked is gone".into());
        app.file_notification(filed);
        app.open_notification_panel();

        let rendered = render_panel_to_string(&mut app, 110, 30);

        assert!(
            rendered.contains("the pane that asked is gone"),
            "{rendered}"
        );
    }

    /// The renderer derives its rows from `inner`; the hit-test derives them
    /// from `AppState`'s copy of the geometry. Both go through
    /// `notifications_panel_stack`, so a change to one that moved a row would
    /// have to be a change to the other. This is the test that says so, rather
    /// than the comment on the function saying so.
    #[test]
    fn the_rows_drawn_are_the_rows_a_click_can_reach() {
        let (cols, rows) = (100u16, 30u16);
        let mut app = AppState::test_new();
        app.file_notification(entry("a", false, now_ms()));
        app.open_notification_panel();
        crate::ui::compute_view(&mut app, Rect::new(0, 0, cols, rows));

        let popup = super::super::centered_popup_rect(Rect::new(0, 0, cols, rows), 76, 22)
            .expect("the panel fits this screen");
        let inner = Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        );
        let drawn = notifications_panel_body_rect(&notifications_panel_stack(inner));

        assert_eq!(
            drawn,
            app.notifications_panel_body_rect(),
            "the drawn row area and the clickable row area must be the same rect"
        );

        let (ack, ack_unread, close) = notifications_panel_button_rects(inner);
        for (button, action) in [
            (ack, NotificationPanelAction::Acknowledge),
            (ack_unread, NotificationPanelAction::AcknowledgeAll),
            (close, NotificationPanelAction::Close),
        ] {
            assert_eq!(
                app.notifications_panel_button_at(button.x, button.y),
                Some(action),
                "the drawn button at {button:?} is not the button a click finds"
            );
        }
    }

    fn render_panel_to_string(app: &mut AppState, cols: u16, rows: u16) -> String {
        use ratatui::{backend::TestBackend, Terminal};

        // `compute_view` first, so the state carries the geometry the app has
        // at runtime. The rows are placed from the *hit-test* rects — the same
        // helpers a click goes through — so rendering without a real screen
        // rect draws the chrome and nothing else, which is precisely the drift
        // these assertions exist to catch.
        crate::ui::compute_view(app, Rect::new(0, 0, cols, rows));
        let mut terminal =
            Terminal::new(TestBackend::new(cols, rows)).expect("test terminal should initialize");
        terminal
            .draw(|frame| render_notifications_panel(app, frame, Rect::new(0, 0, cols, rows)))
            .expect("the notification panel should render");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    #[test]
    fn truncation_keeps_the_tail_out_and_marks_the_cut() {
        assert_eq!(truncate_text("short", 10), "short");
        assert_eq!(truncate_text("truncate me", 5), "trun…");
        assert_eq!(truncate_text("truncate me", 1), "…");
        assert_eq!(truncate_text("truncate me", 0), "");
    }
}
