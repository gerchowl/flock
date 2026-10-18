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
    erase_generation: u64,
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
            erase_generation: terminal.selection_erases.generation,
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
        if self.owner != terminal.raw
            || self.screen != terminal.active_screen().ok()?
            || self.erase_generation != terminal.selection_erases.generation
            || self.erase_generation == u64::MAX
        {
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

/// Flock-side erase identity: native grid pins survive destructive ED commands.
/// Tracks control syntax across writes without retaining an unbounded payload.
#[derive(Default)]
pub(super) struct SelectionEraseTracker {
    state: EraseState,
    parameter: u16,
    valid: bool,
    parameter_started: bool,
    private: bool,
    screen_clear: bool,
    pub(super) generation: u64,
}

#[derive(Default, Clone, Copy)]
enum EraseState {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    Osc,
    String,
}

impl SelectionEraseTracker {
    pub(super) fn invalidate(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }

    pub(super) fn write(&mut self, bytes: &[u8]) {
        use EraseState::*;
        for &byte in bytes {
            // Ground bytes pass through Ghostty's UTF-8 decoder: raw C1 bytes
            // there are text, not controls. In other states its VT table applies.
            let next = match byte {
                0x1b => Some(Escape),
                0x18 | 0x1a => Some(Ground),
                0x9b if !matches!(self.state, Ground) => Some(Csi),
                0x9d if !matches!(self.state, Ground) => Some(Osc),
                0x90 | 0x98 | 0x9e | 0x9f if !matches!(self.state, Ground) => Some(String),
                0x80..=0x8f | 0x91..=0x97 | 0x99 | 0x9a | 0x9c if !matches!(self.state, Ground) => {
                    Some(Ground)
                }
                _ => None,
            };
            if let Some(next) = next {
                self.state = next;
                if matches!(next, Csi) {
                    self.start_csi();
                }
                continue;
            }
            match self.state {
                Ground => {}
                Escape => match byte {
                    b'[' => self.start_csi(),
                    b']' => self.state = Osc,
                    b'P' | b'X' | b'^' | b'_' => self.state = String,
                    0x20..=0x2f => self.state = EscapeIntermediate,
                    0x30..=0x7e => {
                        if byte == b'c' {
                            self.invalidate();
                        }
                        self.state = Ground;
                    }
                    _ => {}
                },
                EscapeIntermediate if (0x30..=0x7e).contains(&byte) => self.state = Ground,
                Csi => match byte {
                    b'?' if !self.parameter_started && !self.private => self.private = true,
                    b'0'..=b'9' => {
                        self.parameter_started = true;
                        match self
                            .parameter
                            .checked_mul(10)
                            .and_then(|n| n.checked_add(u16::from(byte - b'0')))
                        {
                            Some(n) => self.parameter = n,
                            None => self.valid = false,
                        }
                    }
                    b';' => {
                        self.screen_clear |= matches!(self.parameter, 1047 | 1049);
                        self.parameter = 0;
                        self.valid = false;
                    }
                    0x20..=0x3f => self.valid = false,
                    0x40..=0x7e => {
                        if byte == b'J' && self.valid && matches!(self.parameter, 2 | 3) {
                            self.invalidate();
                        }
                        // 1047/1049 screen switches invoke eraseDisplay internally.
                        if self.private
                            && matches!(byte, b'h' | b'l')
                            && (self.screen_clear || matches!(self.parameter, 1047 | 1049))
                        {
                            self.invalidate();
                        }
                        self.state = Ground;
                    }
                    _ => {}
                },
                Osc if byte == 7 => self.state = Ground,
                _ => {}
            }
        }
    }

    fn start_csi(&mut self) {
        self.state = EraseState::Csi;
        self.parameter = 0;
        self.valid = true;
        self.parameter_started = false;
        self.private = false;
        self.screen_clear = false;
    }
}

#[cfg(test)]
mod erase_tests {
    use super::*;

    #[test]
    fn erases_are_tracked_across_fragmented_writes_and_controls() {
        let mut tracker = SelectionEraseTracker::default();
        for byte in b"\x1b[0003J\x1b[?2J\x1bc\x1b[?25;1049h\x1b[?1047l" {
            tracker.write(&[*byte]);
        }
        assert_eq!(tracker.generation, 5);
    }

    #[test]
    fn ordinary_output_strings_and_invalid_erases_keep_the_generation() {
        let mut tracker = SelectionEraseTracker::default();
        tracker.write(b"title [3J\x1b]0;[3J\x07\x1bP[2J\x1b\\\x1b[3;J\x1b[>3J\x1b[3\x18J\x1b[22J");
        assert_eq!(tracker.generation, 0);
    }

    #[test]
    fn no_scrollback_resize_invalidates_pins_but_normal_reflow_does_not() {
        for (capacity, expected) in [(0, 1), (256 * 1024, 0)] {
            let mut terminal = Terminal::new(40, 6, capacity).unwrap();
            terminal.write(b"selected\r\n");
            terminal.resize(20, 8, 1, 1).unwrap();
            assert_eq!(terminal.selection_erases.generation, expected);
        }
    }
}
