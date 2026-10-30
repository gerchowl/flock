//! App-owned editor authority (ADR-0036 S10d3). Projection never owns edits.
use super::{
    contracts::{Admission, Composer},
    Target,
};
use crate::api::error_code::ErrorCode;
use unicode_segmentation::UnicodeSegmentation;

/// UTF-8 bytes in a draft; insertion refuses atomically rather than truncating.
pub(crate) const COMPOSER_MAX_BYTES: usize = 64 * 1024;
/// Minimal MVP operations. Other key bindings remain outside D3a.
pub(crate) enum Edit {
    Insert(char),
    Backspace,
    Left,
    Right,
    Focus,
}
/// One coherent immutable projection, including the last admission identity.
#[derive(Clone)]
pub(crate) struct EditorCut {
    pub composer: Composer,
    pub admission: Admission,
    pub receipt: Option<String>,
}
/// Pure authority scoped to exactly one driver lifetime, not a painted frame.
pub(crate) struct EditorState {
    pub target: Target,
    pub lifetime: u64,
    pub cut: EditorCut,
}
fn revision(counter: &mut u64) -> Result<u64, ErrorCode> {
    *counter = counter.checked_add(1).ok_or(ErrorCode::InternalError)?;
    Ok(*counter)
}
impl EditorState {
    pub fn new(target: Target, counter: &mut u64) -> Result<Self, ErrorCode> {
        let lifetime = revision(counter)?;
        Ok(Self {
            target,
            lifetime,
            cut: EditorCut {
                composer: Composer {
                    text: String::new(),
                    cursor: 0,
                    text_revision: lifetime,
                },
                admission: Admission::Ready,
                receipt: None,
            },
        })
    }
    /// Validate and commit before publishing. Cursor-only cuts keep text identity.
    pub fn edit(&mut self, edit: Edit, counter: &mut u64) -> Result<(), ErrorCode> {
        let draft = &self.cut.composer;
        let mut text = draft.text.clone();
        let at = draft.cursor;
        let previous = text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .take_while(|i| *i < at)
            .last()
            .unwrap_or(0);
        let next = text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .find(|i| *i > at)
            .unwrap_or(text.len());
        let cursor = match edit {
            Edit::Insert(c) => {
                if (c.is_control() && c != '\n' && c != '\t')
                    || text.len() + c.len_utf8() > COMPOSER_MAX_BYTES
                {
                    return Err(ErrorCode::InvalidParams);
                }
                text.insert(at, c);
                let end = at + c.len_utf8();
                text.grapheme_indices(true)
                    .map(|(i, _)| i)
                    .find(|i| *i >= end)
                    .unwrap_or(text.len())
            }
            Edit::Backspace => {
                text.replace_range(previous..at, "");
                previous
            }
            Edit::Left => previous,
            Edit::Right => next,
            Edit::Focus => at,
        };
        let text_revision = if text != draft.text {
            revision(counter)?
        } else {
            draft.text_revision
        };
        self.cut.composer = Composer {
            text,
            cursor,
            text_revision,
        };
        Ok(())
    }
    /// Success clears only the captured text identity, regardless of cursor movement.
    pub fn settle(
        &mut self,
        captured: u64,
        outcome: Result<(), ErrorCode>,
        counter: &mut u64,
    ) -> Result<(), ErrorCode> {
        if outcome.is_ok() && self.cut.composer.text_revision == captured {
            let text_revision = revision(counter)?;
            self.cut.composer = Composer {
                text: String::new(),
                cursor: 0,
                text_revision,
            };
        }
        self.cut.admission = outcome.map_or_else(Admission::Refused, |()| Admission::Ready);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target() -> Target {
        Target {
            agent_id: "fixture-editor".into(),
            generation: 1,
        }
    }
    #[test]
    fn shared_counter_and_grapheme_cursor_obey_text_identity() {
        let mut counter = 0;
        let mut a = EditorState::new(target(), &mut counter).unwrap();
        a.edit(Edit::Insert('e'), &mut counter).unwrap();
        a.edit(Edit::Insert('\u{301}'), &mut counter).unwrap();
        let revision = a.cut.composer.text_revision;
        a.edit(Edit::Left, &mut counter).unwrap();
        assert_eq!(a.cut.composer.cursor, 0);
        assert_eq!(a.cut.composer.text_revision, revision);
        let b = EditorState::new(target(), &mut counter).unwrap();
        assert!(b.lifetime > revision);
        a.edit(Edit::Right, &mut counter).unwrap();
        a.edit(Edit::Backspace, &mut counter).unwrap();
        assert_eq!(a.cut.composer.text, "");
        assert!(a.cut.composer.text_revision > b.lifetime);
    }
    #[test]
    fn overflow_control_and_limit_refusals_preserve_the_whole_cut() {
        let mut counter = 0;
        let mut a = EditorState::new(target(), &mut counter).unwrap();
        let revision = a.cut.composer.text_revision;
        assert_eq!(
            a.edit(Edit::Insert('\u{1b}'), &mut counter),
            Err(ErrorCode::InvalidParams)
        );
        assert_eq!(a.cut.composer.text_revision, revision);
        counter = u64::MAX;
        assert_eq!(
            a.edit(Edit::Insert('x'), &mut counter),
            Err(ErrorCode::InternalError)
        );
        assert_eq!(a.cut.composer.text, "");
        a.cut.composer.text = "x".repeat(COMPOSER_MAX_BYTES);
        a.cut.composer.cursor = COMPOSER_MAX_BYTES;
        assert_eq!(
            a.edit(Edit::Insert('界'), &mut counter),
            Err(ErrorCode::InvalidParams)
        );
        assert_eq!(a.cut.composer.text.len(), COMPOSER_MAX_BYTES);
        assert_eq!(a.cut.composer.text_revision, revision);
    }
    #[test]
    fn joining_graphemes_resegments_the_caret() {
        let mut counter = 0;
        let mut a = EditorState::new(target(), &mut counter).unwrap();
        for c in "👩👩".chars() {
            a.edit(Edit::Insert(c), &mut counter).unwrap();
        }
        a.edit(Edit::Left, &mut counter).unwrap();
        a.edit(Edit::Insert('\u{200d}'), &mut counter).unwrap();
        assert_eq!(a.cut.composer.text, "👩\u{200d}👩");
        assert_eq!(a.cut.composer.cursor, a.cut.composer.text.len());
        a.edit(Edit::Backspace, &mut counter).unwrap();
        assert_eq!(a.cut.composer.text, "");
    }
}
