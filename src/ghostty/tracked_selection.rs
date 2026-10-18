//! Content references for ADR-0040 §1. All operations run under the pane lock.
use super::{ffi, ghostty_screen_point, ActiveScreen, GhosttyResultExt, Terminal};
use std::{ffi::c_void, ptr};

unsafe extern "C" {
    fn ghostty_terminal_grid_ref_track(
        terminal: ffi::GhosttyTerminal_ptr,
        point: ffi::GhosttyPoint,
        out: *mut *mut c_void,
    ) -> ffi::GhosttyResult;
    fn ghostty_tracked_grid_ref_free(reference: *mut c_void);
    fn ghostty_tracked_grid_ref_point(
        reference: *mut c_void,
        tag: ffi::GhosttyPointTag,
        out: *mut ffi::GhosttyPointCoordinate,
    ) -> ffi::GhosttyResult;
    fn ghostty_tracked_grid_ref_set(
        reference: *mut c_void,
        terminal: ffi::GhosttyTerminal_ptr,
        point: ffi::GhosttyPoint,
    ) -> ffi::GhosttyResult;
}

/// Owned endpoints follow scrollback and reflow, and refuse after eviction.
/// The terminal owns mutation bookkeeping; this handle owns only its pins.
/// Pins may be freed after the terminal, as guaranteed by grid_ref_tracked.h.
pub(crate) struct TrackedSelection {
    points: [*mut c_void; 2],
    owner: ffi::GhosttyTerminal_ptr,
    screen: ActiveScreen,
}
// SAFETY: handles stay inside GhosttyPaneCore and every access, mutation and
// drop is serialized by its mutex or exclusive ownership during destruction.
unsafe impl Send for TrackedSelection {}

impl TrackedSelection {
    /// Capture both directed endpoints in the terminal active screen.
    ///
    /// # Safety
    /// Keep the handle and its terminal behind the same lock, including pin
    /// destruction, or under the same exclusive owner during destruction.
    pub(crate) unsafe fn new(terminal: &Terminal, points: [(u32, u16); 2]) -> Option<Self> {
        let mut result = Self {
            points: [ptr::null_mut(); 2],
            owner: terminal.raw,
            screen: terminal.active_screen().ok()?,
        };
        for (reference, (row, col)) in result.points.iter_mut().zip(points) {
            // SAFETY: terminal is live and the out pointer is an owned slot.
            unsafe {
                ghostty_terminal_grid_ref_track(
                    terminal.raw,
                    ghostty_screen_point(col, row),
                    reference,
                )
                .into_result()
                .ok()?;
            }
        }
        Some(result)
    }

    /// Resolve both endpoints, refusing eviction, reset or a different screen.
    pub(crate) fn points(&self, terminal: &Terminal) -> Option<[(u32, u16); 2]> {
        if self.owner != terminal.raw || self.screen != terminal.active_screen().ok()? {
            return None;
        }
        let mut result = [(0, 0); 2];
        for (reference, out) in self.points.iter().zip(result.iter_mut()) {
            let mut point = ffi::GhosttyPointCoordinate::default();
            // SAFETY: the owned pin is live, serialized with terminal mutations.
            unsafe {
                ghostty_tracked_grid_ref_point(
                    *reference,
                    ffi::GhosttyPointTag_GHOSTTY_POINT_TAG_SCREEN,
                    &mut point,
                )
                .into_result()
                .ok()?;
            }
            *out = (point.y, point.x);
        }
        Some(result)
    }

    /// Move endpoints only after validating their original content survives.
    pub(crate) fn update(&mut self, terminal: &Terminal, points: [(u32, u16); 2]) -> Option<()> {
        self.points(terminal)?;
        for (reference, (row, col)) in self.points.iter().zip(points) {
            // SAFETY: the live pin belongs to this terminal and is serialized.
            unsafe {
                ghostty_tracked_grid_ref_set(
                    *reference,
                    terminal.raw,
                    ghostty_screen_point(col, row),
                )
                .into_result()
                .ok()?;
            }
        }
        Some(())
    }
}
impl Drop for TrackedSelection {
    fn drop(&mut self) {
        for reference in self.points {
            // SAFETY: each owned pin is freed once. NULL is allowed by the API.
            unsafe {
                ghostty_tracked_grid_ref_free(reference);
            }
        }
    }
}
