//! A single-line text editor for dialog input fields (#159).
//!
//! flk is a terminal emulator, so its own chrome should edit text like one:
//! a real insertion cursor, mid-string insert/delete, word ops, and horizontal
//! scroll — not an append/backspace-only buffer with a painted block. This is
//! the reusable primitive behind the branch-session dialog's branch + seed
//! fields; the render layer places the native terminal cursor at [`cursor`].
//!
//! The cursor is a *character* index in `0..=chars`, kept valid by every
//! mutation; all indexing is codepoint-aware so multi-byte input never panics.
//!
//! [`cursor`]: LineEditor::cursor

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineEditor {
    value: String,
    /// Character index of the insertion point, in `0..=char_count`.
    cursor: usize,
}

impl LineEditor {
    /// A fresh editor holding `text`, cursor at the end (ready to append/edit).
    pub fn new(text: impl Into<String>) -> Self {
        let value = text.into();
        let cursor = value.chars().count();
        Self { value, cursor }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    /// Insertion-point index. Prod rendering goes through [`view`](Self::view);
    /// this getter exists for tests that assert cursor movement directly.
    #[cfg(test)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    fn char_count(&self) -> usize {
        self.value.chars().count()
    }

    /// Byte offset of character index `char_idx` (end-of-string for `>= len`).
    fn byte_of(&self, char_idx: usize) -> usize {
        self.value
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.value.len())
    }

    /// Replace the whole value, cursor to the end. Used to (re)seed a field.
    pub fn set(&mut self, text: impl Into<String>) {
        self.value = text.into();
        self.cursor = self.char_count();
    }

    // --- editing ---------------------------------------------------------

    pub fn insert(&mut self, c: char) {
        let at = self.byte_of(self.cursor);
        self.value.insert(at, c);
        self.cursor += 1;
    }

    /// Delete the character before the cursor (Backspace).
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.byte_of(self.cursor - 1);
        let end = self.byte_of(self.cursor);
        self.value.replace_range(start..end, "");
        self.cursor -= 1;
    }

    /// Delete the character at the cursor (Delete/^D).
    pub fn delete(&mut self) {
        if self.cursor >= self.char_count() {
            return;
        }
        let start = self.byte_of(self.cursor);
        let end = self.byte_of(self.cursor + 1);
        self.value.replace_range(start..end, "");
    }

    /// Delete the word before the cursor (^W): trailing whitespace, then the
    /// run of non-whitespace.
    pub fn delete_word_back(&mut self) {
        let mut target = self.cursor;
        let chars: Vec<char> = self.value.chars().collect();
        while target > 0 && chars[target - 1].is_whitespace() {
            target -= 1;
        }
        while target > 0 && !chars[target - 1].is_whitespace() {
            target -= 1;
        }
        let start = self.byte_of(target);
        let end = self.byte_of(self.cursor);
        self.value.replace_range(start..end, "");
        self.cursor = target;
    }

    /// Delete from the cursor to the start of the line (^U).
    pub fn delete_to_start(&mut self) {
        let end = self.byte_of(self.cursor);
        self.value.replace_range(0..end, "");
        self.cursor = 0;
    }

    #[cfg(test)]
    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
    }

    // --- movement --------------------------------------------------------

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        if self.cursor < self.char_count() {
            self.cursor += 1;
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.char_count();
    }

    // --- rendering -------------------------------------------------------

    /// Horizontal-scroll window for a box `width` columns wide: the visible
    /// substring and the cursor's column *within it*. Scrolls so the cursor
    /// stays on-screen — anchored left until the cursor passes the right edge,
    /// then follows it. `width == 0` yields an empty view.
    ///
    /// (Assumes one column per character — fine for the branch/seed fields;
    /// wide-glyph handling can come with multi-line support.)
    pub fn view(&self, width: usize) -> (String, usize) {
        if width == 0 {
            return (String::new(), 0);
        }
        let chars: Vec<char> = self.value.chars().collect();
        // Left edge of the window: keep the cursor within `[start, start+width)`.
        let start = self.cursor.saturating_sub(width.saturating_sub(1));
        let visible: String = chars.iter().skip(start).take(width).collect();
        (visible, self.cursor - start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_places_cursor_at_end() {
        let e = LineEditor::new("abc");
        assert_eq!(e.value(), "abc");
        assert_eq!(e.cursor(), 3);
    }

    #[test]
    fn insert_at_cursor_shifts_the_tail() {
        let mut e = LineEditor::new("ac");
        e.left(); // cursor between a and c
        e.insert('b');
        assert_eq!(e.value(), "abc");
        assert_eq!(e.cursor(), 2);
    }

    #[test]
    fn backspace_and_delete_are_cursor_relative() {
        let mut e = LineEditor::new("abc");
        e.left(); // before 'c'
        e.backspace(); // removes 'b'
        assert_eq!(e.value(), "ac");
        assert_eq!(e.cursor(), 1);
        e.delete(); // removes 'c' at cursor
        assert_eq!(e.value(), "a");
        assert_eq!(e.cursor(), 1);
        // Backspace at start and delete at end are no-ops.
        e.home();
        e.backspace();
        e.end();
        e.delete();
        assert_eq!(e.value(), "a");
    }

    #[test]
    fn movement_clamps_at_both_ends() {
        let mut e = LineEditor::new("ab");
        e.right();
        e.right(); // clamped at 2
        assert_eq!(e.cursor(), 2);
        e.home();
        assert_eq!(e.cursor(), 0);
        e.left(); // clamped at 0
        assert_eq!(e.cursor(), 0);
        e.end();
        assert_eq!(e.cursor(), 2);
    }

    #[test]
    fn delete_word_back_eats_trailing_space_then_word() {
        let mut e = LineEditor::new("land the feature ");
        e.delete_word_back();
        assert_eq!(e.value(), "land the ");
        e.delete_word_back();
        assert_eq!(e.value(), "land ");
    }

    #[test]
    fn delete_word_back_respects_mid_string_cursor() {
        let mut e = LineEditor::new("one two three");
        e.home();
        e.right();
        e.right();
        e.right(); // after "one"
        e.delete_word_back(); // removes "one"
        assert_eq!(e.value(), " two three");
        assert_eq!(e.cursor(), 0);
    }

    #[test]
    fn delete_to_start_and_clear() {
        let mut e = LineEditor::new("hello world");
        e.home();
        e.right();
        e.right();
        e.right();
        e.right();
        e.right(); // after "hello"
        e.delete_to_start();
        assert_eq!(e.value(), " world");
        assert_eq!(e.cursor(), 0);
        e.clear();
        assert!(e.is_empty());
        assert_eq!(e.cursor(), 0);
    }

    #[test]
    fn editing_is_codepoint_safe() {
        let mut e = LineEditor::new("café");
        assert_eq!(e.cursor(), 4);
        e.backspace(); // remove 'é'
        assert_eq!(e.value(), "caf");
        e.set("naïve ✓");
        e.home();
        e.right();
        e.right(); // between 'a' and 'ï'
        e.insert('X');
        assert_eq!(e.value(), "naXïve ✓");
    }

    #[test]
    fn view_scrolls_to_keep_the_cursor_visible() {
        let mut e = LineEditor::new("abcdefgh"); // cursor at 8 (end)
        let (shown, col) = e.view(4);
        assert_eq!(shown, "fgh", "window follows the cursor at the tail");
        assert_eq!(col, 3, "cursor sits just past the last visible char");

        e.home(); // cursor at 0
        let (shown, col) = e.view(4);
        assert_eq!(shown, "abcd");
        assert_eq!(col, 0);

        // A value shorter than the window shows in full.
        let short = LineEditor::new("hi");
        let (shown, col) = short.view(10);
        assert_eq!(shown, "hi");
        assert_eq!(col, 2);

        // Zero width is a clean empty view.
        assert_eq!(e.view(0), (String::new(), 0));
    }
}
