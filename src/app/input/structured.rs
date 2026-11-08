//! Native controls and D3a text edits remain typed; no terminal-byte fallback.
use super::*;
use crate::{
    api::error_code::ErrorCode,
    layout::PaneId,
    structured::view::{
        contracts::{Action, Intent},
        Target,
    },
};
impl App {
    fn native_target(&self, id: PaneId) -> Option<Target> {
        self.state
            .active
            .and_then(|i| self.state.workspaces.get(i))?
            .active_tab()?
            .native_views
            .get(&id)?
            .target()
            .cloned()
    }
    fn native_input_target(&self) -> Option<(PaneId, Target)> {
        let id = self
            .state
            .active
            .and_then(|i| self.state.workspaces.get(i))?
            .focused_pane_id()?;
        Some((id, self.native_target(id)?))
    }
    pub(super) fn refuse_structured_paste(&mut self) -> bool {
        if self.state.visible_float_for_active_workspace().is_some()
            || self.native_input_target().is_none()
        {
            return false;
        }
        self.structured_error(ErrorCode::InvalidParams);
        true
    }
    fn structured_error(&mut self, code: ErrorCode) {
        self.state.toast = Some(crate::app::state::ToastNotification {
            kind: crate::app::state::ToastKind::NeedsAttention,
            title: "Structured action refused".into(),
            context: code.as_str().into(),
            position: None,
            target: None,
        });
    }
    fn activate_native(&mut self, id: PaneId, x: u16, y: u16) {
        let Some(prepared) = self.state.view.structured.remove(&id) else {
            return;
        };
        let result = crate::ui::structured::validate(&self.state, id, &prepared.target)
            .and(prepared.pane)
            .and_then(|pane| match pane.into_intent(x, y) {
                Some(intent) => self.dispatch_native_intent(id, intent),
                None => Ok(()),
            });
        if let Err(code) = result {
            self.structured_error(code);
        }
    }
    /// Consume native input only after float, global command and prefix routing.
    pub(super) fn handle_structured_key(&mut self, key: TerminalKey) -> bool {
        let Some((id, target)) = self.native_input_target() else {
            return false;
        };
        // Repeat Enter must stop before Submit AND painted-control activation.
        if key.kind == crossterm::event::KeyEventKind::Release
            || (key.code == KeyCode::Enter && key.kind == crossterm::event::KeyEventKind::Repeat)
        {
            return true;
        }
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('w') {
            if let Err(code) = self.dispatch_native_intent(
                id,
                Intent {
                    target,
                    action: Action::Close,
                },
            ) {
                self.structured_error(code);
            }
            return true;
        }
        let positions = self
            .state
            .view
            .structured
            .get(&id)
            .and_then(|p| p.pane.as_ref().ok())
            .map(|p| p.positions())
            .unwrap_or_default();
        if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
            let selection = self
                .state
                .native_control
                .as_ref()
                .filter(|(t, _)| *t == target)
                .map(|(_, i)| *i);
            let selected = selection.unwrap_or(0);
            match key.code {
                KeyCode::Tab | KeyCode::BackTab if !positions.is_empty() => {
                    let n = positions.len();
                    let back = key.code == KeyCode::BackTab || key.modifiers == KeyModifiers::SHIFT;
                    self.state.native_control = Some((
                        target,
                        if let Some(selected) = selection {
                            if back {
                                (selected + n - 1) % n
                            } else {
                                (selected + 1) % n
                            }
                        } else if back {
                            n - 1
                        } else {
                            0
                        },
                    ));
                }
                KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Left | KeyCode::Right => {
                    use crate::structured::view::editor::Edit;
                    let edit = match key.code {
                        KeyCode::Char(c) => Edit::Insert(c),
                        KeyCode::Backspace => Edit::Backspace,
                        KeyCode::Left => Edit::Left,
                        _ => Edit::Right,
                    };
                    if let Err(code) = self.dispatch_native_intent(
                        id,
                        Intent {
                            target,
                            action: Action::EditComposer(edit),
                        },
                    ) {
                        self.structured_error(code);
                    }
                }
                KeyCode::Enter if selection.is_none() => {
                    let draft = self
                        .state
                        .native_editors
                        .get(&target.agent_id)
                        .map(|e| e.cut.composer.clone());
                    let result = draft.ok_or(ErrorCode::SessionBusy).and_then(|draft| {
                        self.dispatch_native_intent(
                            id,
                            Intent {
                                target,
                                action: Action::Send(draft),
                            },
                        )
                    });
                    if let Err(code) = result {
                        self.structured_error(code);
                    }
                }
                KeyCode::Enter => {
                    if let Some(&(x, y)) = positions.get(selected) {
                        self.activate_native(id, x, y);
                    }
                }
                KeyCode::PageUp | KeyCode::PageDown | KeyCode::End => {
                    let action = match key.code {
                        KeyCode::PageUp => Action::ScrollLines(-1),
                        KeyCode::PageDown => Action::ScrollLines(1),
                        _ => Action::FollowTail,
                    };
                    if let Err(code) = self.dispatch_native_intent(id, Intent { target, action }) {
                        self.structured_error(code);
                    }
                }
                _ => {}
            }
        }
        true
    }
    pub(super) fn handle_structured_mouse(&mut self, mouse: MouseEvent) -> bool {
        if self.state.mode != Mode::Terminal || self.state.drag.is_some() {
            return false;
        }
        let Some(id) = self.state.pane_at(mouse.column, mouse.row).map(|i| i.id) else {
            return false;
        };
        let Some(target) = self.native_target(id) else {
            return false;
        };
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if mouse.modifiers.is_empty() => {
                let hit = self
                    .state
                    .view
                    .structured
                    .get(&id)
                    .and_then(|p| p.pane.as_ref().ok())
                    .and_then(|p| p.hit(mouse.column, mouse.row));
                if hit.is_none() {
                    return false;
                }
                if let Err(code) = crate::ui::structured::validate(&self.state, id, &target) {
                    self.structured_error(code);
                    return true;
                }
                self.state.focus_pane(id);
                self.activate_native(id, mouse.column, mouse.row);
                true
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let action = Action::ScrollLines(if mouse.kind == MouseEventKind::ScrollUp {
                    -1
                } else {
                    1
                });
                if let Err(code) = self.dispatch_native_intent(id, Intent { target, action }) {
                    self.structured_error(code);
                }
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests;
