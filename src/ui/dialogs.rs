use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
    Frame,
};

use super::widgets::{
    action_button_row_rects, centered_popup_rect, panel_contrast_fg, render_action_button,
    render_action_button_focused, render_modal_header, render_modal_shell, render_panel_shell,
    ActionButtonSpec,
};
use crate::app::{
    state::{RemoveWorktreeControl, WorktreeOpenState},
    AppState, Mode,
};

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

/// Popup height for the new-worktree dialog. The branch-session flow adds an
/// editable seed-prompt row pair (#159), so it needs three extra lines; a plain
/// new-worktree keeps the original compact size.
pub(crate) fn new_linked_worktree_popup_height(has_seed: bool) -> u16 {
    if has_seed {
        13
    } else {
        10
    }
}

/// Smallest inner height the new-worktree dialog can draw without overrunning
/// itself.
///
/// The rows are fixed top-down — header, branch label + input, the seed pair
/// when present, checkout label + path — and the error area starts right below
/// them while the buttons stay pinned to `inner.height - 1`. The error needs at
/// least one row strictly above the buttons, so the first error row (5, or 7
/// with a seed) must be no lower than `inner.height - 2`.
///
/// A single bound of 7 covered the plain dialog but was two short for the seed
/// variant, whose error row landed on the popup's bottom border and painted
/// over it (#243). `centered_popup_rect` clamps the popup on short terminals,
/// so this is reachable by resizing rather than by any state the user picks.
pub(crate) fn min_inner_height(has_seed: bool) -> u16 {
    if has_seed {
        9
    } else {
        7
    }
}

pub(crate) fn new_linked_worktree_inner_rect(area: Rect, has_seed: bool) -> Option<Rect> {
    centered_popup_rect(area, 68, new_linked_worktree_popup_height(has_seed)).map(|popup| {
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

/// How many rows the stakes block (#325) needs: one per listed path, plus the
/// unpushed-commits line, plus the "… and N more" tail. Zero while the probe is
/// still running or when there is nothing to lose.
pub(crate) const REMOVE_WORKTREE_MAX_LISTED: usize = 6;

pub(crate) fn remove_worktree_stakes_rows(remove: &crate::app::state::WorktreeRemoveState) -> u16 {
    let Some(probe) = remove.probe.as_ref() else {
        return 0;
    };
    if !probe.has_stakes() {
        return 0;
    }
    let mut rows = 1u16; // the "this also destroys:" lead-in
    match probe.dirty.as_deref() {
        // Unknown collapses to a single line saying so — never a silent zero.
        None => rows += 1,
        Some(paths) => {
            rows += paths.len().min(REMOVE_WORKTREE_MAX_LISTED) as u16;
            if paths.len() > REMOVE_WORKTREE_MAX_LISTED {
                rows += 1;
            }
        }
    }
    if probe.unpushed.is_none_or(|count| count > 0) {
        rows += 1;
    }
    rows
}

/// Where the force toggle's line sits inside the dialog (#326), so a click can
/// reach it. `None` until the probe lands, which is when the toggle is drawn.
///
/// Derived from the same two numbers the render uses — the seven fixed rows
/// above the flexible tail, then the stakes block — so the clickable row and
/// the drawn row cannot drift apart.
pub(crate) fn remove_worktree_force_rect(
    inner: Rect,
    remove: &crate::app::state::WorktreeRemoveState,
) -> Option<Rect> {
    remove.probe.as_ref()?;
    let y = inner.y + REMOVE_WORKTREE_FIXED_ROWS + remove_worktree_stakes_rows(remove);
    (y < inner.y + inner.height.saturating_sub(1)).then(|| Rect::new(inner.x, y, inner.width, 1))
}

/// Rows above the dialog's flexible tail: title, lead-in, path, gate line,
/// dirty warning, status, spacer.
pub(crate) const REMOVE_WORKTREE_FIXED_ROWS: u16 = 7;

/// The dialog grows with what it has to account for (#325): the fixed 10-row
/// box predates it having anything to list.
pub(crate) fn remove_worktree_popup_rect(
    area: Rect,
    remove: &crate::app::state::WorktreeRemoveState,
) -> Option<Rect> {
    // +1 for the force-toggle line, which is always present once the probe has
    // landed so the affordance does not appear and disappear under the cursor.
    let extra = remove_worktree_stakes_rows(remove) + u16::from(remove.probe.is_some());
    centered_popup_rect(area, 72, 10 + extra)
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

fn render_dialog_field_label(
    frame: &mut Frame,
    rect: Rect,
    text: &str,
    palette: &crate::app::state::Palette,
) {
    frame.render_widget(
        Paragraph::new(text.to_string()).style(Style::default().fg(palette.overlay0)),
        rect,
    );
}

/// A single-line text input box backed by a [`LineEditor`] (#159). Renders the
/// horizontally-scrolled window (a leading space, then up to `width-2` chars so
/// a trailing cell is reserved for the cursor). Returns the screen position of
/// the insertion point when `focused`, so the caller can place the *real*
/// terminal cursor there — flk is a terminal emulator; the dialog gets a native
/// blinking cursor instead of a painted block.
fn render_dialog_field_input(
    frame: &mut Frame,
    rect: Rect,
    editor: &crate::app::line_editor::LineEditor,
    focused: bool,
    palette: &crate::app::state::Palette,
) -> Option<(u16, u16)> {
    frame.render_widget(Clear, rect);
    let view_width = usize::from(rect.width.saturating_sub(2));
    let (shown, cursor_col) = editor.view(view_width);
    frame.render_widget(
        Paragraph::new(format!(" {shown}"))
            .style(Style::default().fg(palette.text).bg(palette.surface0)),
        rect,
    );
    // +1 for the leading space; `view` guarantees cursor_col <= view_width.
    focused.then(|| (rect.x + 1 + cursor_col as u16, rect.y))
}

/// Height of the issue-drop popup: header, two labelled fields, the repository
/// list, and a row for the status or error banner.
fn issue_drop_popup_height() -> u16 {
    18
}

/// Rows the repository list gets, once the fixed chrome is accounted for.
fn issue_drop_list_rows(inner_height: u16) -> u16 {
    // header(1) + repo label(1) + repo input(1) + title label(1) + title
    // input(1) + banner(1) + hint(1)
    inner_height.saturating_sub(7)
}

/// Scroll the list so the selected row stays visible.
///
/// Returned as the first visible index rather than mutating state: render is
/// pure in this codebase, so the viewport is derived every frame from the
/// selection instead of being stored and kept in sync.
pub(crate) fn issue_drop_scroll_offset(selected: usize, rows: usize) -> usize {
    if rows == 0 {
        return 0;
    }
    // Keep the cursor on screen: anchored at the top until it passes the last
    // visible row, then follow it.
    selected.saturating_sub(rows - 1)
}

pub(super) fn render_issue_drop_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    use crate::app::issue_drop::{DirectoryStatus, IssueDropFocus};
    use crate::github::repos::Provenance;

    let Some(drop) = app.issue_drop.as_ref() else {
        return;
    };

    super::dim_background(frame, area);
    let Some(inner) = render_modal_shell(frame, area, 72, issue_drop_popup_height(), &app.palette)
    else {
        return;
    };
    if inner.height < 8 {
        return;
    }

    let row = |i: u16| Rect::new(inner.x, inner.y + i, inner.width, 1);
    render_modal_header(frame, row(0), "drop an issue", &app.palette);

    let mut cursor_pos = None;

    render_dialog_field_label(frame, row(1), " repository · type to filter", &app.palette);
    cursor_pos = cursor_pos.or(render_dialog_field_input(
        frame,
        row(2),
        &drop.query,
        drop.focus == IssueDropFocus::Repo,
        &app.palette,
    ));

    render_dialog_field_label(frame, row(3), " title · tab to switch", &app.palette);
    cursor_pos = cursor_pos.or(render_dialog_field_input(
        frame,
        row(4),
        &drop.title,
        drop.focus == IssueDropFocus::Title,
        &app.palette,
    ));

    // The destination list.
    let list_rows = issue_drop_list_rows(inner.height);
    let filtered = drop.filtered();
    let offset = issue_drop_scroll_offset(drop.selected, usize::from(list_rows));
    for slot in 0..list_rows {
        let Some(entry_idx) = filtered.get(offset + usize::from(slot)) else {
            break;
        };
        let Some(repo) = drop.repos.get(*entry_idx) else {
            break;
        };
        let selected = offset + usize::from(slot) == drop.selected;
        // The tier is shown, not just implied by order: "why is this repo
        // first" is otherwise invisible, and the answer is what tells the
        // operator whether they are about to file somewhere they know.
        let tier = match repo.provenance {
            Provenance::LocalCheckout => "local",
            Provenance::SeenInFleet => "fleet",
            Provenance::Reachable => "     ",
        };
        let style = if selected {
            Style::default()
                .fg(app.palette.text)
                .bg(app.palette.surface1)
        } else {
            Style::default().fg(app.palette.subtext0)
        };
        frame.render_widget(
            Paragraph::new(format!(" {tier}  {}", repo.name_with_owner)).style(style),
            row(5 + slot),
        );
    }

    let banner_row = row(5 + list_rows);
    if let Some(error) = drop.error.as_ref() {
        frame.render_widget(
            Paragraph::new(format!(" {error}")).style(Style::default().fg(app.palette.red)),
            banner_row,
        );
    } else {
        let (text, colour) = match &drop.status {
            DirectoryStatus::Loading => (
                " enumerating repositories…".to_string(),
                app.palette.overlay0,
            ),
            DirectoryStatus::Ready => (
                format!(" {} repositories · ↑↓ to choose", filtered.len()),
                app.palette.overlay0,
            ),
            // Not a dead end: a typed owner/name still files.
            DirectoryStatus::Failed(kind) => (
                format!(" could not list repositories ({kind}) — type owner/name"),
                app.palette.yellow,
            ),
        };
        frame.render_widget(
            Paragraph::new(text).style(Style::default().fg(colour)),
            banner_row,
        );
    }

    // Filing into a repo with no local checkout and nobody in the fleet on it
    // is the case a mistyped owner/name looks exactly like, so say so before
    // the operator commits an editor session to it.
    let unfamiliar = !matches!(
        drop.provenance(),
        Some(Provenance::LocalCheckout | Provenance::SeenInFleet)
    );
    let (hint, hint_colour) = if unfamiliar && drop.destination().is_some() {
        (
            " ↵ opens your editor · no checkout here — check the owner/name",
            app.palette.yellow,
        )
    } else {
        (
            " ↵ opens your editor in a new pane · esc keeps the draft",
            app.palette.overlay0,
        )
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(hint_colour)),
        row(6 + list_rows),
    );

    if let Some(pos) = cursor_pos {
        frame.set_cursor_position(pos);
    }
}

pub(super) fn render_new_linked_worktree_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    use crate::app::state::WorktreeCreateFocus;
    let Some(create) = app.worktree_create.as_ref() else {
        return;
    };

    super::dim_background(frame, area);
    let has_seed = create.branch_plan.is_some();
    let Some(inner) = render_modal_shell(
        frame,
        area,
        68,
        new_linked_worktree_popup_height(has_seed),
        &app.palette,
    ) else {
        return;
    };
    if inner.height < min_inner_height(has_seed) {
        return;
    }

    // Lay the labeled rows out top-down; the seed pair only exists in the
    // branch-session flow. Buttons stay bottom-anchored via button_rects.
    let row = |i: u16| Rect::new(inner.x, inner.y + i, inner.width, 1);

    let header = if has_seed {
        "branch session into new worktree"
    } else {
        "new worktree"
    };
    render_modal_header(frame, row(0), header, &app.palette);

    let mut cursor_pos = None;

    render_dialog_field_label(frame, row(1), " branch", &app.palette);
    cursor_pos = cursor_pos.or(render_dialog_field_input(
        frame,
        row(2),
        &create.branch_input,
        create.focus == WorktreeCreateFocus::Branch,
        &app.palette,
    ));

    let mut next = 3;
    if has_seed {
        render_dialog_field_label(frame, row(3), " seed prompt · tab to switch", &app.palette);
        cursor_pos = cursor_pos.or(render_dialog_field_input(
            frame,
            row(4),
            &create.seed,
            create.focus == WorktreeCreateFocus::Seed,
            &app.palette,
        ));
        next = 5;
    }

    let checkout = create.checkout_path.display().to_string();
    render_dialog_field_label(frame, row(next), " checkout", &app.palette);
    frame.render_widget(
        Paragraph::new(format!(" {checkout}")).style(Style::default().fg(app.palette.subtext0)),
        row(next + 1),
    );

    if create.creating {
        frame.render_widget(
            Paragraph::new(" creating…").style(Style::default().fg(app.palette.overlay0)),
            row(next + 2),
        );
    } else if let Some(error) = &create.error {
        // Errors get every row between here and the button row, wrapped. A
        // one-row Rect silently truncated `explain_worktree_add_failure`'s
        // remedy line and left only git's cryptic half on screen (#243).
        // That is two rows without a seed and three with one, which is why
        // the explanations are written to fit two — see that function.
        let top = next + 2;
        let rows_available = inner.height.saturating_sub(1).saturating_sub(top).max(1);
        frame.render_widget(
            // Wrap does not indent continuations, so re-apply the same left
            // margin the rows above use to every line of the message.
            Paragraph::new(format!(" {}", error.replace('\n', "\n ")))
                .style(Style::default().fg(app.palette.red))
                .wrap(Wrap { trim: false }),
            Rect::new(inner.x, inner.y + top, inner.width, rows_available),
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

    // Place the real terminal cursor at the focused field's insertion point.
    // In this mode the focused pane suppresses its own cursor, so the dialog
    // owns it — a native blinking cursor, not a painted block.
    if let Some(pos) = cursor_pos {
        frame.set_cursor_position(pos);
    }
}

pub(super) fn render_remove_worktree_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let Some(remove) = app.worktree_remove.as_ref() else {
        return;
    };

    super::dim_background(frame, area);
    let Some(popup) = remove_worktree_popup_rect(area, remove) else {
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
    // #360: a workspace whose checkout vanished is not being asked to remove
    // a folder — the folder is the thing that already went.
    let checkout_missing = matches!(
        remove.merge_gate,
        Some(crate::worktree::WorktreeMergeGate::CheckoutMissing)
    );
    frame.render_widget(
        Paragraph::new(if checkout_missing {
            " This checkout folder is already gone:"
        } else {
            " This removes the checkout folder:"
        })
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
            // #360 goes ahead of the force arm: force cannot delete a branch
            // that was never resolved, so it has nothing to promise here.
            Some(crate::worktree::WorktreeMergeGate::CheckoutMissing) => (
                " ✗ the checkout is gone — no branch to judge; ↵ closes the workspace.".to_string(),
                Style::default().fg(app.palette.yellow),
            ),
            Some(crate::worktree::WorktreeMergeGate::Merged { evidence }) => (
                format!(
                    " ✓ {evidence} — branch {} will be deleted too.",
                    remove.branch.as_deref().unwrap_or("?")
                ),
                Style::default().fg(app.palette.green),
            ),
            // #325: with force armed the branch goes regardless, so the line
            // has to stop promising it is kept.
            Some(crate::worktree::WorktreeMergeGate::NotMerged) if remove.force => (
                format!(
                    " ! forced — branch {} will be deleted with no merge evidence.",
                    remove.branch.as_deref().unwrap_or("?")
                ),
                Style::default()
                    .fg(app.palette.red)
                    .add_modifier(Modifier::BOLD),
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

    // #325: the stakes block and the force toggle share the flexible tail row,
    // above the button line the button rects pin to `inner.height - 1`.
    let tail = rows[7];
    let tail = Rect::new(tail.x, tail.y, tail.width, tail.height.saturating_sub(1));
    render_remove_worktree_stakes(app, frame, tail, remove);

    let forced = remove.force_confirmation || remove.force;
    let (remove_rect, cancel_rect) = remove_worktree_button_rects(inner, forced);
    let remove_label = if forced { "delete anyway" } else { "remove" };
    render_action_button_focused(
        frame,
        remove_rect,
        Some("↵"),
        remove_label,
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.red)
            .add_modifier(Modifier::BOLD),
        remove.focus == RemoveWorktreeControl::Remove,
    );
    render_action_button_focused(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
        remove.focus == RemoveWorktreeControl::Cancel,
    );
}

/// What this kill destroys beyond the checkout, and the force toggle read in
/// that context (#325).
///
/// Nothing renders while the probe is still running — the confirm is already
/// held until the gate lands, and a half-filled account is worse than none.
/// An unreadable probe says "could not be read", never "clean": the user is
/// authorising a destructive act, and the difference between an empty set and
/// an unknown one is the whole reason this block exists.
fn render_remove_worktree_stakes(
    app: &AppState,
    frame: &mut Frame,
    area: Rect,
    remove: &crate::app::state::WorktreeRemoveState,
) {
    let Some(probe) = remove.probe.as_ref() else {
        return;
    };
    if area.height == 0 {
        return;
    }
    let p = &app.palette;
    // #360: every probe of a checkout that is not there comes back unreadable,
    // and the block below renders unreadable as "assume there are uncommitted
    // changes". Here there are none to assume — the directory is gone and the
    // branch, unresolved, is kept whatever the force toggle says.
    if matches!(
        remove.merge_gate,
        Some(crate::worktree::WorktreeMergeGate::CheckoutMissing)
    ) {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    " nothing to destroy — the checkout is already gone.",
                    Style::default().fg(p.overlay0),
                )),
                remove_worktree_force_line(remove, p),
            ]),
            area,
        );
        return;
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    if probe.has_stakes() {
        lines.push(Line::from(Span::styled(
            " this also destroys:",
            Style::default().fg(p.overlay0),
        )));
        match probe.dirty.as_deref() {
            None => lines.push(Line::from(Span::styled(
                "   uncommitted changes could not be read — assume there are some",
                Style::default().fg(p.yellow),
            ))),
            Some(paths) => {
                for path in paths.iter().take(REMOVE_WORKTREE_MAX_LISTED) {
                    lines.push(Line::from(Span::styled(
                        format!("   {path}"),
                        Style::default().fg(p.peach),
                    )));
                }
                if paths.len() > REMOVE_WORKTREE_MAX_LISTED {
                    lines.push(Line::from(Span::styled(
                        format!("   … and {} more", paths.len() - REMOVE_WORKTREE_MAX_LISTED),
                        Style::default().fg(p.overlay0),
                    )));
                }
            }
        }
        match probe.unpushed {
            None => lines.push(Line::from(Span::styled(
                "   unpushed commits could not be counted",
                Style::default().fg(p.yellow),
            ))),
            Some(count) if count > 0 => lines.push(Line::from(Span::styled(
                format!(
                    "   {count} commit{} no remote holds",
                    if count == 1 { "" } else { "s" }
                ),
                Style::default().fg(p.peach),
            ))),
            Some(_) => {}
        }
    }
    // The toggle is always offered once the probe has landed, so the
    // affordance does not appear and vanish under the cursor as the account
    // above it changes.
    lines.push(remove_worktree_force_line(remove, p));
    frame.render_widget(Paragraph::new(lines), area);
}

/// The force toggle's own line: what it is, and what turning it on changes.
fn remove_worktree_force_line(
    remove: &crate::app::state::WorktreeRemoveState,
    p: &crate::app::state::Palette,
) -> Line<'static> {
    // The ring is a leading marker rather than REVERSED here: this is a line of
    // text, not a button rect, and swapping a whole row's colours would read as
    // a selection bar. Width-neutral — it replaces the leading space (#326).
    let focused = remove.focus == crate::app::state::RemoveWorktreeControl::Force;
    let mut spans = vec![Span::styled(
        if focused { "›[f] " } else { " [f] " },
        Style::default().fg(if focused { p.text } else { p.overlay1 }),
    )];
    if remove.force {
        spans.push(Span::styled(
            "force ON",
            Style::default().fg(p.red).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            if remove.branch_protected {
                " — deletes dirty files; the protected branch is still kept"
            } else {
                " — deletes the branch without evidence, and dirty files"
            },
            Style::default().fg(p.red),
        ));
    } else {
        spans.push(Span::styled("force off", Style::default().fg(p.overlay0)));
        spans.push(Span::styled(
            " — press f to delete anyway",
            Style::default().fg(p.overlay0),
        ));
    }
    Line::from(spans)
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
            // Defensive: the human dialog never plans a scheduled-only
            // quarantine action, but the render must not panic if one
            // ever crosses (belt-and-braces).
            KillAction::Quarantine => {
                n_skip += 1;
                ("skip — quarantine (scheduled only)", app.palette.overlay0)
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
        .and_then(|ws| ws.worktree_space_here());
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
                        ws.worktree_space_here()
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

    #[test]
    fn the_issue_drop_list_scrolls_to_keep_the_selection_visible() {
        // Anchored at the top until the cursor passes the last visible row,
        // then follows it — the same rule LineEditor::view uses horizontally.
        assert_eq!(issue_drop_scroll_offset(0, 5), 0);
        assert_eq!(issue_drop_scroll_offset(4, 5), 0, "last visible row");
        assert_eq!(issue_drop_scroll_offset(5, 5), 1, "scrolls by one");
        assert_eq!(issue_drop_scroll_offset(9, 5), 5);
        // A zero-row viewport must not underflow.
        assert_eq!(issue_drop_scroll_offset(3, 0), 0);
    }

    #[test]
    fn the_issue_drop_list_leaves_room_for_its_chrome() {
        // Header, two labelled fields, banner and hint all sit outside the
        // list; a taller popup must give the extra rows to the list itself.
        assert_eq!(issue_drop_list_rows(18), 11);
        assert!(issue_drop_list_rows(24) > issue_drop_list_rows(18));
        // A popup too short for the chrome yields no list rather than wrapping.
        assert_eq!(issue_drop_list_rows(6), 0);
    }
    use crate::{app::AppState, workspace::Workspace};
    use ratatui::{backend::TestBackend, layout::Rect, Terminal};

    use super::confirm_close_overlay_text;
    use super::{issue_drop_list_rows, issue_drop_scroll_offset};
    use super::{
        new_linked_worktree_inner_rect, new_linked_worktree_popup_height,
        remove_worktree_popup_rect, remove_worktree_stakes_rows,
        render_new_linked_worktree_overlay, render_remove_worktree_overlay,
        REMOVE_WORKTREE_MAX_LISTED,
    };
    use crate::app::state::{
        RemoveWorktreeControl, WorktreeCreateFocus, WorktreeCreateState, WorktreeRemoveState,
    };

    fn worktree_create_with(
        branch_plan: Option<crate::agent_resume::AgentResumePlan>,
        seed_prompt: &str,
        focus: WorktreeCreateFocus,
    ) -> WorktreeCreateState {
        use crate::app::line_editor::LineEditor;
        WorktreeCreateState {
            branch_parent: None,
            branch_plan,
            source_workspace_id: "w".into(),
            source_checkout_path: "/repo".into(),
            source_existing_membership: None,
            source_repo_root: "/repo".into(),
            repo_key: "repo-key".into(),
            repo_name: "flock".into(),
            branch: "feat/x".into(),
            branch_input: LineEditor::new("feat/x"),
            base: "HEAD".into(),
            checkout_path: "/wt/feat-x".into(),
            seed: LineEditor::new(seed_prompt),
            focus,
            error: None,
            creating: false,
        }
    }

    fn claude_fork_plan() -> crate::agent_resume::AgentResumePlan {
        crate::agent_resume::AgentResumePlan {
            agent: "claude".into(),
            argv: vec!["claude".into(), "--fork-session".into()],
            dedupe_key: "k".into(),
        }
    }

    fn render_worktree_dialog(app: &AppState) -> String {
        let mut terminal =
            Terminal::new(TestBackend::new(80, 24)).expect("test terminal should initialize");
        terminal
            .draw(|frame| render_new_linked_worktree_overlay(app, frame, Rect::new(0, 0, 80, 24)))
            .expect("worktree overlay should render");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    fn kill_dialog_state(probe: Option<crate::worktree::KillProbe>) -> WorktreeRemoveState {
        WorktreeRemoveState {
            managed: true,
            workspace_id: "ws".into(),
            repo_root: "/repo/flock".into(),
            path: "/repo/flock-issue".into(),
            error: None,
            removing: false,
            force_confirmation: false,
            focus: RemoveWorktreeControl::Remove,
            force: false,
            probe,
            delete_branch: true,
            branch: Some("feature/x".into()),
            merge_gate: Some(crate::worktree::WorktreeMergeGate::NotMerged),
            branch_protected: false,
            gate_timed_out: false,
        }
    }

    fn render_kill_dialog(app: &AppState) -> String {
        let mut terminal =
            Terminal::new(TestBackend::new(80, 32)).expect("test terminal should initialize");
        terminal
            .draw(|frame| render_remove_worktree_overlay(app, frame, Rect::new(0, 0, 80, 32)))
            .expect("kill overlay should render");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    #[test]
    fn kill_dialog_accounts_for_what_the_removal_destroys() {
        // #325: before this the dialog named the checkout path and the gate
        // verdict, and asked the user to authorise a destructive act against
        // an unnamed set of files.
        let mut app = AppState::test_new();
        app.worktree_remove = Some(kill_dialog_state(Some(crate::worktree::KillProbe {
            dirty: Some(vec![" M src/lib.rs".into(), "?? scratch.txt".into()]),
            unpushed: Some(2),
        })));

        let text = render_kill_dialog(&app);
        assert!(text.contains("this also destroys"), "{text:?}");
        assert!(text.contains("src/lib.rs"), "{text:?}");
        assert!(text.contains("scratch.txt"), "{text:?}");
        assert!(text.contains("2 commits no remote holds"), "{text:?}");
        assert!(
            text.contains("force off"),
            "the toggle is offered: {text:?}"
        );
    }

    #[test]
    fn kill_dialog_says_unknown_rather_than_clean_when_git_could_not_be_asked() {
        // The difference between an empty set and an unknown one is the whole
        // reason the block exists — rendering unknown as clean would be worse
        // than the silence it replaced.
        let mut app = AppState::test_new();
        app.worktree_remove = Some(kill_dialog_state(Some(crate::worktree::KillProbe {
            dirty: None,
            unpushed: None,
        })));

        let text = render_kill_dialog(&app);
        assert!(text.contains("could not be read"), "{text:?}");
        assert!(text.contains("could not be counted"), "{text:?}");
    }

    #[test]
    fn kill_dialog_names_the_missing_checkout_instead_of_blaming_the_branch() {
        // #360: the gate could not form a question, and reporting that as "no
        // merge evidence" sends the operator to go check a PR for work that
        // was never at risk. Every probe of a gone checkout is unreadable too,
        // so the stakes block used to tell them to assume uncommitted changes
        // in a directory that is not there.
        let mut app = AppState::test_new();
        let mut remove = kill_dialog_state(Some(crate::worktree::KillProbe {
            dirty: None,
            unpushed: None,
        }));
        remove.branch = None;
        remove.merge_gate = Some(crate::worktree::WorktreeMergeGate::CheckoutMissing);
        // Force changes nothing here: there is no branch to delete without
        // evidence, so the line must not promise one.
        remove.force = true;
        app.worktree_remove = Some(remove);

        let text = render_kill_dialog(&app);
        assert!(text.contains("the checkout is gone"), "{text:?}");
        assert!(text.contains("closes the workspace"), "{text:?}");
        assert!(
            !text.contains("no merge evidence"),
            "nothing was asked about a branch: {text:?}"
        );
        assert!(
            !text.contains("could not be read"),
            "there are no changes to assume: {text:?}"
        );
        assert!(text.contains("already gone"), "{text:?}");
    }

    #[test]
    fn kill_dialog_shows_nothing_extra_while_the_probe_is_running() {
        // The confirm is already held until the gate lands; a half-filled
        // account is worse than none.
        let mut app = AppState::test_new();
        app.worktree_remove = Some(kill_dialog_state(None));

        let text = render_kill_dialog(&app);
        assert!(!text.contains("this also destroys"), "{text:?}");
        assert!(!text.contains("force"), "{text:?}");
    }

    #[test]
    fn armed_force_restates_the_gate_line_instead_of_promising_the_branch_is_kept() {
        let mut app = AppState::test_new();
        let mut remove = kill_dialog_state(Some(crate::worktree::KillProbe {
            dirty: Some(Vec::new()),
            unpushed: Some(0),
        }));
        remove.force = true;
        app.worktree_remove = Some(remove);

        let text = render_kill_dialog(&app);
        assert!(text.contains("force ON"), "{text:?}");
        assert!(
            text.contains("will be deleted with no merge evidence"),
            "{text:?}"
        );
        assert!(
            !text.contains("the branch is kept"),
            "the old line would now be a lie: {text:?}"
        );
        assert!(text.contains("delete anyway"), "button label: {text:?}");
    }

    #[test]
    fn the_focused_control_is_visibly_marked() {
        // #326: the ring on the force line is a leading marker rather than a
        // reversed row — width-neutral, so the clickable rect and the drawn
        // row stay the same width.
        let mut app = AppState::test_new();
        let mut remove = kill_dialog_state(Some(crate::worktree::KillProbe {
            dirty: Some(vec!["?? scratch.txt".into()]),
            unpushed: Some(1),
        }));
        remove.focus = RemoveWorktreeControl::Force;
        app.worktree_remove = Some(remove);

        let text = render_kill_dialog(&app);
        assert!(text.contains("›[f]"), "focused force line: {text:?}");

        let mut app = AppState::test_new();
        let mut remove = kill_dialog_state(Some(crate::worktree::KillProbe {
            dirty: Some(vec!["?? scratch.txt".into()]),
            unpushed: Some(1),
        }));
        remove.focus = RemoveWorktreeControl::Cancel;
        app.worktree_remove = Some(remove);
        let text = render_kill_dialog(&app);
        assert!(
            !text.contains("›[f]"),
            "the marker moves with the focus: {text:?}"
        );
    }

    #[test]
    fn kill_dialog_grows_with_the_account_and_truncates_a_long_one() {
        let clean = kill_dialog_state(Some(crate::worktree::KillProbe {
            dirty: Some(Vec::new()),
            unpushed: Some(0),
        }));
        assert_eq!(
            remove_worktree_stakes_rows(&clean),
            0,
            "nothing to lose, nothing to show"
        );

        let many = kill_dialog_state(Some(crate::worktree::KillProbe {
            dirty: Some((0..20).map(|n| format!("?? file{n}.txt")).collect()),
            unpushed: Some(1),
        }));
        // lead-in + capped list + "… and N more" + the unpushed line.
        assert_eq!(
            remove_worktree_stakes_rows(&many),
            1 + REMOVE_WORKTREE_MAX_LISTED as u16 + 1 + 1
        );

        let area = Rect::new(0, 0, 80, 40);
        let small = remove_worktree_popup_rect(area, &clean).expect("popup");
        let large = remove_worktree_popup_rect(area, &many).expect("popup");
        assert!(
            large.height > small.height,
            "the box has to make room: {} vs {}",
            large.height,
            small.height
        );

        let mut app = AppState::test_new();
        app.worktree_remove = Some(many);
        let text = render_kill_dialog(&app);
        assert!(text.contains("and 14 more"), "{text:?}");
    }

    #[test]
    fn popup_height_grows_for_the_seed_row() {
        assert_eq!(new_linked_worktree_popup_height(false), 10);
        assert!(new_linked_worktree_popup_height(true) > new_linked_worktree_popup_height(false));
    }

    #[test]
    fn branch_session_dialog_renders_the_editable_seed_row() {
        let mut app = AppState::test_new();
        app.worktree_create = Some(worktree_create_with(
            Some(claude_fork_plan()),
            "seed the fork here",
            WorktreeCreateFocus::Seed,
        ));

        let rendered = render_worktree_dialog(&app);
        assert!(rendered.contains("branch session into new worktree"));
        assert!(rendered.contains("seed prompt"), "seed label must show");
        assert!(
            rendered.contains("seed the fork here"),
            "seed value must show"
        );
    }

    #[test]
    fn create_error_shows_the_remedy_not_just_gits_half() {
        // #243: the error sat in a one-row Rect, so a two-line explanation
        // rendered its first line and dropped the only actionable part. Both
        // dialog variants have to show it — the seed flow is where the unborn
        // HEAD was actually hit.
        let explained =
            crate::worktree::explain_worktree_add_failure("HEAD", "fatal: invalid reference: HEAD");
        assert!(explained.lines().count() > 1, "fixture must be multi-line");

        for plan in [None, Some(claude_fork_plan())] {
            let has_seed = plan.is_some();
            let mut app = AppState::test_new();
            let mut create = worktree_create_with(plan, "", WorktreeCreateFocus::Branch);
            create.error = Some(explained.clone());
            app.worktree_create = Some(create);

            let rendered = render_worktree_dialog(&app);
            for line in explained.lines() {
                assert!(
                    rendered.contains(line.trim()),
                    "has_seed={has_seed}: dropped {line:?}"
                );
            }
            // The buttons keep their row — the error must not overrun them.
            assert!(
                rendered.contains("create and open"),
                "has_seed={has_seed}: the error overran the button row"
            );
        }
    }

    #[test]
    fn a_shrinking_terminal_never_paints_over_the_dialog_border() {
        // #243: the error row is placed from the top while the buttons are
        // pinned to the bottom, so a clamped popup let the two meet on the
        // frame. Sweep every height the popup can be clamped to and require
        // the box to stay a box — either fully drawn or bailed out of, never
        // half-drawn through its own border.
        let explained =
            crate::worktree::explain_worktree_add_failure("HEAD", "fatal: invalid reference: HEAD");

        for has_seed in [false, true] {
            for height in 6..=24u16 {
                let mut app = AppState::test_new();
                let plan = has_seed.then(claude_fork_plan);
                let mut create = worktree_create_with(plan, "seed", WorktreeCreateFocus::Branch);
                create.error = Some(explained.clone());
                app.worktree_create = Some(create);

                let area = Rect::new(0, 0, 80, height);
                let mut terminal = Terminal::new(TestBackend::new(80, height))
                    .expect("test terminal should initialize");
                terminal
                    .draw(|frame| render_new_linked_worktree_overlay(&app, frame, area))
                    .expect("overlay should render");
                let buffer = terminal.backend().buffer().clone();

                let Some(popup) = super::centered_popup_rect(
                    area,
                    68,
                    super::new_linked_worktree_popup_height(has_seed),
                ) else {
                    continue; // too small for a popup at all — nothing drawn
                };

                // Every cell of the top and bottom border rows must still be a
                // box-drawing glyph.
                for y in [popup.y, popup.y + popup.height - 1] {
                    let row: String = (popup.x..popup.x + popup.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect();
                    assert!(
                        row.chars().all(|c| "┌┐└┘─".contains(c)),
                        "has_seed={has_seed} height={height}: border row {y} was painted over: {row:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn long_seed_scrolls_to_keep_the_tail_visible() {
        let mut app = AppState::test_new();
        let long = format!("HEADSTART{}TAILEND", "x".repeat(200));
        app.worktree_create = Some(worktree_create_with(
            Some(claude_fork_plan()),
            &long,
            WorktreeCreateFocus::Seed,
        ));

        let rendered = render_worktree_dialog(&app);
        assert!(
            rendered.contains("TAILEND"),
            "the tail (cursor end) must stay visible"
        );
        assert!(
            !rendered.contains("HEADSTART"),
            "the head should scroll off once the value outgrows the box"
        );
    }

    #[test]
    fn focused_field_places_the_native_terminal_cursor_at_the_insertion_point() {
        let area = Rect::new(0, 0, 80, 24);
        let mut app = AppState::test_new();
        app.worktree_create = Some(worktree_create_with(
            Some(claude_fork_plan()),
            "abc", // seed editor: cursor at end (3)
            WorktreeCreateFocus::Seed,
        ));

        let mut terminal =
            Terminal::new(TestBackend::new(80, 24)).expect("test terminal should initialize");
        terminal
            .draw(|frame| render_new_linked_worktree_overlay(&app, frame, area))
            .expect("overlay renders");

        let pos = terminal
            .get_cursor_position()
            .expect("a focused field must set the terminal cursor");
        let inner = new_linked_worktree_inner_rect(area, true).unwrap();
        // Seed input is row 4 of the inner rect; cursor sits after the leading
        // space + the 3 chars "abc".
        assert_eq!(pos.y, inner.y + 4, "cursor on the seed row");
        assert_eq!(pos.x, inner.x + 1 + 3, "cursor after the seed text");
    }

    #[test]
    fn plain_worktree_dialog_omits_the_seed_row() {
        let mut app = AppState::test_new();
        app.worktree_create = Some(worktree_create_with(None, "", WorktreeCreateFocus::Branch));

        let rendered = render_worktree_dialog(&app);
        assert!(rendered.contains("new worktree"));
        assert!(
            !rendered.contains("seed prompt"),
            "a plain new-worktree has no seed row"
        );
    }

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
