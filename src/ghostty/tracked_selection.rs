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
        let screen = terminal.active_screen().ok()?;
        let mut result = Self {
            points: [ptr::null_mut(); 2],
            owner: terminal.raw,
            screen,
            erase_generation: terminal.selection_erases.generation(screen),
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
            || self.erase_generation != terminal.selection_erases.generation(self.screen)
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
    parameters: [u16; 24],
    parameters_len: usize,
    colon: bool,
    screen: Option<ActiveScreen>,
    screens: [ScreenEraseFacts; 2],
}

/// Facts belong to the screen containing the pinned cells, never its peer.
#[derive(Default)]
struct ScreenEraseFacts {
    generation: u64,
    loss_on_resize: bool,
}

#[derive(Default, Clone, Copy)]
enum EraseState {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate(bool),
    Csi,
    Osc,
    String,
}

impl SelectionEraseTracker {
    /// Mirror native screen creation policy without querying inactive history.
    pub(super) fn new(primary_no_scrollback: bool) -> Self {
        Self {
            screen: Some(ActiveScreen::Primary),
            screens: [
                ScreenEraseFacts {
                    generation: 0,
                    loss_on_resize: primary_no_scrollback,
                },
                // Terminal.switchScreen always creates alternate with max_scrollback=0.
                ScreenEraseFacts {
                    generation: 0,
                    loss_on_resize: true,
                },
            ],
            ..Self::default()
        }
    }

    fn index(screen: ActiveScreen) -> usize {
        match screen {
            ActiveScreen::Primary => 0,
            ActiveScreen::Alternate => 1,
        }
    }

    /// Identity of one screen; saturation permanently refuses its old anchors.
    pub(super) fn generation(&self, screen: ActiveScreen) -> u64 {
        self.screens[Self::index(screen)].generation
    }

    fn invalidate_screen(&mut self, screen: ActiveScreen) {
        let facts = &mut self.screens[Self::index(screen)];
        facts.generation = facts.generation.saturating_add(1);
    }

    fn invalidate_active(&mut self) {
        match self.screen {
            Some(screen) => self.invalidate_screen(screen),
            // XTRESTORE does not reveal its saved value. A later erase in the
            // same chunk may target either screen, so refuse both until resynced.
            None => self.invalidate_all(),
        }
    }

    fn invalidate_all(&mut self) {
        self.invalidate_screen(ActiveScreen::Primary);
        self.invalidate_screen(ActiveScreen::Alternate);
    }

    /// Record native resize history loss independently for each screen.
    pub(super) fn resize(&mut self) {
        for facts in &mut self.screens {
            if facts.loss_on_resize {
                facts.generation = facts.generation.saturating_add(1);
            }
        }
    }

    /// Restore authoritative active-screen knowledge around each native write.
    pub(super) fn synchronize_screen(&mut self, screen: Option<ActiveScreen>) {
        self.screen = screen;
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
                    0x20..=0x2f => self.state = EscapeIntermediate(byte == b'#'),
                    0x30..=0x7e => {
                        if byte == b'c' {
                            self.invalidate_all();
                            self.screen = Some(ActiveScreen::Primary);
                        }
                        self.state = Ground;
                    }
                    _ => {}
                },
                EscapeIntermediate(decaln) => match byte {
                    0x20..=0x2f => self.state = EscapeIntermediate(false),
                    0x30..=0x7e => {
                        if decaln && byte == b'8' {
                            self.invalidate_active();
                        }
                        self.state = Ground;
                    }
                    _ => {}
                },
                Csi => match byte {
                    b'?' if !self.parameter_started
                        && self.parameters_len == 0
                        && !self.private =>
                    {
                        self.private = true
                    }
                    b'0'..=b'9' => {
                        self.parameter_started = true;
                        self.parameter = self
                            .parameter
                            .saturating_mul(10)
                            .saturating_add(u16::from(byte - b'0'));
                    }
                    // Parser.zig saves even an empty slot. A trailing empty
                    // slot is not finalized at dispatch. Excess slots are ignored.
                    b';' | b':' if self.parameters_len < self.parameters.len() => {
                        self.parameters[self.parameters_len] = self.parameter;
                        self.parameters_len += 1;
                        self.parameter = 0;
                        self.parameter_started = false;
                        self.colon |= byte == b':';
                    }
                    b';' | b':' => {}
                    0x20..=0x3f => self.valid = false,
                    0x40..=0x7e => {
                        self.dispatch_csi(byte);
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
        self.parameters_len = 0;
        self.colon = false;
    }

    /// Mirror Parser.zig's 24-slot cap, final-slot rule and non-SGR colon
    /// rejection before interpreting stream.zig's ED and screen-mode actions.
    fn dispatch_csi(&mut self, final_byte: u8) {
        if !self.valid || self.colon || self.parameters_len >= self.parameters.len() {
            return;
        }
        if self.parameter_started {
            self.parameters[self.parameters_len] = self.parameter;
            self.parameters_len += 1;
        }
        if final_byte == b'J' && matches!(&self.parameters[..self.parameters_len], [2 | 3]) {
            self.invalidate_active();
        }
        if !self.private || !matches!(final_byte, b'h' | b'l' | b'r') {
            return;
        }
        // Process mode lists in order: later erases use the newly active screen.
        for i in 0..self.parameters_len {
            match self.parameters[i] {
                3 => {
                    // DECCOLM's internal resize can lose inactive history too.
                    self.resize();
                    self.invalidate_active();
                }
                mode @ (47 | 1047 | 1049) => {
                    if mode != 47 || final_byte == b'r' {
                        self.invalidate_screen(ActiveScreen::Alternate);
                    }
                    self.screen = match final_byte {
                        b'h' => Some(ActiveScreen::Alternate),
                        b'l' => Some(ActiveScreen::Primary),
                        // Saved bits are deliberately not mirrored. Terminal.write
                        // resynchronizes from the native screen after each chunk.
                        _ => None,
                    };
                }
                _ => {}
            }
        }
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
        assert_eq!(tracker.generation(ActiveScreen::Primary), 3);
        assert_eq!(tracker.generation(ActiveScreen::Alternate), 5);
    }

    #[test]
    fn strings_and_invalid_erases_are_ignored_but_trailing_separator_erases() {
        let mut tracker = SelectionEraseTracker::default();
        tracker.write(b"title [3J\x1b]0;[3J\x07\x1bP[2J\x1b\\\x1b[3;J\x1b[>3J\x1b[3\x18J\x1b[22J");
        assert_eq!(
            tracker.generation(ActiveScreen::Primary),
            1,
            "ED 3 with a trailing separator must invalidate the selection"
        );
    }

    #[test]
    fn no_scrollback_resize_invalidates_pins_but_normal_reflow_does_not() {
        for (capacity, expected) in [(0, 1), (256 * 1024, 0)] {
            let mut terminal = Terminal::new(40, 6, capacity).unwrap();
            terminal.write(b"selected\r\n");
            terminal.resize(20, 8, 1, 1).unwrap();
            assert_eq!(
                terminal
                    .selection_erases
                    .generation(terminal.active_screen().unwrap()),
                expected
            );
        }
    }
}

#[cfg(test)]
mod parameter_tests {
    use super::*;

    #[test]
    fn parameter_forms_match_the_vendored_terminal_parser() {
        // Mode 0 clears BOTTOM, 1 clears TOP, 2 clears both, and 3 clears
        // history only. An ignored dispatch keeps every marker. Observe the
        // actual vendored parser's result independently of our generation.
        let cases = [
            ("", Some(0)),
            (";", Some(0)),
            ("0;", Some(0)),
            ("1;", Some(1)),
            ("2", Some(2)),
            ("2;", Some(2)),
            ("3", Some(3)),
            ("3;", Some(3)),
            ("0003;", Some(3)),
            ("?2;", Some(2)),
            ("?3;", Some(3)),
            ("3\x7f;", Some(3)),
            ("3\x00;", Some(3)),
            (";;", None),
            (";0;", None),
            ("3;;", None),
            ("3;0", None),
            ("3;0;", None),
            (";3", None),
            (";3;", None),
            ("2:", None),
            ("3:0", None),
            (":3", None),
            (">3;", None),
            ("3 ;", None),
            ("65539", None),
            ("3;;;;;;;;;;;;;;;;;;;;;;;;", None),
        ];
        for (parameters, mode) in cases {
            let mut terminal = Terminal::new(40, 6, 256 * 1024).unwrap();
            terminal.write(b"history\r\n".repeat(20).as_slice());
            terminal.write(b"\x1b[H TOP\x1b[6;1HBOTTOM\x1b[3;1H");
            let history = terminal.scrollbar().unwrap().total;
            let generation = terminal
                .selection_erases
                .generation(terminal.active_screen().unwrap());
            let command = format!("\x1b[{parameters}J");
            for byte in command.bytes() {
                terminal.write(&[byte]);
            }
            let text = terminal.read_text_viewport((0, 0), (39, 5), false).unwrap();
            assert_eq!(
                text.contains("TOP"),
                !matches!(mode, Some(1 | 2)),
                "native TOP: {parameters:?}"
            );
            assert_eq!(
                text.contains("BOTTOM"),
                !matches!(mode, Some(0 | 2)),
                "native BOTTOM: {parameters:?}"
            );
            assert_eq!(
                terminal.scrollbar().unwrap().total < history,
                mode == Some(3),
                "native history: {parameters:?}"
            );
            assert_eq!(
                terminal
                    .selection_erases
                    .generation(terminal.active_screen().unwrap())
                    - generation,
                u64::from(matches!(mode, Some(2 | 3))),
                "generation: {parameters:?}"
            );
        }
    }
}

#[cfg(test)]
mod mode_restore_tests {
    use super::*;

    #[test]
    fn xtrestore_screen_modes_invalidate_for_saved_true_and_unsaved_false() {
        for mode in [47, 1047, 1049] {
            for saved in [false, true] {
                let mut terminal = Terminal::new(40, 6, 256 * 1024).unwrap();
                if saved {
                    terminal.write(format!("\x1b[?{mode}h\x1b[?{mode}s\x1b[?{mode}l").as_bytes());
                }
                let generation = terminal
                    .selection_erases
                    .generation(ActiveScreen::Alternate);
                terminal.write(format!("\x1b[?{mode}r").as_bytes());
                assert_eq!(
                    terminal.active_screen().unwrap(),
                    if saved {
                        ActiveScreen::Alternate
                    } else {
                        ActiveScreen::Primary
                    }
                );
                assert_eq!(
                    terminal
                        .selection_erases
                        .generation(ActiveScreen::Alternate),
                    generation + 1,
                    "XTRESTORE mode {mode}, saved={saved}"
                );
                assert_eq!(
                    terminal.selection_erases.generation(ActiveScreen::Primary),
                    0
                );
                terminal.write(b"\x1b[?47l");
                let generation = terminal
                    .selection_erases
                    .generation(ActiveScreen::Alternate);
                terminal.resize(20, 3, 1, 1).unwrap();
                assert_eq!(
                    terminal
                        .selection_erases
                        .generation(ActiveScreen::Alternate),
                    generation + 1,
                    "inactive allocation after restore mode {mode}, saved={saved}"
                );
            }
        }
    }

    #[test]
    fn xtsave_and_decrqm_do_not_switch_or_clear_screens() {
        for mode in [47, 1047, 1049] {
            let mut terminal = Terminal::new(40, 6, 256 * 1024).unwrap();
            terminal.write(b"selected");
            let before = terminal.read_text_viewport((0, 0), (39, 5), false).unwrap();
            terminal.write(format!("\x1b[?{mode}s\x1b[?{mode}$p").as_bytes());
            assert_eq!(terminal.active_screen().unwrap(), ActiveScreen::Primary);
            assert_eq!(
                terminal
                    .selection_erases
                    .generation(ActiveScreen::Alternate),
                0
            );
            assert_eq!(
                terminal.selection_erases.generation(ActiveScreen::Primary),
                0
            );
            assert_eq!(
                terminal.read_text_viewport((0, 0), (39, 5), false).unwrap(),
                before
            );
        }
    }
}

#[cfg(test)]
mod full_screen_clear_tests {
    use super::*;

    #[test]
    fn deccolm_set_reset_and_restore_clear_both_native_screens() {
        for alternate in [false, true] {
            for final_byte in ['h', 'l', 'r'] {
                let mut terminal = Terminal::new(40, 6, 256 * 1024).unwrap();
                if alternate {
                    terminal.write(b"\x1b[?47h");
                }
                terminal.write(b"SELECTA\x1b[?40h");
                let generation = terminal
                    .selection_erases
                    .generation(terminal.active_screen().unwrap());
                for byte in format!("\x1b[?3{final_byte}").bytes() {
                    terminal.write(&[byte]);
                }
                assert_eq!(
                    terminal.cols().unwrap(),
                    if final_byte == 'h' { 132 } else { 80 }
                );
                assert!(!terminal
                    .read_text_viewport((0, 0), (39, 5), false)
                    .unwrap()
                    .contains("SELECTA"));
                assert!(
                    terminal
                        .selection_erases
                        .generation(terminal.active_screen().unwrap())
                        > generation
                );
            }
        }
        // Mode 40 is deliberately not mirrored, including across XTRESTORE.
        let mut terminal = Terminal::new(40, 6, 256 * 1024).unwrap();
        terminal.write(b"SELECTA\x1b[?3h");
        assert_eq!(terminal.cols().unwrap(), 40);
        assert_eq!(
            terminal
                .selection_erases
                .generation(terminal.active_screen().unwrap()),
            1
        );
        assert!(terminal
            .read_text_viewport((0, 0), (39, 5), false)
            .unwrap()
            .contains("SELECTA"));
    }

    #[test]
    fn decaln_matches_native_esc_intermediates_across_fragments() {
        for (bytes, clears) in [
            (b"\x1b#8".as_slice(), true),
            (b"\x1b#\x00\x7f8", true),
            (b"\x1b8", false),
            (b"\x1b##8", false),
            (b"\x1b #8", false),
            (b"\x1b#\x188", false),
            (b"\x1b]0;\x1b#8\x07", true),
            (b"\x1b]0;#8\x07", false),
            (b"\x1bP#8\x1b\\", false),
        ] {
            let mut terminal = Terminal::new(40, 6, 256 * 1024).unwrap();
            terminal.write(b"SELECTA");
            for byte in bytes {
                terminal.write(&[*byte]);
            }
            let text = terminal.read_text_viewport((0, 0), (39, 5), false).unwrap();
            assert_eq!(text.contains("SELECTA"), !clears, "native: {bytes:?}");
            assert_eq!(
                terminal
                    .selection_erases
                    .generation(terminal.active_screen().unwrap()),
                u64::from(clears),
                "generation: {bytes:?}"
            );
        }
    }
}

#[cfg(test)]
mod screen_scope_tests {
    use super::*;

    #[test]
    fn one_chunk_alternate_erases_do_not_change_primary_identity() {
        for mode in [47, 1047, 1049] {
            let mut terminal = Terminal::new(40, 6, 256 * 1024).unwrap();
            terminal.write(b"SELECTA");
            terminal.write(format!("\x1b[?{mode}hALT\x1b[3J\x1b[?{mode}l").as_bytes());
            assert_eq!(terminal.active_screen().unwrap(), ActiveScreen::Primary);
            assert_eq!(
                terminal.selection_erases.generation(ActiveScreen::Primary),
                0
            );
            assert!(
                terminal
                    .selection_erases
                    .generation(ActiveScreen::Alternate)
                    > 0
            );
            assert!(terminal
                .read_text_viewport((0, 0), (39, 5), false)
                .unwrap()
                .contains("SELECTA"));
        }
    }

    #[test]
    fn unknown_restored_screen_erases_fail_safe_until_native_resynchronization() {
        let mut terminal = Terminal::new(40, 6, 256 * 1024).unwrap();
        terminal.write(b"\x1b[?1049r\x1b[3J");
        assert!(terminal.selection_erases.generation(ActiveScreen::Primary) > 0);
        assert!(
            terminal
                .selection_erases
                .generation(ActiveScreen::Alternate)
                > 0
        );
        let primary = terminal.selection_erases.generation(ActiveScreen::Primary);
        terminal.write(b"\x1b[?47h\x1b[3J\x1b[?47l");
        assert_eq!(
            terminal.selection_erases.generation(ActiveScreen::Primary),
            primary
        );
    }
}
