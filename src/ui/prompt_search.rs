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

/// Which of the panel's two search surfaces is up (#258).
///
/// They are split by what the answer *is*, which is also why one is exact and
/// the other can be fuzzy:
///
/// * [`SearchSurface::Find`] — the answer is a character range, so matching is
///   exact substring: a highlight has to point at something specific.
/// * [`SearchSurface::Filter`] — the answer is a *turn*, so matching can be
///   fuzzy. Nothing is highlighted; the reader is choosing which turn to go to,
///   and being forgiving about how they half-remember it is the whole value.
///
/// Forcing fuzzy matching into the Find path does not work: token-AND and
/// subsequence matching answer "does this entry match?" (a boolean per turn)
/// and yield no single range to paint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SearchSurface {
    #[default]
    Find,
    Filter,
}

/// A turn that matched on the [`SearchSurface::Filter`] surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FilterHit {
    /// Index into the history ring.
    pub entry: usize,
    /// One-line summary for the results row.
    pub summary: String,
}

/// Search state for the open panel.
#[derive(Debug, Default, Clone)]
pub(crate) struct PromptSearch {
    /// Whether the query line is accepting input.
    pub active: bool,
    /// Which surface the query is being answered on.
    pub surface: SearchSurface,
    pub query: String,
    /// Turns matching the query on the Filter surface, newest first.
    pub filtered: Vec<FilterHit>,
    /// Index into `filtered` of the highlighted row.
    pub filter_selected: usize,
    /// Every match, NEWEST first (#258). Rebuilt whenever the query or the
    /// history moves.
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
        self.surface = SearchSurface::Find;
        self.query.clear();
        self.matches.clear();
        self.filtered.clear();
        self.filter_selected = 0;
        self.searched = None;
        self.current = 0;
    }

    /// Flip between Find and Filter, carrying the query across.
    ///
    /// The same `ctrl+f` that opened Find flips to Filter, so there is no
    /// second chord to learn and switching never costs what was typed.
    pub(crate) fn toggle_surface(&mut self) {
        self.surface = match self.surface {
            SearchSurface::Find => SearchSurface::Filter,
            SearchSurface::Filter => SearchSurface::Find,
        };
        self.filter_selected = 0;
        // Force a recompute: the two surfaces match differently, so the results
        // the other one left behind do not describe this query on this surface.
        self.searched = None;
    }

    pub(crate) fn is_filtering(&self) -> bool {
        self.surface == SearchSurface::Filter
    }

    /// Move the Filter selection. Wraps, like Find's stepping.
    pub(crate) fn step_filter(&mut self, forward: bool) -> Option<&FilterHit> {
        if self.filtered.is_empty() {
            return None;
        }
        self.filter_selected = if forward {
            (self.filter_selected + 1) % self.filtered.len()
        } else {
            (self.filter_selected + self.filtered.len() - 1) % self.filtered.len()
        };
        self.filtered.get(self.filter_selected)
    }

    pub(crate) fn selected_filter_hit(&self) -> Option<&FilterHit> {
        self.filtered.get(self.filter_selected)
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
        self.refresh_filter(history);
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
        // Newest first (#258). The panel is bottom-pinned and read
        // newest-at-bottom, and what a reader is hunting for is almost always
        // recent — landing them at the top of a 300-turn session and making
        // them step forward through every older hit inverts the surface they
        // are looking at. So `1/17` is the most recent match, and "next" walks
        // backwards in time.
        self.matches
            .sort_by_key(|m| std::cmp::Reverse((m.entry, m.body_offset)));
        // Keep the reader on the nearest match to where they were, so typing
        // another character does not throw them elsewhere in the history. The
        // comparison follows the sort: descending, so "nearest" is the first
        // match at or BEFORE where they were.
        self.current = previous
            .and_then(|previous| {
                self.matches.iter().position(|m| {
                    (m.entry, m.body_offset) <= (previous.entry, previous.body_offset)
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

impl PromptSearch {
    /// Recompute the Filter surface's turn list.
    ///
    /// Matching is the app's own convention (`text_matches_query`): the query
    /// splits on whitespace and every token must appear somewhere in the turn,
    /// in any order. So "wrap bug" finds a turn saying "the bug in wrapping",
    /// which exact substring search cannot — and which is exactly the kind of
    /// half-remembered phrasing you search a conversation with. Reusing the
    /// navigator's rule also means one matching behaviour to learn per app,
    /// not per panel.
    ///
    /// An empty query lists every turn, so the surface doubles as a turn index.
    fn refresh_filter(&mut self, history: &[PromptHistoryEntry]) {
        let previous = self.filtered.get(self.filter_selected).map(|hit| hit.entry);
        self.filtered.clear();
        let query = self.query.trim();
        // Newest first, matching the Find surface and the way the panel reads.
        for (index, entry) in history.iter().enumerate().rev() {
            if !query.is_empty() && !crate::app::state::text_matches_query(query, &entry.text) {
                continue;
            }
            self.filtered.push(FilterHit {
                entry: index,
                summary: summarize(&entry.text),
            });
        }
        // Hold the selection on the same turn across a refinement where it
        // survives, so typing another character does not move the reader.
        self.filter_selected = previous
            .and_then(|entry| self.filtered.iter().position(|hit| hit.entry == entry))
            .unwrap_or(0);
    }
}

/// First non-empty line of a turn, flattened for a one-line results row.
fn summarize(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string()
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

    /// Newest first (#258): the panel is bottom-pinned and read
    /// newest-at-bottom, so `1/N` must be the most RECENT hit and "next" must
    /// walk backwards in time. Landing on the oldest match threw the reader to
    /// the top of a long session.
    #[test]
    fn finds_every_occurrence_newest_first() {
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
            vec![(2, 15), (2, 0), (0, 4)],
            "matches must run newest to oldest, latest offset first within a turn"
        );
        assert_eq!(search.status(), "1/3");
        assert_eq!(
            search.current().map(|m| m.entry),
            Some(2),
            "the reader must land on the most recent turn that matched"
        );
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
    fn stepping_walks_backwards_in_time_and_wraps_at_both_ends() {
        let history = vec![entry("wrap wrap wrap")];
        let mut search = PromptSearch {
            query: "wrap".into(),
            ..Default::default()
        };
        search.refresh(&history, 0);
        assert_eq!(
            search.current().map(|m| m.body_offset),
            Some(10),
            "the newest (last) occurrence is where the reader starts"
        );
        assert_eq!(search.step(true).map(|m| m.body_offset), Some(5));
        assert_eq!(search.step(true).map(|m| m.body_offset), Some(0));
        assert_eq!(
            search.step(true).map(|m| m.body_offset),
            Some(10),
            "forward past the oldest match wraps to the newest"
        );
        assert_eq!(
            search.step(false).map(|m| m.body_offset),
            Some(0),
            "backward past the newest wraps to the oldest"
        );
    }

    /// Typing another character must not throw the reader back to the top of
    /// the conversation.
    /// Typing another character must not throw the reader elsewhere in the
    /// history. Under newest-first ordering "nearest" is the first match at or
    /// before where they were, so the comparison has to follow the sort.
    #[test]
    fn refining_a_query_keeps_the_reader_near_their_current_match() {
        let history = vec![
            entry("wrapping alpha"),
            entry("wrap beta"),
            entry("wrapped gamma"),
        ];
        let mut search = PromptSearch {
            query: "wrap".into(),
            ..Default::default()
        };
        search.refresh(&history, 0);
        assert_eq!(search.current().map(|m| m.entry), Some(2), "starts newest");
        search.step(true);
        assert_eq!(
            search.current().map(|m| m.entry),
            Some(1),
            "stepped back one turn in time"
        );

        // Refine so only the turn the reader is standing on still matches.
        search.query = "wrap b".into();
        search.refresh(&history, 0);
        assert_eq!(
            search.current().map(|m| m.entry),
            Some(1),
            "the surviving match where the reader already was stays selected"
        );
    }

    /// The reason the second surface exists (#258): Filter uses the app's own
    /// token-AND rule, so a query typed the way you remember the sentence finds
    /// the turn, where exact substring cannot.
    #[test]
    fn filter_finds_turns_that_exact_find_cannot() {
        let history = vec![
            entry("fix the bug in wrapping"),
            entry("unrelated turn"),
            entry("the wrap bug is fixed"),
        ];
        let mut search = PromptSearch {
            query: "wrap bug".into(),
            ..Default::default()
        };
        search.refresh(&history, 0);

        // Find: only the contiguous phrase.
        assert_eq!(
            search.matches.iter().map(|m| m.entry).collect::<Vec<_>>(),
            vec![2],
            "exact find must not match tokens out of order"
        );

        // Filter: both turns, in any word order, newest first.
        assert_eq!(
            search
                .filtered
                .iter()
                .map(|hit| hit.entry)
                .collect::<Vec<_>>(),
            vec![2, 0],
            "filter must find the turn saying 'bug in wrapping' too"
        );
    }

    /// The two surfaces share one query, so flipping never costs what was
    /// typed — that is why `ctrl+f` can be the toggle rather than a new chord.
    #[test]
    fn toggling_surfaces_carries_the_query() {
        let history = vec![entry("the wrap bug")];
        let mut search = PromptSearch {
            query: "wrap".into(),
            ..Default::default()
        };
        search.open();
        search.refresh(&history, 0);
        assert!(!search.is_filtering());

        search.toggle_surface();
        assert!(search.is_filtering());
        assert_eq!(search.query, "wrap", "the query survives the flip");
        assert!(
            search.is_stale(0),
            "the surfaces match differently, so the flip must force a recompute"
        );

        search.toggle_surface();
        assert!(!search.is_filtering(), "flipping back returns to find");
    }

    /// An empty query lists every turn, so the surface doubles as a turn index.
    #[test]
    fn an_empty_filter_query_lists_every_turn_newest_first() {
        let history = vec![entry("first"), entry("second"), entry("third")];
        let mut search = PromptSearch::default();
        search.toggle_surface();
        search.refresh(&history, 0);
        assert_eq!(
            search
                .filtered
                .iter()
                .map(|hit| hit.summary.as_str())
                .collect::<Vec<_>>(),
            vec!["third", "second", "first"]
        );
    }

    /// Refining must not move the reader off the turn they were about to pick.
    #[test]
    fn refining_the_filter_holds_the_selection_on_the_same_turn() {
        let history = vec![entry("alpha wrap"), entry("beta wrap"), entry("gamma wrap")];
        let mut search = PromptSearch {
            query: "wrap".into(),
            ..Default::default()
        };
        search.toggle_surface();
        search.refresh(&history, 0);
        // Newest first: [gamma(2), beta(1), alpha(0)]. Step onto beta.
        search.step_filter(true);
        assert_eq!(search.selected_filter_hit().map(|hit| hit.entry), Some(1));

        // Refine so only beta survives.
        search.query = "beta".into();
        search.refresh(&history, 0);
        assert_eq!(
            search.selected_filter_hit().map(|hit| hit.entry),
            Some(1),
            "the surviving turn the reader was on stays selected"
        );
    }

    /// Summaries are one line: a multi-line turn must not blow a list row out
    /// to the height of the turn.
    #[test]
    fn filter_summaries_are_the_first_non_empty_line() {
        let history = vec![entry("\n\n  the real first line\nand more\nand more")];
        let mut search = PromptSearch::default();
        search.toggle_surface();
        search.refresh(&history, 0);
        assert_eq!(search.filtered[0].summary, "the real first line");
    }

    #[test]
    fn closing_returns_to_the_find_surface() {
        let history = vec![entry("wrap")];
        let mut search = PromptSearch::default();
        search.open();
        search.toggle_surface();
        search.refresh(&history, 0);
        assert!(search.is_filtering());
        search.close();
        assert!(
            !search.is_filtering(),
            "the next ctrl+f must open Find, not the list the reader last left"
        );
        assert!(search.filtered.is_empty());
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
