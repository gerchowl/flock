//! App owns task lifetime; pure state receives immutable frames, never Model borrows.
use super::App;
use crate::{
    api::error_code::ErrorCode,
    structured::{
        launch::PermissionMode,
        view::{
            driver::{Driver, Facts},
            Target,
        },
    },
};
use std::{collections::HashMap, sync::Arc};
impl App {
    pub(in crate::app) fn sync_structured_views(&mut self) -> bool {
        let wanted: HashMap<String, Target> = self
            .state
            .workspaces
            .iter()
            .flat_map(|w| &w.tabs)
            .flat_map(|t| t.native_views.values())
            .filter_map(|v| v.target())
            .map(|t| (t.agent_id.clone(), t.clone()))
            .collect();
        self.structured_views
            .retain(|id, d| wanted.get(id) == Some(&d.target));
        let before = self.state.native_frames.len();
        self.state
            .native_frames
            .retain(|id, _| wanted.contains_key(id));
        let mut changed = before != self.state.native_frames.len();
        for (id, target) in wanted {
            let info = target
                .validate(&self.state)
                .map_err(|_| ErrorCode::AgentNotFound)
                .and_then(|()| self.structured_info(&id));
            if info.is_err() {
                self.structured_views.remove(&id);
            }
            let frame = info.and_then(|agent| {
                if let Some(driver) = self.structured_views.get(&id) {
                    driver.refresh(Facts { target, agent });
                    driver.frame()
                } else {
                    // LaunchSettings currently supports exactly this explicit mode.
                    let stream = self
                        .open_structured_view(target, PermissionMode::Default)
                        .map_err(|err| {
                            err.get_ref()
                                .and_then(|e| e.downcast_ref::<ErrorCode>())
                                .copied()
                                .unwrap_or_else(|| crate::cli::output::io_error_code(&err))
                        })?;
                    let driver = Driver::new(stream, self.render_notify.clone());
                    let frame = driver.frame();
                    self.structured_views.insert(id.clone(), driver);
                    frame
                }
            });
            changed |= match (self.state.native_frames.get(&id), &frame) {
                (Some(Ok(a)), Ok(b)) => !Arc::ptr_eq(a, b),
                (Some(Err(a)), Err(b)) => a != b,
                _ => true,
            };
            self.state.native_frames.insert(id, frame);
        }
        changed
    }
}
