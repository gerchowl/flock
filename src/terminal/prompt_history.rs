//! Per-pane prompt + recap + reply history.
//!
//! Today the pane header keeps only the LAST prompt; this module backs the
//! upgrade (issue #96): every `pane.report_prompt` APPENDS a timestamped
//! entry, `pane.report_recap` appends a visually distinct recap entry from a
//! session's Stop hook, and `pane.report_reply` appends the last assistant
//! reply so the user can scan the agent's side of the conversation
//! alongside their own prompts. The pane header keeps the collapsed (latest)
//! prompt unchanged; the expanded view becomes a bounded scrollable panel.
//!
//! Ephemeral by design — never persisted into session snapshots.

use std::time::{Duration, Instant, SystemTime};

/// Hard cap on history per pane, measured in entries.
///
/// Entries, not rendered lines (#254). The old line cap existed because hooks
/// streamed forever with no natural end, and it bounded render cost back when
/// one logical line meant one rendered row. Neither holds now: the transcript
/// is a finite file, and rows are wrapped, so a line budget no longer describes
/// either the memory held or the work done.
///
/// Sized against the transcripts on the development machine — 71 sessions,
/// longest 170 turns, p95 129 — so a real session is retained whole with an
/// order of magnitude spare. The ceiling that actually protects memory is
/// [`crate::agent_transcript::MAX_BLOCK_BYTES`], which caps any single block at
/// 128 KiB against a measured 1.35 MB `tool_result`.
pub const MAX_PROMPT_HISTORY_ENTRIES: usize = 2000;

/// What kind of entry this is — each kind renders with its own palette color
/// so the user can scan the float at a glance: `Prompt` (their input, dim
/// subtext), `Reply` (the agent's prose response, cool blue), `Recap` (a
/// disciplined `※ recap:` summary line at end-of-turn, warm mauve accent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptHistoryKind {
    Prompt,
    Recap,
    Reply,
}

/// One history entry. `text` is already sanitized when stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptHistoryEntry {
    pub kind: PromptHistoryKind,
    pub text: String,
    pub recorded_at: Instant,
    /// Set for entries hydrated from a session transcript (#246). Those are
    /// historical, so their age must come from the wall clock — a monotonic
    /// `recorded_at` stamped at hydration would render a month-old prompt as
    /// "0s ago". Live entries leave this `None` and keep using `recorded_at`.
    pub wall_clock: Option<SystemTime>,
}

impl PromptHistoryEntry {
    /// Compact "12s ago"-style age string.
    pub fn relative_age(&self, now: Instant) -> String {
        if let Some(at) = self.wall_clock {
            // Historical entry: age against the wall clock. A stamp in the
            // future (clock skew between hosts) reads as "0s ago" rather than
            // panicking or wrapping.
            let elapsed = SystemTime::now().duration_since(at).unwrap_or_default();
            return relative_age_for_duration(elapsed);
        }
        let elapsed = now.saturating_duration_since(self.recorded_at);
        relative_age_for_duration(elapsed)
    }
}

fn relative_age_for_duration(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// Append `entry`, then drop oldest entries until the history fits within
/// [`MAX_PROMPT_HISTORY_ENTRIES`]. The newest entry always survives.
///
/// `drain` rather than repeated `remove(0)`: hydration replays a whole
/// transcript through here, and shifting the vector once per dropped entry is
/// quadratic over a long session.
pub fn append_with_cap(history: &mut Vec<PromptHistoryEntry>, entry: PromptHistoryEntry) {
    history.push(entry);
    if history.len() > MAX_PROMPT_HISTORY_ENTRIES {
        let excess = history.len() - MAX_PROMPT_HISTORY_ENTRIES;
        history.drain(..excess);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: PromptHistoryKind, text: &str) -> PromptHistoryEntry {
        PromptHistoryEntry {
            kind,
            text: text.to_string(),
            recorded_at: Instant::now(),
            wall_clock: None,
        }
    }

    #[test]
    fn append_below_cap_keeps_everything() {
        let mut history = Vec::new();
        append_with_cap(&mut history, entry(PromptHistoryKind::Prompt, "fix bug"));
        append_with_cap(&mut history, entry(PromptHistoryKind::Recap, "did it"));
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].kind, PromptHistoryKind::Prompt);
        assert_eq!(history[1].kind, PromptHistoryKind::Recap);
    }

    #[test]
    fn append_over_cap_drops_oldest_whole_entries() {
        let mut history = Vec::new();
        for i in 0..MAX_PROMPT_HISTORY_ENTRIES + 5 {
            let mut e = entry(PromptHistoryKind::Prompt, "body");
            // Give each a distinct text so we can identify which survived.
            e.text = format!("entry-{i}");
            append_with_cap(&mut history, e);
        }
        assert_eq!(history.len(), MAX_PROMPT_HISTORY_ENTRIES);
        // The newest entry must always survive; the oldest must be gone.
        assert_eq!(
            history.last().unwrap().text,
            format!("entry-{}", MAX_PROMPT_HISTORY_ENTRIES + 4)
        );
        assert_eq!(history.first().unwrap().text, "entry-5");
    }

    /// A single enormous entry is retained whole: the byte ceiling belongs to
    /// the parser's per-block cap, not to the ring, which counts entries.
    #[test]
    fn append_single_oversized_entry_is_still_kept() {
        let mut history = Vec::new();
        let huge_body = (0..2000)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        append_with_cap(&mut history, entry(PromptHistoryKind::Prompt, &huge_body));
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].text, huge_body, "content must not be clipped");
    }

    /// A long session must be retained whole: the longest transcript measured
    /// on the development machine was 170 turns, so a real session sits far
    /// inside the cap rather than being silently clipped as it was under the
    /// old 1000-rendered-line budget.
    #[test]
    fn a_realistically_long_session_is_retained_whole() {
        let mut history = Vec::new();
        // 170 turns, each a substantial multi-line reply — well past what the
        // old line cap admitted (it held roughly 1000 rendered lines total).
        let body = (0..20)
            .map(|i| format!("a reasonably long line of agent prose number {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        for i in 0..170 {
            let mut e = entry(PromptHistoryKind::Reply, &body);
            e.text = format!("turn-{i}\n{body}");
            append_with_cap(&mut history, e);
        }
        assert_eq!(history.len(), 170, "no turn may be dropped");
        assert!(history[0].text.starts_with("turn-0"));
    }

    #[test]
    fn relative_age_buckets() {
        assert_eq!(relative_age_for_duration(Duration::from_secs(5)), "5s ago");
        assert_eq!(
            relative_age_for_duration(Duration::from_secs(125)),
            "2m ago"
        );
        assert_eq!(
            relative_age_for_duration(Duration::from_secs(3 * 3600 + 1)),
            "3h ago"
        );
        assert_eq!(
            relative_age_for_duration(Duration::from_secs(2 * 86_400)),
            "2d ago"
        );
    }
}
