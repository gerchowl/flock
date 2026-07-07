use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
    Frame,
};

use super::widgets::{
    action_button_row_rects, centered_popup_rect, panel_contrast_fg, render_action_button,
    render_modal_header, render_modal_shell, render_panel_shell, ActionButtonSpec,
};
use crate::app::{state::WorktreeOpenState, AppState, Mode};

fn truncate_text(text: &str, max_width: usize) -> String {
    let len = text.chars().count();
    if len <= max_width {
        return text.to_string();
    }
    if max_width <= 1 {
        return "…".into();
    }
    format!(
        "{}…",
        text.chars()
            .take(max_width.saturating_sub(1))
            .collect::<String>()
    )
}

pub(crate) fn rename_button_rects(inner: Rect) -> (Rect, Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "save",
            },
            ActionButtonSpec {
                hint: Some("^c"),
                label: "clear",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        3,
    );
    (rects[0], rects[1], rects[2])
}

pub(super) fn render_rename_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    super::dim_background(frame, area);

    let title = match app.mode {
        Mode::RenameWorkspace => "rename workspace",
        Mode::RenameTab if app.creating_new_tab => "new tab",
        Mode::RenameTab => "rename tab",
        Mode::RenamePane => "rename pane",
        _ => return,
    };

    let Some(inner) = render_modal_shell(frame, area, 56, 7, &app.palette) else {
        return;
    };
    if inner.height < 4 {
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas::<5>(inner);

    render_modal_header(frame, rows[0], title, &app.palette);

    let input_rect = Rect::new(rows[2].x, rows[2].y, rows[2].width, 1);
    frame.render_widget(Clear, input_rect);
    frame.render_widget(
        Paragraph::new(format!(" {}█", app.name_input)).style(
            Style::default()
                .fg(app.palette.text)
                .bg(app.palette.surface0),
        ),
        input_rect,
    );

    let (save_rect, clear_rect, cancel_rect) = rename_button_rects(inner);

    render_action_button(
        frame,
        save_rect,
        Some("↵"),
        "save",
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.accent)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        clear_rect,
        Some("^c"),
        "clear",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

pub(crate) fn new_linked_worktree_inner_rect(area: Rect) -> Option<Rect> {
    centered_popup_rect(area, 68, 10).map(|popup| {
        Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        )
    })
}

pub(crate) fn new_linked_worktree_button_rects(inner: Rect) -> (Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "create and open",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (rects[0], rects[1])
}

pub(crate) fn remove_worktree_popup_rect(area: Rect) -> Option<Rect> {
    centered_popup_rect(area, 72, 10)
}

pub(crate) fn remove_worktree_button_rects(inner: Rect, force_confirmation: bool) -> (Rect, Rect) {
    let primary_label = if force_confirmation {
        "delete anyway"
    } else {
        "remove"
    };
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: primary_label,
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (rects[0], rects[1])
}

pub(crate) fn open_existing_worktree_inner_rect(area: Rect, entry_count: usize) -> Option<Rect> {
    let height = (entry_count as u16)
        .saturating_mul(2)
        .saturating_add(7)
        .clamp(12, 26);
    centered_popup_rect(area, 96, height).map(|popup| {
        Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        )
    })
}

pub(crate) fn open_existing_worktree_max_visible_rows(inner: Rect) -> usize {
    usize::from(inner.height.saturating_sub(5) / 2)
}

pub(crate) fn open_existing_worktree_visible_start(
    open: &WorktreeOpenState,
    max_rows: usize,
) -> usize {
    let filtered = open.filtered_indices();
    let selected = open.selected_entry_index().unwrap_or(open.selected);
    let selected_pos = filtered
        .iter()
        .position(|idx| *idx == selected)
        .unwrap_or(0);
    selected_pos.saturating_sub(max_rows.saturating_sub(1))
}

pub(crate) fn open_existing_worktree_button_rects(inner: Rect) -> (Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "open",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (rects[0], rects[1])
}

pub(super) fn render_new_linked_worktree_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let Some(create) = app.worktree_create.as_ref() else {
        return;
    };

    super::dim_background(frame, area);
    let Some(inner) = render_modal_shell(frame, area, 68, 10, &app.palette) else {
        return;
    };
    if inner.height < 7 {
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas::<8>(inner);

    let header = if app
        .worktree_create
        .as_ref()
        .is_some_and(|create| create.branch_plan.is_some())
    {
        "branch session into new worktree"
    } else {
        "new worktree"
    };
    render_modal_header(frame, rows[0], header, &app.palette);

    frame.render_widget(
        Paragraph::new(" branch").style(Style::default().fg(app.palette.overlay0)),
        rows[1],
    );
    let input_rect = Rect::new(rows[2].x, rows[2].y, rows[2].width, 1);
    frame.render_widget(Clear, input_rect);
    frame.render_widget(
        Paragraph::new(format!(" {}█", app.name_input)).style(
            Style::default()
                .fg(app.palette.text)
                .bg(app.palette.surface0),
        ),
        input_rect,
    );

    let checkout = create.checkout_path.display().to_string();
    frame.render_widget(
        Paragraph::new(" checkout").style(Style::default().fg(app.palette.overlay0)),
        rows[3],
    );
    frame.render_widget(
        Paragraph::new(format!(" {checkout}")).style(Style::default().fg(app.palette.subtext0)),
        rows[4],
    );

    if create.creating {
        frame.render_widget(
            Paragraph::new(" creating…").style(Style::default().fg(app.palette.overlay0)),
            rows[5],
        );
    } else if let Some(error) = &create.error {
        frame.render_widget(
            Paragraph::new(format!(" {error}")).style(Style::default().fg(app.palette.red)),
            rows[5],
        );
    }

    let (create_rect, cancel_rect) = new_linked_worktree_button_rects(inner);
    render_action_button(
        frame,
        create_rect,
        Some("↵"),
        "create and open",
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.accent)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

pub(super) fn render_remove_worktree_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let Some(remove) = app.worktree_remove.as_ref() else {
        return;
    };

    super::dim_background(frame, area);
    let Some(popup) = remove_worktree_popup_rect(area) else {
        return;
    };
    let Some(inner) = render_panel_shell(frame, popup, app.palette.red, app.palette.panel_bg)
    else {
        return;
    };

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas::<8>(inner);

    let title = if remove.delete_branch {
        " kill worktree & branch?"
    } else {
        " delete worktree checkout?"
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            title,
            Style::default()
                .fg(app.palette.red)
                .add_modifier(Modifier::BOLD),
        )])),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(" This removes the checkout folder:")
            .style(Style::default().fg(app.palette.overlay0)),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(format!(" {}", remove.path.display()))
            .style(Style::default().fg(app.palette.text)),
        rows[2],
    );
    if remove.branch_protected {
        // #121: the "& branch" path landed on the default/protected branch.
        // The checkout may still be removed; the branch is always kept.
        frame.render_widget(
            Paragraph::new(format!(
                " ✓ {} is a protected branch — kept (checkout only).",
                remove.branch.as_deref().unwrap_or("default")
            ))
            .style(Style::default().fg(app.palette.green)),
            rows[3],
        );
    } else if remove.delete_branch {
        let (gate_line, gate_style) = match &remove.merge_gate {
            None => (
                " checking merge status…".to_string(),
                Style::default().fg(app.palette.overlay0),
            ),
            Some(crate::worktree::WorktreeMergeGate::Merged { evidence }) => (
                format!(
                    " ✓ {evidence} — branch {} will be deleted too.",
                    remove.branch.as_deref().unwrap_or("?")
                ),
                Style::default().fg(app.palette.green),
            ),
            Some(crate::worktree::WorktreeMergeGate::NotMerged) if remove.gate_timed_out => (
                " ⏱ merge status unknown (timed out) — checkout only; the branch is kept."
                    .to_string(),
                Style::default().fg(app.palette.yellow),
            ),
            Some(crate::worktree::WorktreeMergeGate::NotMerged) => (
                " ✗ no merge evidence — checkout only; the branch is kept.".to_string(),
                Style::default().fg(app.palette.peach),
            ),
        };
        frame.render_widget(Paragraph::new(gate_line).style(gate_style), rows[3]);
    } else {
        frame.render_widget(
            Paragraph::new(" The branch is not deleted. The Flock workspace will close.")
                .style(Style::default().fg(app.palette.overlay0)),
            rows[3],
        );
    }
    if remove.force_confirmation {
        frame.render_widget(
            Paragraph::new(" Dirty or untracked files will be permanently deleted.")
                .style(Style::default().fg(app.palette.red)),
            rows[4],
        );
    }
    if remove.removing {
        frame.render_widget(
            Paragraph::new(" removing…").style(Style::default().fg(app.palette.overlay0)),
            rows[5],
        );
    } else if let Some(error) = &remove.error {
        frame.render_widget(
            Paragraph::new(format!(" {error}")).style(Style::default().fg(app.palette.red)),
            rows[5],
        );
    }

    let (remove_rect, cancel_rect) = remove_worktree_button_rects(inner, remove.force_confirmation);
    let remove_label = if remove.force_confirmation {
        "delete anyway"
    } else {
        "remove"
    };
    render_action_button(
        frame,
        remove_rect,
        Some("↵"),
        remove_label,
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.red)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

/// The fleet-wide kill sweep dialog (#81): a per-worktree dry-run plan + counts,
/// the force toggle, and execute/cancel. Confirm is held until every linked
/// row's merge gate has resolved.
pub(super) fn render_kill_all_worktrees_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    use crate::app::state::WorktreeKillRowStatus;
    use crate::worktree::{KillAction, KillTier};

    let Some(kill_all) = app.worktree_kill_all.as_ref() else {
        return;
    };
    super::dim_background(frame, area);

    let force = kill_all.force_dirty;
    let resolving = kill_all.resolving();

    let mut lines: Vec<Line> = Vec::new();
    let (mut n_kill, mut n_checkout, mut n_close, mut n_skip) = (0usize, 0usize, 0usize, 0usize);
    for row in &kill_all.rows {
        let (verb, color) = match crate::worktree::planned_action(row.tier, force) {
            KillAction::KillBranch { dirty } => {
                n_kill += 1;
                (
                    if dirty {
                        "kill + branch (dirty!)"
                    } else {
                        "kill + branch"
                    },
                    app.palette.red,
                )
            }
            KillAction::CheckoutOnly => {
                n_checkout += 1;
                ("checkout only", app.palette.peach)
            }
            KillAction::ClosePane => {
                n_close += 1;
                ("close pane", app.palette.blue)
            }
            KillAction::Skip => {
                n_skip += 1;
                let reason = match row.tier {
                    KillTier::SkipUnmergedDirty => "skip — unmerged + dirty",
                    KillTier::SkipMainDirty => "skip — main, dirty",
                    KillTier::SkipAgent => "skip — agent busy",
                    _ => "skip",
                };
                (reason, app.palette.overlay0)
            }
        };
        let status = match &row.status {
            WorktreeKillRowStatus::Removing => "  …",
            WorktreeKillRowStatus::Done => "  ✓",
            WorktreeKillRowStatus::Error(_) => "  ✗err",
            WorktreeKillRowStatus::Pending => "",
        };
        let gate = if !row.checkout_is_main() && row.merge_gate.is_none() {
            "  ⏳"
        } else {
            ""
        };
        let label = truncate_text(&row.label, 30);
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {label:<30} "),
                Style::default().fg(app.palette.text),
            ),
            Span::styled(verb, Style::default().fg(color)),
            Span::styled(
                format!("{gate}{status}"),
                Style::default().fg(app.palette.overlay0),
            ),
        ]));
    }
    const MAX_ROWS: usize = 14;
    let hidden = lines.len().saturating_sub(MAX_ROWS);
    lines.truncate(MAX_ROWS);
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!(" … and {hidden} more"),
            Style::default().fg(app.palette.overlay0),
        )));
    }

    let body_h = lines.len().max(1) as u16;
    let popup_h = (body_h + 6).min(area.height.saturating_sub(2));
    let Some(popup) = centered_popup_rect(area, 80, popup_h) else {
        return;
    };
    let Some(inner) = render_panel_shell(frame, popup, app.palette.red, app.palette.panel_bg)
    else {
        return;
    };

    let layout = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas::<5>(inner);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " kill all worktrees",
            Style::default()
                .fg(app.palette.red)
                .add_modifier(Modifier::BOLD),
        ))),
        layout[0],
    );

    let summary = if resolving {
        " resolving merge status…".to_string()
    } else {
        format!(
            " {n_kill} kill · {n_checkout} checkout-only · {n_close} close pane · {n_skip} skipped"
        )
    };
    frame.render_widget(
        Paragraph::new(summary).style(Style::default().fg(app.palette.subtext0)),
        layout[1],
    );

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().fg(app.palette.text)),
        layout[2],
    );

    let hint = if force {
        " [f] force ON — unmerged+dirty becomes checkout-only"
    } else {
        " [f] include unmerged+dirty as checkout-only"
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(if force {
            app.palette.peach
        } else {
            app.palette.overlay0
        })),
        layout[3],
    );

    let exec_label = if kill_all.executing {
        "executing…"
    } else if resolving {
        "resolving…"
    } else {
        "execute"
    };
    let buttons = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .areas::<2>(layout[4]);
    render_action_button(
        frame,
        buttons[0],
        Some("↵"),
        exec_label,
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.red)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        buttons[1],
        Some("esc"),
        "cancel",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

pub(super) fn render_open_existing_worktree_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let Some(open) = app.worktree_open.as_ref() else {
        return;
    };

    super::dim_background(frame, area);
    let height = (open.entries.len() as u16)
        .saturating_mul(2)
        .saturating_add(7)
        .clamp(12, 26);
    let Some(inner) = render_modal_shell(frame, area, 96, height, &app.palette) else {
        return;
    };
    if inner.height < 8 {
        return;
    }

    render_modal_header(
        frame,
        Rect::new(inner.x, inner.y, inner.width, 1),
        "open worktree",
        &app.palette,
    );
    render_open_worktree_search(
        app,
        frame,
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
        open,
    );
    frame.render_widget(
        Paragraph::new("─".repeat(inner.width as usize))
            .style(Style::default().fg(app.palette.surface1)),
        Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1),
    );

    let filtered = open.filtered_indices();
    let max_rows = open_existing_worktree_max_visible_rows(inner);
    let start = open_existing_worktree_visible_start(open, max_rows);
    for (visible_idx, entry_idx) in filtered.iter().skip(start).take(max_rows).enumerate() {
        let Some(entry) = open.entries.get(*entry_idx) else {
            continue;
        };
        let selected = Some(*entry_idx) == open.selected_entry_index();
        let y = inner.y.saturating_add(3 + (visible_idx as u16 * 2));
        let marker = if selected { "›" } else { " " };
        let row_style = if selected {
            Style::default()
                .fg(app.palette.text)
                .bg(app.palette.surface0)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(app.palette.subtext0)
        };
        let path_style = if selected {
            Style::default()
                .fg(app.palette.subtext0)
                .bg(app.palette.surface0)
        } else {
            Style::default().fg(app.palette.overlay0)
        };
        let status = entry.status_label();
        let title_width = inner
            .width
            .saturating_sub(status.len() as u16)
            .saturating_sub(4) as usize;
        let mut title = format!(
            "{marker} {}",
            truncate_text(&entry.display_name(), title_width)
        );
        if !status.is_empty() {
            let pad = inner
                .width
                .saturating_sub(title.chars().count() as u16)
                .saturating_sub(status.len() as u16)
                .max(1);
            title.push_str(&" ".repeat(pad as usize));
            title.push_str(status);
        }
        frame.render_widget(
            Paragraph::new(truncate_text(&title, inner.width as usize)).style(row_style),
            Rect::new(inner.x, y, inner.width, 1),
        );
        frame.render_widget(
            Paragraph::new(truncate_text(
                &format!("  {}", entry.path.display()),
                inner.width as usize,
            ))
            .style(path_style),
            Rect::new(inner.x, y.saturating_add(1), inner.width, 1),
        );
    }

    if filtered.is_empty() {
        frame.render_widget(
            Paragraph::new(" no matching worktrees")
                .style(Style::default().fg(app.palette.overlay0)),
            Rect::new(inner.x, inner.y.saturating_add(3), inner.width, 1),
        );
    }

    if let Some(error) = &open.error {
        frame.render_widget(
            Paragraph::new(format!(" {error}")).style(Style::default().fg(app.palette.red)),
            Rect::new(
                inner.x,
                inner.y + inner.height.saturating_sub(2),
                inner.width,
                1,
            ),
        );
    }

    let (open_rect, cancel_rect) = open_existing_worktree_button_rects(inner);
    render_action_button(
        frame,
        open_rect,
        Some("↵"),
        "open",
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.accent)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

fn render_open_worktree_search(
    app: &AppState,
    frame: &mut Frame,
    area: Rect,
    open: &WorktreeOpenState,
) {
    let focus_style = if open.search_focused {
        Style::default()
            .fg(app.palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.palette.overlay0)
    };
    let filtered_count = open.filtered_indices().len();
    let count = if open.query.trim().is_empty() {
        format!("{} checkouts", open.entries.len())
    } else {
        format!("{filtered_count}/{} checkouts", open.entries.len())
    };
    let mut spans = vec![Span::styled(" / ", focus_style)];
    if open.query.trim().is_empty() {
        spans.push(Span::styled(
            "filter worktrees",
            Style::default().fg(app.palette.overlay0),
        ));
    } else {
        spans.push(Span::styled(
            open.query.clone(),
            Style::default().fg(app.palette.text),
        ));
    }
    spans.push(Span::styled(
        format!(
            "{count:>width$}",
            width = area.width.saturating_sub(18) as usize
        ),
        Style::default().fg(app.palette.overlay0),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn confirm_close_overlay_text(app: &AppState) -> (String, String) {
    let ws_name = app
        .workspaces
        .get(app.selected)
        .map(|ws| ws.display_name())
        .unwrap_or_else(|| "?".to_string());
    let selected_space = app
        .workspaces
        .get(app.selected)
        .and_then(|ws| ws.worktree_space());
    // The whole-space close (#62) is an explicit affordance now, signalled by
    // the flag — NOT inferred from the selection being a non-linked parent.
    // Plain "Close" closes only the selected workspace even on the main row.
    let group_member_indices = if app.confirm_close_whole_space {
        selected_space
            .map(|space| {
                app.workspaces
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, ws)| {
                        ws.worktree_space()
                            .is_some_and(|member| member.key == space.key)
                            .then_some(idx)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let closes_group = app.confirm_close_whole_space && group_member_indices.len() > 1;
    let pane_count = if closes_group {
        group_member_indices
            .iter()
            .filter_map(|idx| app.workspaces.get(*idx))
            .map(|ws| ws.layout.pane_count())
            .sum()
    } else {
        app.workspaces
            .get(app.selected)
            .map(|ws| ws.layout.pane_count())
            .unwrap_or(0)
    };

    let pane_text = if pane_count == 1 {
        "1 pane".to_string()
    } else {
        format!("{pane_count} panes")
    };
    let workspace_text = if closes_group {
        let count = group_member_indices.len();
        if count == 1 {
            "1 workspace, ".to_string()
        } else {
            format!("{count} workspaces, ")
        }
    } else {
        String::new()
    };

    let title = if closes_group {
        "Close worktree group?"
    } else {
        "Close workspace?"
    };
    let detail = format!("{ws_name} — {workspace_text}{pane_text}");
    (title.to_string(), detail)
}

pub(super) fn render_confirm_close_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let (title, detail) = confirm_close_overlay_text(app);

    super::dim_background(frame, area);

    let Some(popup) = confirm_close_popup_rect(area) else {
        return;
    };

    let warn = Style::default()
        .fg(app.palette.red)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(app.palette.overlay0);

    let title_line = Line::from(vec![Span::styled(format!(" {title}"), warn)]);

    let detail_line = Line::from(vec![
        Span::styled(
            format!(" {}", detail.split(" — ").next().unwrap_or(&detail)),
            Style::default()
                .fg(app.palette.text)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            detail
                .split_once(" — ")
                .map(|(_, rest)| format!(" — {rest}"))
                .unwrap_or_default(),
            dim,
        ),
    ]);

    let Some(inner) = render_panel_shell(frame, popup, app.palette.red, app.palette.panel_bg)
    else {
        return;
    };

    if inner.height >= 3 {
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas::<4>(inner);

        frame.render_widget(Paragraph::new(title_line), rows[0]);
        frame.render_widget(Paragraph::new(detail_line), rows[1]);

        let (confirm_rect, cancel_rect) = confirm_close_button_rects(inner);
        render_action_button(
            frame,
            confirm_rect,
            Some("↵"),
            "confirm",
            Style::default()
                .fg(panel_contrast_fg(&app.palette))
                .bg(app.palette.red)
                .add_modifier(Modifier::BOLD),
        );
        render_action_button(
            frame,
            cancel_rect,
            Some("esc"),
            "cancel",
            Style::default()
                .fg(app.palette.text)
                .bg(app.palette.surface0)
                .add_modifier(Modifier::BOLD),
        );
    }
}

pub(crate) fn confirm_close_popup_rect(area: Rect) -> Option<Rect> {
    centered_popup_rect(area, 64, 6)
}

/// The body lines for the cross-machine checkout confirm dialog (#125):
/// (title, summary, warnings, status). Pure function of the state so it can be
/// unit-tested without a frame.
pub(super) fn cross_checkout_overlay_lines(
    app: &AppState,
) -> (String, String, Vec<String>, Option<String>) {
    let Some(checkout) = app.peer_checkout.as_ref() else {
        return (String::new(), String::new(), Vec::new(), None);
    };
    let title = format!("Check out {}", checkout.branch);
    let summary = format!(
        "Push '{}' from {} to origin and check it out here?",
        checkout.branch, checkout.host
    );
    let mut warnings = Vec::new();
    if let Some(report) = checkout.report.as_ref() {
        if report.was_dirty {
            warnings.push("⚠ peer has uncommitted changes (not transferred)".to_string());
        }
        if report.was_unpushed {
            warnings.push("⚠ branch has unpushed commits (will push to origin)".to_string());
        }
    }
    let status = if let Some(error) = checkout.error.as_ref() {
        Some(format!("✗ {error}"))
    } else if checkout.busy {
        Some("working…".to_string())
    } else {
        None
    };
    (title, summary, warnings, status)
}

pub(crate) fn cross_checkout_popup_rect(area: Rect) -> Option<Rect> {
    centered_popup_rect(area, 68, 9)
}

pub(super) fn render_cross_checkout_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let (title, summary, warnings, status) = cross_checkout_overlay_lines(app);
    if title.is_empty() {
        return;
    }
    let busy = app.peer_checkout.as_ref().is_some_and(|c| c.busy);

    super::dim_background(frame, area);
    let Some(popup) = cross_checkout_popup_rect(area) else {
        return;
    };

    let accent = app.palette.blue;
    let Some(inner) = render_panel_shell(frame, popup, accent, app.palette.panel_bg) else {
        return;
    };
    if inner.height < 5 {
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1), // title
        Constraint::Length(1), // summary
        Constraint::Length(1), // warning 1 / status
        Constraint::Length(1), // warning 2
        Constraint::Length(1), // buttons
    ])
    .areas::<5>(inner);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {title}"),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ))),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {summary}"),
            Style::default().fg(app.palette.text),
        ))),
        rows[1],
    );

    let warn_style = Style::default().fg(app.palette.yellow);
    if let Some(line) = warnings.first() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(format!(" {line}"), warn_style))),
            rows[2],
        );
    }
    if let Some(line) = warnings.get(1) {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(format!(" {line}"), warn_style))),
            rows[3],
        );
    }

    if let Some(status) = status {
        // Errors render red, the busy spinner dim; both sit on the status row.
        let is_error = status.starts_with('✗');
        let style = if is_error {
            Style::default()
                .fg(app.palette.red)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(app.palette.overlay0)
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(format!(" {status}"), style))),
            rows[4],
        );
    }

    // While a leg is running there is nothing to confirm — hide the buttons so
    // the user can't double-fire; esc still cancels via the key handler.
    if !busy {
        let (confirm_rect, cancel_rect) = confirm_close_button_rects(inner);
        render_action_button(
            frame,
            confirm_rect,
            Some("↵"),
            "check out",
            Style::default()
                .fg(panel_contrast_fg(&app.palette))
                .bg(accent)
                .add_modifier(Modifier::BOLD),
        );
        render_action_button(
            frame,
            cancel_rect,
            Some("esc"),
            "cancel",
            Style::default()
                .fg(app.palette.text)
                .bg(app.palette.surface0)
                .add_modifier(Modifier::BOLD),
        );
    }
}

pub(crate) fn confirm_close_button_rects(inner: Rect) -> (Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "confirm",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        3,
    );
    (rects[0], rects[1])
}

#[cfg(test)]
mod tests {
    use crate::{app::AppState, workspace::Workspace};

    use super::confirm_close_overlay_text;

    #[test]
    fn cross_checkout_lines_carry_host_branch_and_warnings() {
        let mut app = AppState::test_new();
        app.peer_checkout = Some(crate::app::state::PeerCheckoutState {
            generation: 1,
            peer: crate::config::PeerConfig {
                name: "anvil".into(),
                ..Default::default()
            },
            host: "anvil".into(),
            remote_workspace_id: "ws_3".into(),
            branch: "feature-x".into(),
            source_repo_root: "/repo".into(),
            source_checkout_path: "/repo".into(),
            source_workspace_id: "w".into(),
            repo_key: "/repo/.git".into(),
            repo_name: "proj".into(),
            report: Some(crate::peers::PeerCheckoutOutcome {
                branch: "feature-x".into(),
                was_dirty: true,
                was_unpushed: true,
                pushed: false,
            }),
            busy: false,
            error: None,
        });

        let (title, summary, warnings, status) = super::cross_checkout_overlay_lines(&app);
        assert!(title.contains("feature-x"));
        assert!(summary.contains("anvil") && summary.contains("feature-x"));
        assert_eq!(warnings.len(), 2, "dirty + unpushed both warn");
        assert!(
            status.is_none(),
            "idle dialog shows buttons, no status line"
        );

        // Busy hides warnings-vs-status independence: a running leg shows status.
        if let Some(checkout) = app.peer_checkout.as_mut() {
            checkout.busy = true;
        }
        let (_, _, _, status) = super::cross_checkout_overlay_lines(&app);
        assert_eq!(status.as_deref(), Some("working…"));

        // An error takes precedence over the busy spinner.
        if let Some(checkout) = app.peer_checkout.as_mut() {
            checkout.error = Some("fetch failed".into());
        }
        let (_, _, _, status) = super::cross_checkout_overlay_lines(&app);
        assert!(status.as_deref().unwrap().contains("fetch failed"));
    }

    #[test]
    fn confirm_close_text_reports_parent_group_scope() {
        let mut app = AppState::test_new();
        let mut parent = Workspace::test_new("main");
        parent.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "flock".into(),
            repo_root: "/repo/flock".into(),
            checkout_path: "/repo/flock".into(),
            is_linked_worktree: false,
        });
        let mut child = Workspace::test_new("issue");
        child.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "flock".into(),
            repo_root: "/repo/flock".into(),
            checkout_path: "/repo/flock-issue".into(),
            is_linked_worktree: true,
        });
        app.workspaces = vec![parent, child];
        app.selected = 0;
        // Whole-space scope is now explicit (#62), not inferred.
        app.confirm_close_whole_space = true;

        let (title, detail) = confirm_close_overlay_text(&app);

        assert_eq!(title, "Close worktree group?");
        assert_eq!(detail, "main — 2 workspaces, 2 panes");
    }

    #[test]
    fn confirm_close_text_reports_single_workspace_when_not_whole_space() {
        // Plain "Close" on the main row (#62): even with worktree siblings,
        // the confirm reports a single-workspace close, not the group.
        let mut app = AppState::test_new();
        let mut parent = Workspace::test_new("main");
        parent.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "flock".into(),
            repo_root: "/repo/flock".into(),
            checkout_path: "/repo/flock".into(),
            is_linked_worktree: false,
        });
        let mut child = Workspace::test_new("issue");
        child.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "flock".into(),
            repo_root: "/repo/flock".into(),
            checkout_path: "/repo/flock-issue".into(),
            is_linked_worktree: true,
        });
        app.workspaces = vec![parent, child];
        app.selected = 0;
        app.confirm_close_whole_space = false;

        let (title, _detail) = confirm_close_overlay_text(&app);

        assert_eq!(title, "Close workspace?");
    }
}
