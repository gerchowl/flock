//! The cross-repo issue-drop dialog (#371).
//!
//! ## Why the body is not edited here
//!
//! This dialog collects the things that fit on one line — destination, title —
//! and nothing else. The body is composed in a **real PTY** by the command the
//! dialog hands off to.
//!
//! That is not a shortcut. flock's only text primitive is
//! [`crate::app::line_editor::LineEditor`], which is single-line by
//! construction and defers wide-glyph handling. An issue body needs soft wrap,
//! a viewport, and logical-versus-visual cursor movement on top of it — and
//! even then the result would be unusable for the purpose, because
//! `handle_paste` early-returns unless the mode is `Terminal` (so an overlay
//! cannot accept a pasted URL) and #327 means overlay text cannot be selected
//! or copied out. A pane already has all of that, plus the operator's own
//! editor and keybindings.
//!
//! ## Why this does not disturb the focused pane
//!
//! The dialog is a modal over `AppState`, and the hand-off opens a NEW pane.
//! Neither sends a keystroke to the agent in the focused pane, which is the
//! hard requirement in #371. `SpawnLocation` and ADR-0014 govern
//! *agent-initiated* spawn and are not in play: this is the operator acting
//! directly.

use crate::app::line_editor::LineEditor;
use crate::github::repos::{Provenance, RankedRepo};

/// Which part of the dialog has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IssueDropFocus {
    /// Filtering and choosing a destination.
    #[default]
    Repo,
    /// Typing the title.
    Title,
}

/// How the destination list is doing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DirectoryStatus {
    #[default]
    Loading,
    Ready,
    /// Enumeration failed. The dialog stays usable — a typed `owner/name` still
    /// files — so this is a banner, not a dead end.
    Failed(String),
}

#[derive(Debug, Clone, Default)]
pub struct IssueDropState {
    /// The ranked directory, once it arrives.
    pub repos: Vec<RankedRepo>,
    pub status: DirectoryStatus,
    /// Filter over `repos`, which doubles as a free-typed `owner/name` for a
    /// repository the directory does not list.
    pub query: LineEditor,
    /// Index into [`Self::filtered`], not into `repos`.
    pub selected: usize,
    pub title: LineEditor,
    pub focus: IssueDropFocus,
    pub error: Option<String>,
}

impl IssueDropState {
    /// Indices into `repos` that match the current query, in rank order.
    pub fn filtered(&self) -> Vec<usize> {
        let needle = self.query.value().trim().to_ascii_lowercase();
        self.repos
            .iter()
            .enumerate()
            .filter(|(_, repo)| {
                needle.is_empty() || repo.name_with_owner.to_ascii_lowercase().contains(&needle)
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    /// The highlighted repository, if the filter matches anything.
    pub fn highlighted(&self) -> Option<&RankedRepo> {
        let filtered = self.filtered();
        filtered
            .get(self.selected)
            .and_then(|idx| self.repos.get(*idx))
    }

    /// Keep the cursor inside the filtered list after the query changes.
    ///
    /// Without this a narrowing filter leaves `selected` past the end and the
    /// dialog highlights nothing while still accepting Enter — which files into
    /// whatever the free-typed query happens to spell.
    pub fn clamp_selection(&mut self) {
        let len = self.filtered().len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        let len = self.filtered().len();
        if len == 0 {
            return;
        }
        let next = (self.selected as isize + delta.signum()).rem_euclid(len as isize);
        self.selected = next as usize;
    }

    /// The destination this dialog would file into.
    ///
    /// A highlighted row wins over the typed text, so arrowing to a repo and
    /// pressing Enter files where the highlight is — the query is a filter
    /// first and a free-text destination only when it matches nothing.
    pub fn destination(&self) -> Option<String> {
        if let Some(repo) = self.highlighted() {
            return Some(repo.name_with_owner.clone());
        }
        let typed = self.query.value().trim();
        (!typed.is_empty()).then(|| typed.to_string())
    }

    /// Where the highlighted destination came from, for the advisory.
    pub fn provenance(&self) -> Option<Provenance> {
        self.highlighted().map(|repo| repo.provenance)
    }

    /// Validate what the operator has assembled, returning the argv-ready
    /// `(repo, title)` pair.
    pub fn confirm(&self) -> Result<(String, String), String> {
        let Some(destination) = self.destination() else {
            return Err("pick a repository, or type owner/name".to_string());
        };
        let repo = crate::github::drop::normalize_destination(&destination)
            .map_err(|err| err.to_string())?;
        let title = self.title.value().trim().to_string();
        if title.is_empty() {
            return Err("an issue needs a title".to_string());
        }
        Ok((repo, title))
    }
}

/// The command the dialog hands off to.
///
/// Shelling `flk issue drop` rather than calling the composer in-process is
/// deliberate: it puts the operator's `$EDITOR` in a real PTY with real paste
/// and select-to-copy, and it keeps one implementation of the filing rules
/// instead of a CLI copy and a TUI copy that drift.
///
/// Arguments are quoted rather than interpolated raw — an issue title is
/// arbitrary operator prose and routinely contains spaces and quotes.
pub fn handoff_command(exe: &str, repo: &str, title: &str) -> String {
    format!(
        "{} issue drop --repo {} --title {} --file-it",
        shell_quote(exe),
        shell_quote(repo),
        shell_quote(title)
    )
}

/// Single-quote a value for `sh -c`, escaping embedded single quotes.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str, provenance: Provenance) -> RankedRepo {
        RankedRepo {
            name_with_owner: name.to_string(),
            provenance,
        }
    }

    fn state() -> IssueDropState {
        IssueDropState {
            repos: vec![
                repo("me/flock", Provenance::LocalCheckout),
                repo("org/flocking", Provenance::SeenInFleet),
                repo("other/thing", Provenance::Reachable),
            ],
            status: DirectoryStatus::Ready,
            ..Default::default()
        }
    }

    #[test]
    fn filtering_preserves_rank_order() {
        let mut s = state();
        s.query.set("flock");
        assert_eq!(s.filtered(), vec![0, 1]);
        assert_eq!(s.highlighted().unwrap().name_with_owner, "me/flock");
    }

    #[test]
    fn narrowing_the_filter_never_leaves_the_cursor_past_the_end() {
        // Otherwise the dialog highlights nothing while still accepting Enter,
        // and files into whatever the query happens to spell.
        let mut s = state();
        s.selected = 2;
        s.query.set("flock");
        s.clamp_selection();
        assert_eq!(s.selected, 1);
        assert!(s.highlighted().is_some());

        s.query.set("nothing-matches");
        s.clamp_selection();
        assert_eq!(s.selected, 0);
        assert!(s.highlighted().is_none());
    }

    #[test]
    fn a_highlighted_row_wins_over_the_typed_text() {
        let mut s = state();
        s.query.set("flock");
        s.clamp_selection();
        assert_eq!(s.destination().as_deref(), Some("me/flock"));
    }

    #[test]
    fn a_query_matching_nothing_becomes_the_destination() {
        // The whole feature is filing into a repo you have never checked out,
        // so a name the directory does not list must still be reachable.
        let mut s = state();
        s.query.set("stranger/repo");
        s.clamp_selection();
        assert!(s.highlighted().is_none());
        assert_eq!(s.destination().as_deref(), Some("stranger/repo"));
        s.title.set("a title");
        assert_eq!(
            s.confirm().unwrap(),
            ("stranger/repo".to_string(), "a title".to_string())
        );
    }

    #[test]
    fn selection_wraps_in_both_directions() {
        let mut s = state();
        s.move_selection(1);
        assert_eq!(s.selected, 1);
        s.move_selection(-1);
        assert_eq!(s.selected, 0);
        s.move_selection(-1);
        assert_eq!(s.selected, 2, "wraps to the end");
        s.move_selection(1);
        assert_eq!(s.selected, 0, "wraps to the start");
    }

    #[test]
    fn moving_an_empty_list_is_a_no_op_rather_than_a_panic() {
        let mut s = IssueDropState::default();
        s.move_selection(1);
        assert_eq!(s.selected, 0);
        assert!(s.highlighted().is_none());
    }

    #[test]
    fn confirm_reports_what_is_missing() {
        let mut s = state();
        s.query.set("   ");
        s.repos.clear();
        assert!(s.confirm().unwrap_err().contains("pick a repository"));

        let mut typed = IssueDropState::default();
        typed.query.set("owner/name");
        assert!(typed.confirm().unwrap_err().contains("title"));

        let mut bad = IssueDropState::default();
        bad.query.set("not-a-repo");
        bad.title.set("t");
        assert!(bad.confirm().unwrap_err().contains("owner/name"));
    }

    /// Undo POSIX single-quoting, so the test proves the escaping round-trips
    /// rather than asserting a dangerous substring is absent — it is present,
    /// and correctly inert, inside the quotes.
    fn unquote(quoted: &str) -> String {
        let inner = quoted
            .strip_prefix('\'')
            .and_then(|v| v.strip_suffix('\''))
            .expect("value is single-quoted end to end");
        inner.replace(r"'\''", "'")
    }

    #[test]
    fn a_title_with_quotes_cannot_break_out_of_the_handoff_command() {
        // An issue title is arbitrary prose; unquoted it would run as shell.
        for hostile in [
            "oops'; rm -rf /; echo '",
            "plain",
            "it's got an apostrophe",
            "$(whoami) `id` \\ \"dq\"",
        ] {
            let command = handoff_command("flk", "o/n", hostile);
            let title = command
                .split("--title ")
                .nth(1)
                .and_then(|rest| rest.strip_suffix(" --file-it"))
                .expect("title is the last quoted argument");
            assert_eq!(
                unquote(title),
                hostile,
                "the title must reach argv verbatim: {command}"
            );
        }
    }

    #[test]
    fn the_handoff_names_the_destination_and_files_it() {
        let command = handoff_command("flk", "acme/widgets", "a title");
        assert!(command.contains("--repo 'acme/widgets'"));
        assert!(command.contains("--title 'a title'"));
        assert!(
            command.contains("--file-it"),
            "the hand-off is the operator's confirmed action"
        );
    }

    #[test]
    fn a_failed_directory_still_allows_a_typed_destination() {
        // Enumeration is a shortcut. If it fails the dialog must degrade to
        // typing a name, not become a dead end.
        let mut s = IssueDropState {
            status: DirectoryStatus::Failed("rate limited".into()),
            ..Default::default()
        };
        s.query.set("owner/name");
        s.title.set("still works");
        assert!(s.confirm().is_ok());
    }
}
