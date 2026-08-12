//! Search over the prompt-history panel.
//!
//! Matches are addressed in **content** coordinates — `(entry, byte_offset)` —
//! never as row indices (#254). A row index is only meaningful at the width and
//! detail level that produced it, so a match found before a resize would point
//! somewhere arbitrary after one. The same reason the scroll anchor is content
//! addressed, and the two share a resolution path.
//!
//! Scope is the whole retained history, not the rendered window. A search that
//! silently covers only what is on screen is worse than no search: a miss reads
//! as "not in this conversation" when it means "not visible right now".

use crate::terminal::PromptHistoryEntry;

/// One match, in content coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PromptMatch {
    /// Index into the history ring.
    pub entry: usize,
    /// Byte offset of the match start within that entry's text.
    pub body_offset: usize,
    /// Match length in bytes.
    pub len: usize,
}

/// Search state for the open panel.
#[derive(Debug, Default, Clone)]
pub(crate) struct PromptSearch {
    /// Whether the query line is accepting input.
    pub active: bool,
    pub query: String,
    /// Every match, oldest first. Rebuilt whenever the query or history moves.
    pub matches: Vec<PromptMatch>,
    /// Index into `matches` of the one the view is parked on.
    pub current: usize,
    /// The `(history generation, query)` the current matches were computed
    /// from. Matches address entry indices, so a history mutation invalidates
    /// them; re-running per frame over a whole transcript would not be viable,
    /// so staleness is checked against the counter instead.
    searched: Option<(u64, String)>,
}

impl PromptSearch {
    /// Open the query line.
    pub(crate) fn open(&mut self) {
        self.active = true;
    }

    /// Close the query line and drop the query, matches, and staleness key, so
    /// no highlight lingers over content the reader is no longer searching.
    pub(crate) fn close(&mut self) {
        self.active = false;
        self.query.clear();
        self.matches.clear();
        self.searched = None;
        self.current = 0;
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.query.trim().is_empty()
    }

    /// Whether the matches no longer describe `generation` and the live query.
    pub(crate) fn is_stale(&self, generation: u64) -> bool {
        match &self.searched {
            Some((seen, query)) => *seen != generation || query != &self.query,
            None => !self.is_empty(),
        }
    }

    /// Recompute matches over `history`.
    ///
    /// Case-insensitive substring, which is what a reader scanning their own
    /// conversation expects; a regex would need escaping rules for text that is
    /// mostly code and paths.
    pub(crate) fn refresh(&mut self, history: &[PromptHistoryEntry], generation: u64) {
        let previous = self.matches.get(self.current).copied();
        self.searched = Some((generation, self.query.clone()));
        self.matches.clear();
        let needle = self.query.trim().to_lowercase();
        if needle.is_empty() {
            self.current = 0;
            return;
        }
        for (index, entry) in history.iter().enumerate() {
            let haystack = entry.text.to_lowercase();
            // `to_lowercase` can change byte length (e.g. 'İ'), which would
            // make offsets into it invalid for the original text. Fall back to
            // a case-sensitive scan for those, rather than reporting an offset
            // that slices mid-character.
            if haystack.len() != entry.text.len() {
                // Trimmed, like the fast path: otherwise a trailing space in the
                // query would match differently depending on which entry it hit.
                collect_matches(&entry.text, self.query.trim(), index, &mut self.matches);
                continue;
            }
            collect_matches(&haystack, &needle, index, &mut self.matches);
        }
        // Keep the reader on the nearest match to where they were, so typing
        // another character does not throw them to the top of the file.
        self.current = previous
            .and_then(|previous| {
                self.matches.iter().position(|m| {
                    (m.entry, m.body_offset) >= (previous.entry, previous.body_offset)
                })
            })
            .unwrap_or(0);
    }

    /// Step to the next match (`forward`) or the previous one, wrapping at the
    /// ends — a search that stops dead at the last match reads as broken.
    pub(crate) fn step(&mut self, forward: bool) -> Option<PromptMatch> {
        if self.matches.is_empty() {
            return None;
        }
        self.current = if forward {
            (self.current + 1) % self.matches.len()
        } else {
            (self.current + self.matches.len() - 1) % self.matches.len()
        };
        self.matches.get(self.current).copied()
    }

    /// The match the view is parked on.
    pub(crate) fn current(&self) -> Option<PromptMatch> {
        self.matches.get(self.current).copied()
    }

    /// `[3/17]`, or `no matches` once a query has been typed — the difference
    /// between "not here" and "not on screen" has to be legible, or an
    /// off-screen hit reads as absence.
    pub(crate) fn status(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        if self.matches.is_empty() {
            return "no matches".to_string();
        }
        format!("{}/{}", self.current + 1, self.matches.len())
    }
}

fn collect_matches(haystack: &str, needle: &str, entry: usize, out: &mut Vec<PromptMatch>) {
    if needle.is_empty() {
        return;
    }
    let mut from = 0usize;
    while let Some(found) = haystack[from..].find(needle) {
        let at = from + found;
        out.push(PromptMatch {
            entry,
            body_offset: at,
            len: needle.len(),
        });
        // Advance past this match. `needle` is non-empty (checked above), so
        // this always makes progress.
        from = at + needle.len();
        if from >= haystack.len() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::PromptHistoryKind;
    use std::time::Instant;

    fn entry(text: &str) -> PromptHistoryEntry {
        PromptHistoryEntry {
            kind: PromptHistoryKind::Prompt,
            text: text.to_string(),
            recorded_at: Instant::now(),
            wall_clock: None,
        }
    }

    #[test]
    fn finds_every_occurrence_across_entries_in_order() {
        let history = vec![
            entry("the wrap bug is here"),
            entry("nothing"),
            entry("wrap again and wrap once more"),
        ];
        let mut search = PromptSearch {
            query: "wrap".into(),
            ..Default::default()
        };
        search.refresh(&history, 0);
        assert_eq!(
            search
                .matches
                .iter()
                .map(|m| (m.entry, m.body_offset))
                .collect::<Vec<_>>(),
            vec![(0, 4), (2, 0), (2, 15)]
        );
        assert_eq!(search.status(), "1/3");
    }

    #[test]
    fn matching_is_case_insensitive() {
        let history = vec![entry("The Wrap Bug")];
        let mut search = PromptSearch {
            query: "wrap".into(),
            ..Default::default()
        };
        search.refresh(&history, 0);
        assert_eq!(search.matches.len(), 1);
        assert_eq!(search.matches[0].body_offset, 4);
    }

    /// Offsets index the ORIGINAL text, so they must land on char boundaries
    /// or highlighting slices mid-character and panics.
    #[test]
    fn offsets_are_char_boundaries_in_the_source_text() {
        let history = vec![entry("αβγ wrap δεζ wrap")];
        let mut search = PromptSearch {
            query: "wrap".into(),
            ..Default::default()
        };
        search.refresh(&history, 0);
        assert_eq!(search.matches.len(), 2);
        for found in &search.matches {
            assert!(
                history[found.entry]
                    .text
                    .is_char_boundary(found.body_offset),
                "offset {} is not a char boundary",
                found.body_offset
            );
            assert!(history[found.entry]
                .text
                .is_char_boundary(found.body_offset + found.len));
        }
    }

    /// Lowercasing can change byte length, which would invalidate offsets into
    /// the original. Those entries must degrade to a case-sensitive scan rather
    /// than report an offset that slices mid-character.
    #[test]
    fn entries_whose_case_folding_changes_length_stay_addressable() {
        let history = vec![entry("İstanbul wrap")];
        let mut search = PromptSearch {
            query: "wrap".into(),
            ..Default::default()
        };
        search.refresh(&history, 0);
        assert_eq!(search.matches.len(), 1);
        let found = search.matches[0];
        assert!(history[0].text.is_char_boundary(found.body_offset));
        assert_eq!(&history[0].text[found.body_offset..], "wrap");
    }

    #[test]
    fn stepping_wraps_at_both_ends() {
        let history = vec![entry("wrap wrap wrap")];
        let mut search = PromptSearch {
            query: "wrap".into(),
            ..Default::default()
        };
        search.refresh(&history, 0);
        assert_eq!(search.current, 0);
        assert_eq!(search.step(true).map(|m| m.body_offset), Some(5));
        assert_eq!(search.step(true).map(|m| m.body_offset), Some(10));
        assert_eq!(
            search.step(true).map(|m| m.body_offset),
            Some(0),
            "forward past the last match wraps to the first"
        );
        assert_eq!(
            search.step(false).map(|m| m.body_offset),
            Some(10),
            "backward past the first wraps to the last"
        );
    }

    /// Typing another character must not throw the reader back to the top of
    /// the conversation.
    #[test]
    fn refining_a_query_keeps_the_reader_near_their_current_match() {
        let history = vec![entry("wrapping"), entry("wrap"), entry("wrapped text")];
        let mut search = PromptSearch {
            query: "wrap".into(),
            ..Default::default()
        };
        search.refresh(&history, 0);
        search.step(true);
        search.step(true);
        assert_eq!(search.current().map(|m| m.entry), Some(2));

        // Refine so only the last entry still matches.
        search.query = "wrapped".into();
        search.refresh(&history, 0);
        assert_eq!(
            search.current().map(|m| m.entry),
            Some(2),
            "the surviving match near the reader stays selected"
        );
    }

    #[test]
    fn status_distinguishes_no_matches_from_no_query() {
        let history = vec![entry("nothing here")];
        let mut search = PromptSearch::default();
        search.refresh(&history, 0);
        assert_eq!(search.status(), "", "no query means no status at all");

        search.query = "wrap".into();
        search.refresh(&history, 0);
        assert_eq!(
            search.status(),
            "no matches",
            "a query with no hits must say so, not read as empty"
        );
    }

    #[test]
    fn closing_drops_matches_so_highlights_do_not_linger() {
        let history = vec![entry("wrap")];
        let mut search = PromptSearch {
            active: true,
            query: "wrap".into(),
            ..Default::default()
        };
        search.refresh(&history, 0);
        assert_eq!(search.matches.len(), 1);
        search.close();
        assert!(!search.active);
        assert!(search.matches.is_empty());
    }
}
