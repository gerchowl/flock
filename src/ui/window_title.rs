//! The HOST terminal's window title (#361).
//!
//! flock computes everything a good title wants — how many agents are blocked,
//! which workspace is focused, which server you are on — and, until this
//! module, published none of it outward. An Alacritty window running flock was
//! labelled with whatever the shell last set, so with several terminals open
//! you could not tell which one was flock, which server it was attached to, or
//! that something was blocked and waiting on you.
//!
//! # The grammar
//!
//! ```text
//! [● <badge>  ]<target> · <server> — flk
//! ```
//!
//! | situation | title |
//! | --- | --- |
//! | all idle, focused on `main` | `main · mba22 — flk` |
//! | 2 blocked, 3 working | `● 2B  feat/foo · mba22 — flk` |
//! | remote, 1 blocked | `● 1B  main · anvil — flk` |
//! | empty flock | `flk` |
//!
//! The ordering is not aesthetic. Titles are read in their first ~2 words and
//! truncated by PIXEL width from the right, everywhere real (a Firefox tab at
//! its 76px minimum shows the favicon and zero title characters), so the one
//! thing that CHANGES leads and the constant identity marker trails where
//! truncation can take it. Apple's "don't title windows with your app name"
//! does not transfer: in a terminal there is no Dock icon or window chrome, so
//! the switcher sees only this string and a short identity marker is doing work
//! no other channel does — at the END, because by the time you read that far
//! you already know.
//!
//! # Two rules that are load-bearing, not cosmetic
//!
//! * **The badge vanishes when nothing wants attention.** W3C Badging's own
//!   explainer names the failure mode: a permanently-present count leaves the
//!   user "unable to tell when new messages arrive". A steady number is
//!   furniture; only its APPEARANCE is a signal. So the badge carries the
//!   attention states only — blocked, then done-unseen — and a fleet that is
//!   merely busy renders no badge at all.
//! * **Publishes are debounced and deduped on the rendered title.** A title
//!   rewritten every frame flickers, and some window managers bounce the
//!   taskbar entry for it. [`WindowTitlePublisher`] rate-limits to one publish
//!   per [`TITLE_DEBOUNCE`] and drops any publish whose text matches the last
//!   one emitted.
//!
//! The exact glyphs and separators are an invented grammar — nothing in this
//! space has a convention to borrow (tmux, nvim, zellij, k9s and lazygit all
//! set DESCRIPTIVE titles; none encodes an attention count). The ordering and
//! the elision rule are evidence-backed; the punctuation is a choice.

use std::time::{Duration, Instant};

use super::state_signal::StateTally;
use crate::app::AppState;
use crate::terminal::TerminalRuntimeRegistry;

/// Minimum spacing between two published titles. The issue's ~500 ms: long
/// enough that a burst of state changes collapses into one write, short enough
/// that the badge still reads as immediate.
pub(crate) const TITLE_DEBOUNCE: Duration = Duration::from_millis(500);

/// Cap on the target segment. A branch name is user-controlled and can be
/// arbitrarily long; middle-truncating it keeps the server and the identity
/// marker inside any plausible title budget.
const MAX_TARGET_CHARS: usize = 48;

/// The trailing identity marker. Short, constant, last — see the module docs.
const IDENTITY: &str = "flk";

/// Renders the title from its parts. Pure: no host name lookup, no clock, no
/// terminal — the whole grammar is testable from here.
///
/// `target` is the focused workspace's branch (or name); `None` means an empty
/// flock, which renders as the bare identity marker.
pub(crate) fn render_window_title(
    tally: &StateTally,
    target: Option<&str>,
    server: &str,
) -> String {
    // Attention states only, worst first. Working and idle are deliberately
    // absent: a busy fleet is not a fleet that wants you.
    let mut counts: Vec<String> = Vec::with_capacity(2);
    if tally.blocked > 0 {
        counts.push(format!("{}B", tally.blocked));
    }
    if tally.done > 0 {
        counts.push(format!("{}D", tally.done));
    }
    // Two spaces after the badge group: the gap is what makes the badge read as
    // a separate field rather than a prefix of the workspace name.
    let badge = if counts.is_empty() {
        String::new()
    } else {
        format!("\u{25cf} {}  ", counts.join(" "))
    };

    let body = match target.map(str::trim).filter(|target| !target.is_empty()) {
        Some(target) => format!(
            "{} \u{00b7} {server} \u{2014} {IDENTITY}",
            crate::terminal::middle_truncate_chars(target, MAX_TARGET_CHARS)
        ),
        None => IDENTITY.to_string(),
    };

    // Branch names and workspace names are user data on their way into an
    // escape sequence: a stray ESC or BEL would terminate the OSC early and
    // spill the rest into the pane.
    crate::terminal_notify::sanitize_text(format!("{badge}{body}"))
}

/// The title for the current app state: this server's own tally and its
/// focused workspace. Local scope on purpose — a remote server computes and
/// publishes its OWN title, which is exactly what an attached client should be
/// advertising while it is attached there.
pub(crate) fn window_title(
    state: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> String {
    let tally = super::sidebar::local_server_tally(state);
    let target = state
        .active
        .and_then(|idx| state.workspaces.get(idx))
        .map(|ws| super::grammar::local_member_target(state, ws, terminal_runtimes));
    render_window_title(
        &tally,
        target.as_deref(),
        &super::grammar::local_server_name(),
    )
}

/// Debounce + dedupe in front of the title write.
///
/// Recomputing the title is cheap but WRITING it is not free: a title rewritten
/// every frame flickers and bounces taskbar entries. Feed every candidate title
/// through [`due`](Self::due) — it answers `Some` only when the text actually
/// changed AND the debounce window has passed.
#[derive(Debug, Default)]
pub(crate) struct WindowTitlePublisher {
    /// The last title handed out — the dedupe baseline.
    last_published: Option<String>,
    /// Earliest instant the next publish may go out.
    next_allowed: Option<Instant>,
    /// Whether a title has EVER gone out. Survives [`invalidate`](Self::invalidate),
    /// which only clears the dedupe baseline — the exit path needs to know
    /// whether flock ever touched the host title, not whether it currently
    /// remembers what it set.
    ever_published: bool,
}

impl WindowTitlePublisher {
    /// Offer `title` for publication at `now`. Returns the title to publish, or
    /// `None` when it is unchanged or still inside the debounce window.
    pub(crate) fn due(&mut self, title: String, now: Instant) -> Option<String> {
        if self.last_published.as_deref() == Some(title.as_str()) {
            return None;
        }
        if self.next_allowed.is_some_and(|allowed| now < allowed) {
            return None;
        }
        self.next_allowed = Some(now + TITLE_DEBOUNCE);
        self.last_published = Some(title.clone());
        self.ever_published = true;
        Some(title)
    }

    /// When a suppressed publish becomes possible again, if the caller should
    /// wake up for it. `None` once the window has passed.
    pub(crate) fn retry_deadline(&self, now: Instant) -> Option<Instant> {
        self.next_allowed.filter(|allowed| *allowed > now)
    }

    /// Forget the dedupe baseline so the next candidate publishes even if its
    /// text is unchanged. Used when the AUDIENCE changes rather than the title:
    /// a new foreground client, or a background connection slot resuming — both
    /// are terminals that have never seen this title.
    pub(crate) fn invalidate(&mut self) {
        self.last_published = None;
    }

    /// Whether a title has ever been published. The exit path uses it to decide
    /// whether it has a title to take back.
    pub(crate) fn has_published(&self) -> bool {
        self.ever_published
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tally(blocked: usize, done: usize, working: usize, idle: usize) -> StateTally {
        StateTally {
            blocked,
            done,
            working,
            idle,
        }
    }

    #[test]
    fn calm_flock_renders_no_badge() {
        assert_eq!(
            render_window_title(&tally(0, 0, 0, 3), Some("main"), "mba22"),
            "main \u{00b7} mba22 \u{2014} flk"
        );
    }

    #[test]
    fn blocked_agents_lead_the_title() {
        // The badge is FIRST: it is the only part that changes, and the only
        // part reliably read before truncation.
        let title = render_window_title(&tally(2, 0, 3, 0), Some("feat/foo"), "mba22");
        assert_eq!(title, "\u{25cf} 2B  feat/foo \u{00b7} mba22 \u{2014} flk");
        assert!(title.starts_with('\u{25cf}'), "got {title:?}");
    }

    #[test]
    fn working_and_idle_never_reach_the_badge() {
        // W3C Badging's documented failure mode: a count that is always there
        // stops being a signal. Only attention states qualify, so a fleet that
        // is merely busy renders exactly like a calm one.
        let busy = render_window_title(&tally(0, 0, 9, 4), Some("main"), "mba22");
        assert_eq!(
            busy,
            render_window_title(&tally(0, 0, 0, 0), Some("main"), "mba22")
        );
        assert!(!busy.contains('\u{25cf}'), "got {busy:?}");
        assert!(!busy.contains('W'), "got {busy:?}");
    }

    #[test]
    fn done_unseen_badges_after_blocked() {
        assert_eq!(
            render_window_title(&tally(1, 1, 0, 0), Some("main"), "anvil"),
            "\u{25cf} 1B 1D  main \u{00b7} anvil \u{2014} flk"
        );
        assert_eq!(
            render_window_title(&tally(0, 2, 0, 0), Some("main"), "anvil"),
            "\u{25cf} 2D  main \u{00b7} anvil \u{2014} flk"
        );
    }

    #[test]
    fn empty_flock_is_the_bare_identity_marker() {
        assert_eq!(
            render_window_title(&tally(0, 0, 0, 0), None, "mba22"),
            "flk"
        );
        assert_eq!(
            render_window_title(&tally(0, 0, 0, 0), Some("  "), "mba22"),
            "flk"
        );
    }

    #[test]
    fn long_targets_are_middle_truncated() {
        let branch = "feat/".to_string() + &"x".repeat(200);
        let title = render_window_title(&tally(0, 0, 0, 0), Some(&branch), "mba22");
        assert!(title.contains('\u{2026}'), "got {title:?}");
        assert!(
            title.ends_with("\u{00b7} mba22 \u{2014} flk"),
            "got {title:?}"
        );
        assert!(title.chars().count() < 80, "got {title:?}");
    }

    #[test]
    fn control_bytes_in_a_branch_name_cannot_terminate_the_sequence() {
        // A workspace name is user data on its way into an OSC payload.
        let title = render_window_title(&tally(0, 0, 0, 0), Some("a\u{1b}]0;b\u{7}"), "mba22");
        assert!(!title.contains('\u{1b}'), "got {title:?}");
        assert!(!title.contains('\u{7}'), "got {title:?}");
    }

    #[test]
    fn publisher_emits_the_first_title_immediately() {
        let mut publisher = WindowTitlePublisher::default();
        let now = Instant::now();
        assert_eq!(publisher.due("a".into(), now), Some("a".to_string()));
    }

    #[test]
    fn publisher_dedupes_an_unchanged_title() {
        let mut publisher = WindowTitlePublisher::default();
        let now = Instant::now();
        assert!(publisher.due("a".into(), now).is_some());
        // Well past the debounce window, but the text is identical: no write.
        assert_eq!(publisher.due("a".into(), now + TITLE_DEBOUNCE * 10), None);
    }

    #[test]
    fn publisher_debounces_a_burst_and_publishes_the_latest_after_it() {
        let mut publisher = WindowTitlePublisher::default();
        let now = Instant::now();
        assert!(publisher.due("a".into(), now).is_some());
        // A burst of changes inside the window collapses to nothing...
        assert_eq!(
            publisher.due("b".into(), now + Duration::from_millis(50)),
            None
        );
        assert_eq!(
            publisher.due("c".into(), now + Duration::from_millis(100)),
            None
        );
        // ...and the caller is told when to come back.
        assert_eq!(
            publisher.retry_deadline(now + Duration::from_millis(100)),
            Some(now + TITLE_DEBOUNCE)
        );
        // Past the window the CURRENT title goes out, not the queued ones.
        assert_eq!(
            publisher.due("d".into(), now + TITLE_DEBOUNCE),
            Some("d".to_string())
        );
    }

    #[test]
    fn publisher_stops_asking_to_be_woken_once_the_window_passes() {
        let mut publisher = WindowTitlePublisher::default();
        let now = Instant::now();
        assert!(publisher.due("a".into(), now).is_some());
        assert!(publisher.retry_deadline(now + TITLE_DEBOUNCE).is_none());
    }

    #[test]
    fn invalidate_republishes_the_same_title_to_a_new_audience() {
        let mut publisher = WindowTitlePublisher::default();
        let now = Instant::now();
        assert!(publisher.due("a".into(), now).is_some());
        assert_eq!(publisher.due("a".into(), now + TITLE_DEBOUNCE), None);
        publisher.invalidate();
        assert_eq!(
            publisher.due("a".into(), now + TITLE_DEBOUNCE),
            Some("a".to_string())
        );
    }

    #[test]
    fn has_published_tracks_whether_the_host_title_was_touched() {
        let mut publisher = WindowTitlePublisher::default();
        assert!(!publisher.has_published());
        publisher.due("a".into(), Instant::now());
        assert!(publisher.has_published());
        // `invalidate` drops the dedupe baseline, not the fact that the host
        // title was touched — the exit path still owes a restore.
        publisher.invalidate();
        assert!(publisher.has_published());
    }

    #[test]
    fn window_title_reads_the_focused_workspace_and_a_calm_tally() {
        let mut app = AppState::test_new();
        let mut ws = crate::workspace::Workspace::test_new("flock");
        ws.custom_name = None;
        ws.cached_git_branch = Some("feat/title".into());
        app.workspaces = vec![ws];
        app.active = Some(0);
        let runtimes = TerminalRuntimeRegistry::new();
        let title = window_title(&app, &runtimes);
        let server = super::super::grammar::local_server_name();
        assert_eq!(title, format!("feat/title \u{00b7} {server} \u{2014} flk"));
    }

    #[test]
    fn window_title_of_an_empty_flock_is_the_identity_marker() {
        let mut app = AppState::test_new();
        app.workspaces.clear();
        app.active = None;
        let runtimes = TerminalRuntimeRegistry::new();
        assert_eq!(window_title(&app, &runtimes), "flk");
    }
}
