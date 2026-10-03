use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    widgets::Paragraph,
    Frame,
};

use super::widgets::panel_contrast_fg;
use crate::app::AppState;

const MIN_TAB_WIDTH: u16 = 8;
const NEW_TAB_WIDTH: u16 = 3;
const TAB_SCROLL_BUTTON_WIDTH: u16 = 3;

#[derive(Debug, Clone, Default)]
pub(crate) struct TabBarView {
    pub scroll: usize,
    pub tab_hit_areas: Vec<Rect>,
    pub scroll_left_hit_area: Rect,
    pub scroll_right_hit_area: Rect,
    pub new_tab_hit_area: Rect,
}

fn label_cell_width(label: &str) -> u16 {
    (label.chars().count() as u16 + 4).max(MIN_TAB_WIDTH)
}

fn tab_width(tab: &crate::workspace::Tab) -> u16 {
    label_cell_width(&tab.display_name())
}

fn layout_strip_hit_areas(widths: &[u16], area: Rect, scroll: usize) -> Vec<Rect> {
    let mut rects = vec![Rect::default(); widths.len()];
    if area.width == 0 || area.height == 0 {
        return rects;
    }

    let mut x = area.x;
    let right = area.x + area.width;
    for (idx, rect) in rects.iter_mut().enumerate().skip(scroll) {
        if x >= right {
            break;
        }
        let desired = widths[idx];
        let remaining = right.saturating_sub(x);
        let width = desired.min(remaining).max(1);
        *rect = Rect::new(x, area.y, width, 1);
        x = x.saturating_add(width + 1);
    }
    rects
}

fn centered_strip_scroll(widths: &[u16], active: usize, area: Rect) -> usize {
    let mut best_scroll = active;
    let mut best_distance = u16::MAX;
    let viewport_center = area.x.saturating_mul(2).saturating_add(area.width);

    for scroll in 0..=active {
        let rects = layout_strip_hit_areas(widths, area, scroll);
        let Some(active_rect) = rects.get(active).copied() else {
            continue;
        };
        if active_rect.width == 0 {
            continue;
        }

        let active_center = active_rect
            .x
            .saturating_mul(2)
            .saturating_add(active_rect.width);
        let distance = active_center.abs_diff(viewport_center);
        if distance <= best_distance {
            best_distance = distance;
            best_scroll = scroll;
        }
    }

    best_scroll
}

fn trailing_tab_controls_x(tab_hit_areas: &[Rect], fallback_x: u16) -> u16 {
    tab_hit_areas
        .iter()
        .rev()
        .find(|rect| rect.width > 0)
        .map(|rect| rect.x + rect.width)
        .unwrap_or(fallback_x)
}

fn max_strip_scroll(widths: &[u16], area: Rect) -> usize {
    (0..widths.len())
        .find(|&scroll| {
            layout_strip_hit_areas(widths, area, scroll)
                .last()
                .is_some_and(|rect| rect.width > 0)
        })
        .unwrap_or(0)
}

pub(crate) fn compute_tab_bar_view(
    ws: &crate::workspace::Workspace,
    area: Rect,
    current_scroll: usize,
    follow_active: bool,
    mouse_chrome: bool,
) -> TabBarView {
    // #102 part 3: tabs render in `(display_name, tab.number)` order.
    // Widths are computed in display order and the resulting hit rects
    // are permuted back to storage indexing so downstream — mouse
    // handlers, drag targets, tests keyed on `tab_hit_areas[storage_i]`
    // — keep working. Only the on-screen x positions change.
    let display_order = ws.tab_display_order();
    let widths: Vec<u16> = display_order
        .iter()
        .map(|&i| tab_width(&ws.tabs[i]))
        .collect();
    let active_display_pos = display_order
        .iter()
        .position(|&i| i == ws.active_tab)
        .unwrap_or(0);
    let display_view = compute_strip_view(
        &widths,
        active_display_pos,
        area,
        current_scroll,
        follow_active,
        mouse_chrome,
    );
    let mut tab_hit_areas = vec![Rect::default(); ws.tabs.len()];
    for (display_pos, &storage_idx) in display_order.iter().enumerate() {
        if let Some(rect) = display_view.tab_hit_areas.get(display_pos) {
            tab_hit_areas[storage_idx] = *rect;
        }
    }
    TabBarView {
        scroll: display_view.scroll,
        tab_hit_areas,
        scroll_left_hit_area: display_view.scroll_left_hit_area,
        scroll_right_hit_area: display_view.scroll_right_hit_area,
        new_tab_hit_area: display_view.new_tab_hit_area,
    }
}

/// The `<ID> <name>` label of one member-strip slot (#33): the 1-based
/// strip position, then the member's display label — the same branch-first
/// short form the sidebar uses for grouped members.
pub(crate) fn member_strip_label(app: &AppState, pos: usize, ws_idx: usize) -> String {
    let Some(ws) = app.workspaces.get(ws_idx) else {
        return format!("{}", pos + 1);
    };
    let label = super::sidebar::grouped_child_display_label(
        &ws.display_name(),
        ws.branch().as_deref(),
        ws.custom_name.is_some(),
    );
    format!("{} {}", pos + 1, label)
}

/// Tab-bar layout for workspace tab-mode (#33): the strip slots are the
/// active session's project-group members ([`AppState::workspace_strip_members`]),
/// laid out with exactly the tab mechanics (widths, centering, overflow
/// scroll, the `+` button — which already creates a sibling workspace in
/// this mode).
pub(crate) fn compute_member_strip_view(
    app: &AppState,
    area: Rect,
    current_scroll: usize,
    follow_active: bool,
    mouse_chrome: bool,
) -> TabBarView {
    let members = app.workspace_strip_members();
    let widths: Vec<u16> = members
        .iter()
        .enumerate()
        .map(|(pos, ws_idx)| label_cell_width(&member_strip_label(app, pos, *ws_idx)))
        .collect();
    let active = app
        .active
        .and_then(|active| members.iter().position(|ws_idx| *ws_idx == active))
        .unwrap_or(0);
    compute_strip_view(
        &widths,
        active,
        area,
        current_scroll,
        follow_active,
        mouse_chrome,
    )
}

fn compute_strip_view(
    widths: &[u16],
    active: usize,
    area: Rect,
    current_scroll: usize,
    follow_active: bool,
    mouse_chrome: bool,
) -> TabBarView {
    if area.width == 0 || area.height == 0 {
        return TabBarView::default();
    }

    if !mouse_chrome {
        let max_scroll = max_strip_scroll(widths, area);
        let scroll = if follow_active {
            centered_strip_scroll(widths, active, area).min(max_scroll)
        } else {
            current_scroll.min(max_scroll)
        };
        return TabBarView {
            scroll,
            tab_hit_areas: layout_strip_hit_areas(widths, area, scroll),
            scroll_left_hit_area: Rect::default(),
            scroll_right_hit_area: Rect::default(),
            new_tab_hit_area: Rect::default(),
        };
    }

    let area_right = area.x + area.width;
    let all_tabs_area = Rect::new(
        area.x,
        area.y,
        area.width.saturating_sub(NEW_TAB_WIDTH),
        area.height,
    );
    let all_tabs = layout_strip_hit_areas(widths, all_tabs_area, 0);
    let overflow = all_tabs.iter().any(|rect| rect.width == 0);
    if !overflow {
        let new_tab_x = trailing_tab_controls_x(&all_tabs, area.x);
        let new_tab_hit_area = Rect::new(
            new_tab_x,
            area.y,
            area_right.saturating_sub(new_tab_x).min(NEW_TAB_WIDTH),
            1,
        );
        return TabBarView {
            scroll: 0,
            tab_hit_areas: all_tabs,
            scroll_left_hit_area: Rect::default(),
            scroll_right_hit_area: Rect::default(),
            new_tab_hit_area,
        };
    }

    let left_hit_area = Rect::new(area.x, area.y, TAB_SCROLL_BUTTON_WIDTH.min(area.width), 1);
    let tab_area_x = left_hit_area.x + left_hit_area.width;
    let reserved_trailing_width = NEW_TAB_WIDTH.saturating_add(TAB_SCROLL_BUTTON_WIDTH);
    let tab_area_right = area_right.saturating_sub(reserved_trailing_width);
    let tab_area = Rect::new(
        tab_area_x,
        area.y,
        tab_area_right.saturating_sub(tab_area_x),
        area.height,
    );

    let max_scroll = max_strip_scroll(widths, tab_area);
    let scroll = if follow_active {
        centered_strip_scroll(widths, active, tab_area).min(max_scroll)
    } else {
        current_scroll.min(max_scroll)
    };
    let tab_hit_areas = layout_strip_hit_areas(widths, tab_area, scroll);
    let trailing_x = trailing_tab_controls_x(&tab_hit_areas, tab_area_x).min(tab_area_right);
    let right_hit_area = Rect::new(
        trailing_x,
        area.y,
        area_right
            .saturating_sub(trailing_x)
            .min(TAB_SCROLL_BUTTON_WIDTH),
        1,
    );
    let new_tab_x = right_hit_area.x + right_hit_area.width;
    let new_tab_hit_area = Rect::new(
        new_tab_x,
        area.y,
        area_right.saturating_sub(new_tab_x).min(NEW_TAB_WIDTH),
        1,
    );

    TabBarView {
        scroll,
        tab_hit_areas,
        scroll_left_hit_area: left_hit_area,
        scroll_right_hit_area: right_hit_area,
        new_tab_hit_area,
    }
}

pub(super) fn render_tab_bar(app: &AppState, frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let Some(active_ws_idx) = app.active else {
        return;
    };
    let Some(ws) = app.workspaces.get(active_ws_idx) else {
        return;
    };

    // #33: workspace tab-mode repurposes the strip as the active session's
    // member switcher. Tabs mode renders exactly as before.
    if app.tab_strip_shows_members() {
        render_member_strip(app, frame, area, active_ws_idx);
        return;
    }

    let p = &app.palette;

    frame.render_widget(
        Paragraph::new(" ".repeat(area.width as usize)).style(Style::default().bg(p.panel_bg)),
        area,
    );

    let first_visible_idx = app
        .view
        .tab_hit_areas
        .iter()
        .enumerate()
        .find(|(_, rect)| rect.width > 0)
        .map(|(idx, _)| idx);
    let last_visible_idx = app
        .view
        .tab_hit_areas
        .iter()
        .enumerate()
        .rev()
        .find(|(_, rect)| rect.width > 0)
        .map(|(idx, _)| idx);
    let can_scroll_left = app.view.tab_scroll_left_hit_area.width > 0 && app.tab_scroll > 0;
    let can_scroll_right = app.view.tab_scroll_right_hit_area.width > 0
        && last_visible_idx.is_some_and(|idx| idx + 1 < ws.tabs.len());

    if app.mouse_capture && app.view.tab_scroll_left_hit_area.width > 0 {
        let style = if can_scroll_left {
            Style::default().fg(p.overlay1).bg(p.surface0)
        } else {
            Style::default()
                .fg(p.overlay0)
                .bg(p.surface0)
                .add_modifier(Modifier::DIM)
        };
        frame.render_widget(
            Paragraph::new(" < ").style(style),
            app.view.tab_scroll_left_hit_area,
        );
    }

    if app.mouse_capture && app.view.tab_scroll_right_hit_area.width > 0 {
        let style = if can_scroll_right {
            Style::default().fg(p.overlay1).bg(p.surface0)
        } else {
            Style::default()
                .fg(p.overlay0)
                .bg(p.surface0)
                .add_modifier(Modifier::DIM)
        };
        frame.render_widget(
            Paragraph::new(" > ").style(style),
            app.view.tab_scroll_right_hit_area,
        );
    }

    for (idx, tab) in ws.tabs.iter().enumerate() {
        let Some(rect) = app.view.tab_hit_areas.get(idx).copied() else {
            break;
        };
        if rect.width == 0 {
            continue;
        }
        let active = idx == ws.active_tab;
        // #394: an unselected tab wears the state tint of its own panes — the
        // same signal, palette and code shape the workspace-mode strip already
        // uses, so a blocked agent in a background tab is visible in the strip
        // instead of reading as an idle numbered tab. `None` (no live agent in
        // the tab) keeps the neutral fg, and the background stays the neutral
        // `surface0`: state whispers here exactly as it does there (#42).
        let tint = (!active)
            .then(|| tab_state_tint(app, active_ws_idx, idx, p))
            .flatten();
        let style = if active {
            let base = Style::default().fg(panel_contrast_fg(p)).bg(p.accent);
            if tab.is_auto_named() {
                base.add_modifier(Modifier::DIM)
            } else {
                base.add_modifier(Modifier::BOLD)
            }
        } else {
            let base = Style::default()
                .fg(tint.unwrap_or(if tab.is_auto_named() {
                    p.overlay0
                } else {
                    p.overlay1
                }))
                .bg(p.surface0);
            if tab.is_auto_named() {
                base.add_modifier(Modifier::DIM)
            } else {
                base
            }
        };
        let width = rect.width as usize;
        let name = tab.display_name();
        let text = format!(" {:width$}", name, width = width.saturating_sub(1));
        frame.render_widget(Paragraph::new(text).style(style), rect);
    }

    if app.mouse_capture && app.view.new_tab_hit_area.width > 0 {
        frame.render_widget(
            Paragraph::new(" + ").style(Style::default().fg(p.overlay1)),
            app.view.new_tab_hit_area,
        );
    }

    if first_visible_idx.is_some_and(|idx| idx > 0) {
        let x = if app.mouse_capture && app.view.tab_scroll_left_hit_area.width > 0 {
            app.view.tab_scroll_left_hit_area.x + app.view.tab_scroll_left_hit_area.width
        } else {
            area.x
        };
        if x < area.x + area.width {
            frame.buffer_mut()[(x, area.y)]
                .set_symbol("…")
                .set_style(Style::default().fg(p.overlay0));
        }
    }
    if last_visible_idx.is_some_and(|idx| idx + 1 < ws.tabs.len()) {
        let x = if app.mouse_capture && app.view.tab_scroll_right_hit_area.width > 0 {
            app.view.tab_scroll_right_hit_area.x.saturating_sub(1)
        } else {
            area.x + area.width.saturating_sub(1)
        };
        if x >= area.x && x < area.x + area.width {
            frame.buffer_mut()[(x, area.y)]
                .set_symbol("…")
                .set_style(Style::default().fg(p.overlay0));
        }
    }
}

/// The state tint of a strip slot that is not the selected one (#33/#42 for
/// member slots, #394 for tabs): the head (worst class) of that slot's own
/// pane-state join, or `None` when nothing in it carries a live agent signal.
///
/// One derivation for both strips, over the shared severity ladder
/// ([`join_states`] + [`StateClass::of`]) — the tabs strip and
/// [`render_member_strip`] must never disagree about the same pane (#394).
fn slot_state_tint(
    states: impl IntoIterator<Item = (crate::detect::AgentState, bool)>,
    p: &crate::app::state::Palette,
) -> Option<ratatui::style::Color> {
    use super::state_signal::{join_states, StateClass};
    join_states(
        states
            .into_iter()
            .map(|(state, seen)| StateClass::of(state, seen)),
    )
    .head()
    .map(|class| class.color(p))
}

/// The state tint of an unselected member tab (#33/#42): the head (worst
/// class) of the member's own pane-state join, muted when nothing is live.
fn member_state_tint(
    app: &AppState,
    ws_idx: usize,
    p: &crate::app::state::Palette,
) -> ratatui::style::Color {
    app.workspaces
        .get(ws_idx)
        .and_then(|ws| slot_state_tint(ws.pane_states(&app.terminals), p))
        .unwrap_or(p.overlay1)
}

/// The state tint of an unselected tab in `tab_mode = tabs` (#394): the same
/// signal, over that tab's own panes. `None` leaves the tab on its neutral fg,
/// so a strip with no agents in it looks exactly as it did before — the tint
/// is additive, not a repaint.
fn tab_state_tint(
    app: &AppState,
    ws_idx: usize,
    tab_idx: usize,
    p: &crate::app::state::Palette,
) -> Option<ratatui::style::Color> {
    app.workspaces
        .get(ws_idx)?
        .tabs
        .get(tab_idx)
        .and_then(|tab| slot_state_tint(tab.pane_states(&app.terminals), p))
}

/// Workspace tab-mode (#33): the strip renders the active session's
/// project-group members as `<ID> <name>` tabs. Selected = accent (focus
/// is always accent, #43); unselected = the member's state-join head as
/// DIM text tint only (#42) — no state backgrounds or chips: state
/// whispers, focus speaks. Scroll buttons, overflow ellipses, and the `+`
/// entry (a sibling workspace in this mode) keep the tab bar's mechanics.
fn render_member_strip(app: &AppState, frame: &mut Frame, area: Rect, active_ws_idx: usize) {
    let p = &app.palette;

    frame.render_widget(
        Paragraph::new(" ".repeat(area.width as usize)).style(Style::default().bg(p.panel_bg)),
        area,
    );

    let members = app.workspace_strip_members();
    let first_visible_idx = app
        .view
        .tab_hit_areas
        .iter()
        .enumerate()
        .find(|(_, rect)| rect.width > 0)
        .map(|(idx, _)| idx);
    let last_visible_idx = app
        .view
        .tab_hit_areas
        .iter()
        .enumerate()
        .rev()
        .find(|(_, rect)| rect.width > 0)
        .map(|(idx, _)| idx);
    let can_scroll_left = app.view.tab_scroll_left_hit_area.width > 0 && app.tab_scroll > 0;
    let can_scroll_right = app.view.tab_scroll_right_hit_area.width > 0
        && last_visible_idx.is_some_and(|idx| idx + 1 < members.len());

    if app.mouse_capture && app.view.tab_scroll_left_hit_area.width > 0 {
        let style = if can_scroll_left {
            Style::default().fg(p.overlay1).bg(p.surface0)
        } else {
            Style::default()
                .fg(p.overlay0)
                .bg(p.surface0)
                .add_modifier(Modifier::DIM)
        };
        frame.render_widget(
            Paragraph::new(" < ").style(style),
            app.view.tab_scroll_left_hit_area,
        );
    }

    if app.mouse_capture && app.view.tab_scroll_right_hit_area.width > 0 {
        let style = if can_scroll_right {
            Style::default().fg(p.overlay1).bg(p.surface0)
        } else {
            Style::default()
                .fg(p.overlay0)
                .bg(p.surface0)
                .add_modifier(Modifier::DIM)
        };
        frame.render_widget(
            Paragraph::new(" > ").style(style),
            app.view.tab_scroll_right_hit_area,
        );
    }

    for (pos, ws_idx) in members.iter().enumerate() {
        let Some(rect) = app.view.tab_hit_areas.get(pos).copied() else {
            break;
        };
        if rect.width == 0 {
            continue;
        }
        let selected = *ws_idx == active_ws_idx;
        let style = if selected {
            Style::default()
                .fg(panel_contrast_fg(p))
                .bg(p.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(member_state_tint(app, *ws_idx, p))
                .bg(p.surface0)
                .add_modifier(Modifier::DIM)
        };
        let width = rect.width as usize;
        let name = member_strip_label(app, pos, *ws_idx);
        let text = format!(" {:width$}", name, width = width.saturating_sub(1));
        frame.render_widget(Paragraph::new(text).style(style), rect);
    }

    if app.mouse_capture && app.view.new_tab_hit_area.width > 0 {
        frame.render_widget(
            Paragraph::new(" + ").style(Style::default().fg(p.overlay1)),
            app.view.new_tab_hit_area,
        );
    }

    if first_visible_idx.is_some_and(|idx| idx > 0) {
        let x = if app.mouse_capture && app.view.tab_scroll_left_hit_area.width > 0 {
            app.view.tab_scroll_left_hit_area.x + app.view.tab_scroll_left_hit_area.width
        } else {
            area.x
        };
        if x < area.x + area.width {
            frame.buffer_mut()[(x, area.y)]
                .set_symbol("…")
                .set_style(Style::default().fg(p.overlay0));
        }
    }
    if last_visible_idx.is_some_and(|idx| idx + 1 < members.len()) {
        let x = if app.mouse_capture && app.view.tab_scroll_right_hit_area.width > 0 {
            app.view.tab_scroll_right_hit_area.x.saturating_sub(1)
        } else {
            area.x + area.width.saturating_sub(1)
        };
        if x >= area.x && x < area.x + area.width {
            frame.buffer_mut()[(x, area.y)]
                .set_symbol("…")
                .set_style(Style::default().fg(p.overlay0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;
    use ratatui::{backend::TestBackend, Terminal};

    fn member(state: &mut crate::app::state::AppState, ws_idx: usize, linked: bool) {
        state.workspaces[ws_idx].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "grp".into(),
            label: "flock".into(),
            repo_root: "/repo/flock".into(),
            checkout_path: format!("/repo/ws-{ws_idx}").into(),
            is_linked_worktree: linked,
        });
    }

    fn strip_app() -> crate::app::state::AppState {
        let mut app = crate::app::state::AppState::test_new();
        app.workspaces = vec![
            Workspace::test_new("main"),
            Workspace::test_new("keys"),
            Workspace::test_new("forest"),
        ];
        member(&mut app, 0, false);
        member(&mut app, 1, true);
        member(&mut app, 2, true);
        // Pin equal branches so the strip's branch-alphabetical member sort
        // ties and falls back to insertion (ws_idx) order. Without this the
        // order is non-hermetic: `Workspace::test_new` seeds `cached_git_branch`
        // from the ambient cwd's git branch, so a normal checkout (all three
        // share the branch → tie → ws_idx order) and a detached-HEAD CI
        // checkout (branch() is None → sort falls back to display_name →
        // alphabetical) render the members in different orders.
        for ws in &mut app.workspaces {
            ws.cached_git_branch = Some("section".into());
        }
        app.ensure_test_terminals();
        app.active = Some(0);
        app.selected = 0;
        app.mode = crate::app::Mode::Terminal;
        app.tab_mode = crate::config::TabModeConfig::Workspace;
        app
    }

    fn set_pane_state(
        app: &mut crate::app::state::AppState,
        ws_idx: usize,
        state: crate::detect::AgentState,
    ) {
        let pane = app.workspaces[ws_idx].tabs[0]
            .panes
            .keys()
            .next()
            .copied()
            .unwrap();
        let tid = app.workspaces[ws_idx].terminal_id(pane).unwrap().clone();
        app.terminals.get_mut(&tid).unwrap().state = state;
    }

    fn render_strip(app: &mut crate::app::state::AppState, area: Rect) -> ratatui::buffer::Buffer {
        let view = compute_member_strip_view(app, area, 0, true, app.mouse_capture);
        app.view.tab_bar_rect = area;
        app.view.tab_hit_areas = view.tab_hit_areas;
        app.view.tab_scroll_left_hit_area = view.scroll_left_hit_area;
        app.view.tab_scroll_right_hit_area = view.scroll_right_hit_area;
        app.view.new_tab_hit_area = view.new_tab_hit_area;
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))
            .expect("test terminal should initialize");
        terminal
            .draw(|frame| render_tab_bar(app, frame, area))
            .expect("tab bar should render");
        terminal.backend().buffer().clone()
    }

    fn row_text(buffer: &ratatui::buffer::Buffer, area: Rect) -> String {
        (area.x..area.x + area.width)
            .map(|x| buffer[(x, area.y)].symbol().to_string())
            .collect()
    }

    /// #394 fixture: one workspace, three tabs, `tab_mode = tabs` — the
    /// strip's normal shape. The first tab is auto-named (its number) and is
    /// the active one, the second carries a custom name.
    fn tabs_app() -> crate::app::state::AppState {
        let mut app = crate::app::state::AppState::test_new();
        let mut ws = Workspace::test_new("main");
        ws.test_add_tab(Some("logs"));
        ws.test_add_tab(None);
        app.workspaces = vec![ws];
        app.ensure_test_terminals();
        app.active = Some(0);
        app.selected = 0;
        app.mode = crate::app::Mode::Terminal;
        app.tab_mode = crate::config::TabModeConfig::Tabs;
        app
    }

    fn set_tab_pane_state(
        app: &mut crate::app::state::AppState,
        tab_idx: usize,
        state: crate::detect::AgentState,
    ) {
        let pane = app.workspaces[0].tabs[tab_idx].root_pane;
        let tid = app.workspaces[0].tabs[tab_idx].panes[&pane]
            .attached_terminal_id
            .clone();
        app.terminals.get_mut(&tid).unwrap().state = state;
    }

    fn render_tabs(app: &mut crate::app::state::AppState, area: Rect) -> ratatui::buffer::Buffer {
        let view = compute_tab_bar_view(
            &app.workspaces[0],
            area,
            app.tab_scroll,
            app.tab_scroll_follow_active,
            app.mouse_capture,
        );
        app.tab_scroll = view.scroll;
        app.view.tab_bar_rect = area;
        app.view.tab_hit_areas = view.tab_hit_areas;
        app.view.tab_scroll_left_hit_area = view.scroll_left_hit_area;
        app.view.tab_scroll_right_hit_area = view.scroll_right_hit_area;
        app.view.new_tab_hit_area = view.new_tab_hit_area;
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))
            .expect("test terminal should initialize");
        terminal
            .draw(|frame| render_tab_bar(app, frame, area))
            .expect("tab bar should render");
        terminal.backend().buffer().clone()
    }

    /// #394 — `tab_mode = tabs` styled a tab on active/auto-named only, so a
    /// blocked agent sitting in a background tab was invisible: the strip
    /// showed a number, tinted exactly like an idle one. Asserted on the
    /// RENDERED cells, because that is the surface that was wrong —
    /// `render_member_strip`'s equivalent tint was already working, so no
    /// test over `member_state_tint` alone could have shown the tabs strip
    /// missing it.
    #[test]
    fn tab_strip_tints_an_unselected_tab_by_its_panes_state() {
        let mut app = tabs_app();
        set_tab_pane_state(&mut app, 1, crate::detect::AgentState::Blocked);

        let area = Rect::new(0, 0, 48, 1);
        let buffer = render_tabs(&mut app, area);
        let p = &app.palette;

        // The ACTIVE tab stays accent — focus speaks (#43); only unselected
        // slots carry state, exactly as in workspace mode.
        let active = app.view.tab_hit_areas[0];
        assert_eq!(buffer[(active.x + 1, active.y)].style().bg, Some(p.accent));

        // The background tab holding the blocked agent reads red, over the
        // same neutral surface the member strip uses (no state background).
        let blocked = app.view.tab_hit_areas[1];
        let cell = &buffer[(blocked.x + 1, blocked.y)];
        assert_eq!(cell.style().fg, Some(p.red));
        assert_eq!(cell.style().bg, Some(p.surface0));

        // An unselected tab with no live agent keeps the neutral fg it always
        // had — the tint is additive, not a repaint of the strip.
        let idle = app.view.tab_hit_areas[2];
        assert_eq!(
            buffer[(idle.x + 1, idle.y)].style().fg,
            Some(p.overlay0),
            "an auto-named tab with no agent signal is unchanged"
        );
    }

    #[test]
    fn tab_strip_tints_by_the_worst_state_in_the_tab() {
        let mut app = tabs_app();
        // Tab 1 holds a working pane; the sibling tab's blocked pane must not
        // bleed into it.
        set_tab_pane_state(&mut app, 1, crate::detect::AgentState::Working);
        set_tab_pane_state(&mut app, 2, crate::detect::AgentState::Blocked);

        let area = Rect::new(0, 0, 48, 1);
        let buffer = render_tabs(&mut app, area);
        let working = app.view.tab_hit_areas[1];
        assert_eq!(
            buffer[(working.x + 1, working.y)].style().fg,
            Some(app.palette.yellow)
        );
        let blocked = app.view.tab_hit_areas[2];
        assert_eq!(
            buffer[(blocked.x + 1, blocked.y)].style().fg,
            Some(app.palette.red)
        );
    }

    /// #33 must be untouched: workspace tab-mode still renders its member
    /// slots — tinted by the member's aggregate, labelled `<ID> <name>` —
    /// not tabs, even where the member has tabs of its own.
    #[test]
    fn workspace_tab_mode_still_renders_the_member_strip() {
        let mut app = strip_app();
        app.workspaces[1].test_add_tab(Some("logs"));
        app.ensure_test_terminals();
        set_pane_state(&mut app, 1, crate::detect::AgentState::Blocked);

        let area = Rect::new(0, 0, 48, 1);
        let buffer = render_strip(&mut app, area);
        let text = row_text(&buffer, area);
        assert!(text.contains("2 keys"), "{text:?}");
        assert!(
            !text.contains("logs"),
            "member slots are not tabs: {text:?}"
        );
        assert_eq!(app.view.tab_hit_areas.len(), 3, "one slot per member");
        let blocked = app.view.tab_hit_areas[1];
        assert_eq!(
            buffer[(blocked.x + 1, blocked.y)].style().fg,
            Some(app.palette.red)
        );
    }

    /// #33 — workspace tab-mode: the strip renders the session's members as
    /// `<ID> <name>` tabs; selected = accent, unselected = the member's
    /// state-join head as DIM text over the neutral tab surface.
    #[test]
    fn member_strip_renders_id_name_labels_with_accent_and_state_tint() {
        let mut app = strip_app();
        set_pane_state(&mut app, 1, crate::detect::AgentState::Working);

        let area = Rect::new(0, 0, 40, 1);
        let buffer = render_strip(&mut app, area);
        let p = &app.palette;

        let text = row_text(&buffer, area);
        assert!(text.contains("1 main"), "{text:?}");
        assert!(text.contains("2 keys"), "{text:?}");
        assert!(text.contains("3 forest"), "{text:?}");

        // The selected member (the active workspace) carries the accent.
        let selected = app.view.tab_hit_areas[0];
        let cell = &buffer[(selected.x + 1, selected.y)];
        assert_eq!(cell.style().bg, Some(p.accent));

        // The working member's tab is yellow DIM text — no state bg fill.
        let working = app.view.tab_hit_areas[1];
        let cell = &buffer[(working.x + 1, working.y)];
        assert_eq!(cell.style().fg, Some(p.yellow));
        assert_eq!(cell.style().bg, Some(p.surface0));
        assert!(cell.style().add_modifier.contains(Modifier::DIM));

        // The idle (no live signal) member tints muted.
        let idle = app.view.tab_hit_areas[2];
        let cell = &buffer[(idle.x + 1, idle.y)];
        assert_eq!(cell.style().fg, Some(p.overlay1));
    }

    /// #33 — auto-named members label by their short branch, like the
    /// sidebar's grouped rows; the ID stays the strip position.
    #[test]
    fn member_strip_labels_use_branch_for_auto_named_members() {
        let mut app = strip_app();
        app.workspaces[1].custom_name = None;
        app.workspaces[1].cached_git_branch = Some("worktree/issue-33".into());

        assert_eq!(member_strip_label(&app, 1, 1), "2 issue-33");
    }

    /// #33 — strip hit-areas lay out exactly like tabs: one slot per
    /// member, and clicking resolves through the same tab_at geometry.
    #[test]
    fn member_strip_hit_areas_cover_every_member() {
        let mut app = strip_app();
        let area = Rect::new(0, 0, 60, 1);
        let _ = render_strip(&mut app, area);

        assert_eq!(app.view.tab_hit_areas.len(), 3);
        assert!(app.view.tab_hit_areas.iter().all(|rect| rect.width > 0));

        // The member switch reuses switch_workspace via switch_tab — the
        // click path's entry point (mouse resolves the slot via tab_at over
        // these same hit-areas).
        app.switch_tab(2);
        assert_eq!(app.active, Some(2));
    }
}
