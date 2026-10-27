//! Native geometry and paint share one prepared cut; no Model or runtime access.
use super::Pane;
use crate::{
    api::error_code::ErrorCode,
    app::AppState,
    layout::{PaneId, PaneInfo},
    structured::view::{placement::View, Target},
};
use ratatui::{
    style::Style,
    widgets::{Block, Paragraph},
    Frame,
};
use std::collections::HashMap;
/// Pure presentation data scoped to the active client's computed pane geometry.
pub(crate) struct Prepared {
    pub target: Target,
    pub pane: Result<Pane, ErrorCode>,
}
fn view(app: &AppState, id: PaneId) -> Option<&View> {
    app.active
        .and_then(|i| app.workspaces.get(i))?
        .active_tab()?
        .native_views
        .get(&id)
}
pub(crate) fn validate(app: &AppState, id: PaneId, target: &Target) -> Result<(), ErrorCode> {
    target.validate(app).map_err(|_| ErrorCode::AgentNotFound)?;
    let tab = app
        .active
        .and_then(|i| app.workspaces.get(i))
        .and_then(|w| w.active_tab())
        .ok_or(ErrorCode::AgentNotFound)?;
    if !tab.membership_valid() {
        return Err(ErrorCode::InternalError);
    }
    if !matches!(tab.content(id), Ok(crate::workspace::Content::Native(view)) if view.target() == Some(target))
    {
        return Err(ErrorCode::AgentNotFound);
    }
    Ok(())
}
pub(crate) fn prepare(app: &AppState) -> HashMap<PaneId, Prepared> {
    app.view
        .pane_infos
        .iter()
        .filter_map(|info| {
            let target = view(app, info.id)?.target()?.clone();
            let pane = validate(app, info.id, &target).and_then(|()| {
                match app.native_frames.get(&target.agent_id) {
                    Some(Ok(frame))
                        if frame.target == target && frame.agent.agent_id == target.agent_id =>
                    {
                        Ok(Pane::compute(
                            frame,
                            info.rect,
                            Style::default().fg(crate::ui::panes::pane_focus_color(
                                info.is_focused,
                                &app.palette,
                            )),
                        ))
                    }
                    Some(Ok(_)) => Err(ErrorCode::AgentNotFound),
                    Some(Err(code)) => Err(*code),
                    None => Err(ErrorCode::SessionBusy),
                }
            });
            Some((info.id, Prepared { target, pane }))
        })
        .collect()
}
/// Every native member is handled before terminal dispatch, including typed failures.
pub(crate) fn render_native(app: &AppState, frame: &mut Frame, info: &PaneInfo) -> bool {
    let Some(view) = view(app, info.id) else {
        return false;
    };
    if view.render_unavailable(frame, info.rect) {
        return true;
    }
    let result = app
        .view
        .structured
        .get(&info.id)
        .ok_or(ErrorCode::SessionBusy)
        .and_then(|prepared| {
            validate(app, info.id, &prepared.target)?;
            prepared.pane.as_ref().map_err(|code| *code)
        });
    match result {
        Ok(pane) => {
            pane.render(frame);
            if let Some((target, index)) = &app.native_control {
                if view.target() == Some(target) && info.is_focused {
                    if let Some(&(x, y)) = pane.positions().get(*index) {
                        frame.buffer_mut()[(x, y)].set_style(Style::default().reversed());
                    }
                }
            }
        }
        Err(code) => frame.render_widget(
            Paragraph::new(format!("Structured view: {}", code.as_str())).block(Block::bordered()),
            info.rect,
        ),
    }
    true
}
