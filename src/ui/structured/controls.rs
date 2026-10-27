//! Prepared permission/composer regions (ADR-0036 §11); no authority or effects.
use super::markdown::{wrap, DisplayLine};
use crate::structured::view::contracts::{Action, Admission, Composer, PermissionHandle, Snapshot};
use crate::structured_wire::{PermissionDecision, PermissionState};
use ratatui::{layout::Rect, text::Line, widgets::Paragraph, Frame};

/// Sanitized physical rows and their exact intent rectangles, computed together.
/// Regions require four rows; smaller regions omit controls rather than clipping hits.
/// Full-input intents retain the exact handle; S10e owns inspection and confirmation.
#[derive(Default)]
pub(crate) struct Panel {
    rows: Vec<(Rect, Vec<DisplayLine>)>,
    hits: Vec<(Rect, Action)>,
}
impl Panel {
    /// Match the painted button, never derive an action from untrusted text.
    pub fn hit(&self, x: u16, y: u16) -> Option<&Action> {
        self.hits
            .iter()
            .find(|(r, _)| r.contains((x, y).into()))
            .map(|(_, a)| a)
    }
    /// Immutable paint. Input dispatch must revalidate generation and book state.
    pub fn render(&self, f: &mut Frame) {
        // Prepared-row cloning is tracked in #2590.
        for (rect, rows) in &self.rows {
            f.render_widget(
                Paragraph::new(rows.iter().map(|r| r.line().clone()).collect::<Vec<_>>()),
                *rect,
            );
        }
    }
    /// Prepare a bounded, sanitized region for the pane composition.
    pub(super) fn text(&mut self, area: Rect, text: String) {
        let mut rows = wrap(
            text.split('\n').map(|s| Line::from(s.to_owned())).collect(),
            area.width,
        );
        rows.truncate(usize::from(area.height));
        self.rows.push((area, rows));
    }
    /// Register only complete, painted buttons; omitted labels have no hit.
    pub(super) fn buttons(&mut self, area: Rect, actions: Vec<(&str, Action)>) {
        let mut x: u16 = 0;
        for (label, action) in actions {
            let width = label.len() as u16;
            if area.height > 0 && x.saturating_add(width) <= area.width {
                let rect = Rect::new(area.x + x, area.y, width, 1);
                self.text(rect, label.into());
                self.hits.push((rect, action));
                x = x.saturating_add(width + 1);
            }
        }
    }
}
fn preview(area: Rect) -> Rect {
    Rect {
        height: area.height.min(2),
        ..area
    }
}
fn row(area: Rect, offset: u16) -> Rect {
    Rect::new(
        area.x,
        area.y.saturating_add(offset.min(area.height)),
        area.width,
        u16::from(offset < area.height),
    )
}
/// Show only the final record for each exact handle. A stale selection never retargets consent.
pub(crate) fn permission(s: &Snapshot, area: Rect) -> Panel {
    let mut p = Panel::default();
    let open: Vec<_> = s
        .permissions
        .iter()
        .enumerate()
        .filter(|(i, a)| {
            matches!(a.state, PermissionState::Open { .. })
                && !s.permissions[i + 1..]
                    .iter()
                    .any(|b| a.request_id == b.request_id && a.request_seq == b.request_seq)
        })
        .map(|(_, a)| a)
        .collect();
    let selected = s
        .selected_permission
        .as_ref()
        .and_then(|h| {
            open.iter()
                .position(|r| r.request_id == h.request_id && r.request_seq == h.request_seq)
        })
        .or_else(|| s.selected_permission.is_none().then_some(0));
    let handle = |r: &crate::structured_wire::PermissionRecord| PermissionHandle {
        request_id: r.request_id.clone(),
        request_seq: r.request_seq,
    };
    if let Some((i, record)) = selected.and_then(|i| open.get(i).map(|r| (i, *r))) {
        if let PermissionState::Open { request } = &record.state {
            p.text(
                preview(area),
                format!(
                    "Permission {}/{}: {} {}@{}\nInput: {}",
                    i + 1,
                    open.len(),
                    request.tool_name,
                    record.request_id,
                    record.request_seq,
                    serde_json::Value::Object(request.input.clone())
                ),
            );
        }
        p.buttons(
            row(area, 2),
            vec![
                ("[Full input]", Action::InspectPermission(handle(record))),
                (
                    "[Next]",
                    Action::SelectPermission(handle(open[(i + 1) % open.len()])),
                ),
            ],
        );
        p.buttons(
            row(area, 3),
            [
                ("[Allow once]", PermissionDecision::AllowOnce),
                ("[Always (session)...]", PermissionDecision::AllowAlways),
                ("[Deny]", PermissionDecision::Deny),
            ]
            .into_iter()
            .map(|(label, decision)| {
                (
                    label,
                    Action::Decide {
                        handle: handle(record),
                        decision,
                        message: None,
                    },
                )
            })
            .collect(),
        );
    } else {
        p.text(preview(area), "Permission: none selected / closed".into());
        if let Some(next) = open.first() {
            p.buttons(
                row(area, 2),
                vec![("[Next]", Action::SelectPermission(handle(next)))],
            );
        }
    }
    p
}
/// Preserve the exact UTF-8 draft and byte cursor; uncertain admissions have no Send hit.
/// The first two rows preview wrapped draft text; input/history mutation belongs to S10d.
pub(crate) fn composer(s: &Snapshot, area: Rect) -> Panel {
    let mut p = Panel::default();
    p.text(preview(area), format!("> {}", s.composer.text));
    let status = match &s.admission {
        Admission::Ready => "ready".into(),
        Admission::Pending => "pending; retain draft".into(),
        Admission::Refused(c) => format!("refused: {}", c.as_str()),
    };
    p.text(row(area, 3), format!("Admission: {status}"));
    let mut buttons = vec![("[History]", Action::ComposerHistory(-1))];
    if !matches!(s.admission, Admission::Pending) {
        buttons.insert(
            0,
            (
                "[Send at idle]",
                Action::Send(Composer {
                    text: s.composer.text.clone(),
                    cursor: s.composer.cursor,
                }),
            ),
        );
    }
    p.buttons(row(area, 2), buttons);
    p
}
#[cfg(test)]
mod tests;
