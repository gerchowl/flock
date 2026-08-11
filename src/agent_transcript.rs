//! Reading an agent's conversation from its own session transcript.
//!
//! The prompt-history panel used to be fed by lifecycle hooks
//! (`HookPromptReported` / `HookReplyReported`), i.e. by whatever text a hook
//! chose to emit. That is second-hand: it only exists when hooks are
//! installed, it never captured the full turn, and it is ephemeral. The
//! session transcript is the source of truth, and — unlike pane scrollback —
//! it holds *unwrapped* logical text, so the panel can re-wrap it at any
//! width (#246).
//!
//! The transcript is an internal Claude Code file with no published schema and
//! no stability contract: three writer versions appeared in two weeks on the
//! development machine, and the entry-`type` set grew to 14 in that time. So
//! this module depends only on a small kernel that has held:
//!
//! ```text
//! type ∈ {user, assistant}          — everything else is meta, hidden
//! message.content: String | [Block]
//! Block.type ∈ {text, thinking, tool_use, tool_result}
//! ```
//!
//! Everything outside the kernel parses to [`Block::Unknown`] or
//! [`TranscriptEvent::Meta`] rather than failing, so a newer writer degrades
//! to "fewer rendered blocks", never to an error.
//!
//! Hard rules this module keeps, because the file is appended live by another
//! process and single entries reach ~1.4 MB:
//!
//! * read line-by-line through [`BufRead`], **never** mmap — Claude Code
//!   prunes transcripts, and mmap under truncation is a segfault
//! * never parse a partial trailing line (no `\n` yet means "not written yet")
//! * cap every block at [`MAX_BLOCK_BYTES`] before it can reach a renderer
//! * never panic and never log transcript *content* — these files contain
//!   everything typed and every tool result

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Longest block body handed to a renderer. A single `tool_result` line of
/// 1.35 MB was measured in a real transcript; re-wrapping that would stall the
/// UI, so bodies are truncated with an elision marker instead.
pub const MAX_BLOCK_BYTES: usize = 128 * 1024;

/// Give up if more than this fraction of lines fail the kernel parse — that
/// means the format moved, not that one entry is odd.
const MAX_UNPARSED_RATIO: f64 = 0.05;

/// Writer versions this parser has been exercised against. A newer writer
/// still renders (leniently); callers may surface a "newer than tested" hint.
pub const KNOWN_GOOD_WRITER_MAX: (u32, u32, u32) = (2, 1, 227);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

/// One piece of a message. Anything the kernel doesn't recognise lands in
/// `Unknown` and is dropped at render time rather than erroring the read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Text(String),
    Thinking,
    ToolCall { name: String },
    ToolResult { preview: String },
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEvent {
    Message {
        role: Role,
        blocks: Vec<Block>,
        /// Wall-clock time the entry was written, when the writer supplied a
        /// parseable one. Hydrated history is old, so ages must come from
        /// this rather than from a monotonic clock started at hydration.
        at: Option<SystemTime>,
    },
    /// A compaction boundary: prior messages remain on disk but are logically
    /// superseded. Rendered as a single divider, not as content.
    Compacted,
    /// Anything else on disk — hooks, file-history, titles, permission mode.
    Meta,
}

#[derive(Debug)]
pub enum TranscriptError {
    /// No readable file at that path.
    Unreadable,
    /// The file exists but too little of it matches the kernel — treat the
    /// format as moved and fall back to whatever the caller had before. The
    /// counts are carried for the diagnostic log, never the content.
    FormatMoved {
        #[allow(dead_code, reason = "surfaced through the Debug log line only")]
        parsed: usize,
        #[allow(dead_code, reason = "surfaced through the Debug log line only")]
        failed: usize,
    },
}

/// What a source produced, plus the drift signal callers may surface.
#[derive(Debug, Default)]
pub struct TranscriptRead {
    pub events: Vec<TranscriptEvent>,
    /// Highest `version` string seen, if any entry carried one.
    pub writer_version: Option<String>,
    /// True when `writer_version` is newer than [`KNOWN_GOOD_WRITER_MAX`].
    pub writer_newer_than_tested: bool,
}

/// An agent's on-disk conversation format.
///
/// flock recognises six official agent sources (`agent_resume.rs`); only
/// Claude is implemented here. Keying callers off this trait rather than off
/// "is it claude" means a second source is a parser, not a rewrite.
pub trait TranscriptSource {
    fn read(&self, path: &Path) -> Result<TranscriptRead, TranscriptError>;
}

pub struct ClaudeTranscript;

impl TranscriptSource for ClaudeTranscript {
    fn read(&self, path: &Path) -> Result<TranscriptRead, TranscriptError> {
        let file = File::open(path).map_err(|_| TranscriptError::Unreadable)?;
        read_lines(BufReader::new(file))
    }
}

/// Split out so tests can drive the parser from an in-memory buffer without
/// touching the filesystem.
pub fn read_lines<R: BufRead>(reader: R) -> Result<TranscriptRead, TranscriptError> {
    let mut out = TranscriptRead::default();
    let mut parsed = 0usize;
    let mut failed = 0usize;
    let mut max_version: Option<String> = None;

    let mut reader = reader;
    let mut line = String::new();
    loop {
        line.clear();
        // `BufRead::lines()` cannot be used here: it yields a final
        // unterminated line as though it were complete, so a half-written
        // entry would count as a parse failure and could trip the
        // format-moved threshold. Read raw and require the newline.
        match reader.read_line(&mut line) {
            Ok(0) => break,
            // No trailing newline means the writer is mid-append. Stop; the
            // rest arrives on a later read.
            Ok(_) if !line.ends_with('\n') => break,
            Ok(_) => {}
            // A read error mid-file (truncation under us) ends the read with
            // what we have rather than discarding it.
            Err(_) => break,
        }
        let line = line.trim_end_matches(['\n', '\r']);
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            failed += 1;
            continue;
        };
        parsed += 1;

        if let Some(version) = value.get("version").and_then(|v| v.as_str()) {
            // Compare parsed tuples: lexicographically "1.9.9" > "10.0.0".
            let newer = match (
                parse_version(version),
                max_version.as_deref().and_then(parse_version),
            ) {
                (Some(candidate), Some(seen)) => candidate > seen,
                (Some(_), None) => true,
                _ => false,
            };
            if newer || max_version.is_none() {
                max_version = Some(version.to_string());
            }
        }

        out.events.push(parse_entry(&value));
    }

    let total = parsed + failed;
    if total > 0 && (failed as f64 / total as f64) > MAX_UNPARSED_RATIO {
        return Err(TranscriptError::FormatMoved { parsed, failed });
    }

    out.writer_newer_than_tested = max_version
        .as_deref()
        .and_then(parse_version)
        .is_some_and(|v| v > KNOWN_GOOD_WRITER_MAX);
    out.writer_version = max_version;
    Ok(out)
}

fn parse_version(raw: &str) -> Option<(u32, u32, u32)> {
    let mut parts = raw.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// Entries that are structurally conversation but should never render as
/// content: agent-internal side conversations and harness bookkeeping.
fn is_hidden(value: &serde_json::Value) -> bool {
    ["isMeta", "isSidechain"]
        .iter()
        .any(|flag| value.get(flag).and_then(|v| v.as_bool()).unwrap_or(false))
}

fn parse_entry(value: &serde_json::Value) -> TranscriptEvent {
    if value
        .get("isCompactSummary")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return TranscriptEvent::Compacted;
    }
    if is_hidden(value) {
        return TranscriptEvent::Meta;
    }
    let role = match value.get("type").and_then(|v| v.as_str()) {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        _ => return TranscriptEvent::Meta,
    };
    let Some(content) = value.pointer("/message/content") else {
        return TranscriptEvent::Meta;
    };
    let blocks = match content {
        serde_json::Value::String(text) => vec![Block::Text(cap(text))],
        serde_json::Value::Array(items) => items.iter().map(parse_block).collect(),
        _ => return TranscriptEvent::Meta,
    };
    let at = value
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(parse_rfc3339_utc);
    TranscriptEvent::Message { role, blocks, at }
}

/// Parse the fixed `YYYY-MM-DDTHH:MM:SS[.fff]Z` shape the writer emits.
///
/// A full date library would be a dependency for one field; this accepts only
/// the exact UTC form observed and returns `None` for anything else, so a
/// format change costs the age label, never the content.
fn parse_rfc3339_utc(raw: &str) -> Option<SystemTime> {
    let (date, rest) = raw.split_once('T')?;
    let time = rest.strip_suffix('Z')?;
    let time = time.split_once('.').map_or(time, |(head, _frac)| head);

    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;

    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let second: i64 = time_parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        // 60 is a leap second: real, and not worth rejecting a whole entry's
        // age over, so it clamps into the minute rather than failing.
        || !(0..=60).contains(&second)
    {
        return None;
    }

    // days_from_civil (Howard Hinnant): civil date → days since 1970-01-01.
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;

    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second;
    let secs = u64::try_from(secs).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

fn parse_block(value: &serde_json::Value) -> Block {
    match value.get("type").and_then(|v| v.as_str()) {
        Some("text") => value
            .get("text")
            .and_then(|v| v.as_str())
            .map(|t| Block::Text(cap(t)))
            .unwrap_or(Block::Unknown),
        Some("thinking") => Block::Thinking,
        Some("tool_use") => Block::ToolCall {
            name: value
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("tool")
                .to_string(),
        },
        Some("tool_result") => Block::ToolResult {
            preview: cap(&tool_result_text(value)),
        },
        _ => Block::Unknown,
    }
}

/// `tool_result.content` is mixed in practice: usually a string, sometimes a
/// list of typed blocks. Both shapes flatten to text; anything else is empty.
fn tool_result_text(value: &serde_json::Value) -> String {
    match value.get("content") {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// How much of a turn to render.
///
/// Three fixed global levels, deliberately: the review round for #246 landed
/// on "plain text, one wrap style, no per-node state" as the boundary that
/// holds. Per-node expandable UI turns a view into a tree editor, and every
/// past attempt to add "just one" per-entry flag has become an iceberg.
///
/// Cycling levels re-reads the transcript rather than caching all three
/// renderings per entry: a 60k-message session would otherwise triple the
/// memory the panel holds for a mode the user is not looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TranscriptDetail {
    /// Prompt + assistant prose only. What the pre-#246 panel roughly showed.
    #[default]
    Reply,
    /// Adds tool calls as a fixed one-line marker (`⚙ Edit`). Never the
    /// output — that is the whole point of the "collapsed" step.
    Collapsed,
    /// Adds tool call output as well, still capped by [`MAX_BLOCK_BYTES`] per
    /// block so a 1.4 MB `tool_result` cannot reach the renderer.
    Full,
}

impl TranscriptDetail {
    /// A short label for the panel chrome — matches the on-disk config
    /// enum variants in kebab so `Debug`-derived strings are never surfaced.
    pub fn label(self) -> &'static str {
        match self {
            Self::Reply => "replies",
            Self::Collapsed => "with tool calls",
            Self::Full => "with tool output",
        }
    }

    /// Cycle to the next level. Fixed order so muscle memory works: the
    /// panel's Tab key is a "give me more" gesture and wraps back to Reply
    /// after Full.
    pub fn next(self) -> Self {
        match self {
            Self::Reply => Self::Collapsed,
            Self::Collapsed => Self::Full,
            Self::Full => Self::Reply,
        }
    }
}

/// Flatten events into the turns a reader actually shows: one entry per
/// message that carries content at `detail`, in file order.
///
/// The three levels differ only in which block kinds contribute a line:
///
/// * [`TranscriptDetail::Reply`] keeps text blocks
/// * [`TranscriptDetail::Collapsed`] adds `⚙ ToolName` for each tool_use
/// * [`TranscriptDetail::Full`] adds the (already-capped) tool_result preview
///
/// A user message that ends up carrying only tool_result content is
/// re-labelled as an assistant turn: it renders in the reply palette rather
/// than the "you typed this" palette, which is what tool output actually is.
pub fn turns_at_level(
    events: &[TranscriptEvent],
    detail: TranscriptDetail,
) -> Vec<(Role, String, Option<SystemTime>)> {
    let mut turns = Vec::new();
    for event in events {
        let TranscriptEvent::Message { role, blocks, at } = event else {
            continue;
        };
        let mut pieces: Vec<String> = Vec::new();
        let mut only_tool_result = true;
        for block in blocks {
            match block {
                Block::Text(text) => {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        pieces.push(trimmed.to_string());
                    }
                    only_tool_result = false;
                }
                Block::ToolCall { name } if detail != TranscriptDetail::Reply => {
                    pieces.push(format!("\u{2699} {name}"));
                    only_tool_result = false;
                }
                Block::ToolResult { preview } if detail == TranscriptDetail::Full => {
                    let trimmed = preview.trim();
                    if !trimmed.is_empty() {
                        pieces.push(trimmed.to_string());
                    }
                }
                _ => {}
            }
        }
        if pieces.is_empty() {
            continue;
        }
        // A user message consisting entirely of tool_result blocks is tool
        // output surfaced back to the model — attributing it to "the user" in
        // the panel's palette would misread who spoke.
        let display_role = if *role == Role::User && only_tool_result {
            Role::Assistant
        } else {
            *role
        };
        turns.push((display_role, pieces.join("\n\n"), *at));
    }
    turns
}

/// Read and flatten a Claude session transcript on a worker thread, handing
/// the turns (rendered at `detail`) back through `deliver`.
///
/// Transcripts reach ~15 MB, so this must never run on the UI thread. Failure
/// is silent by design: the panel keeps whatever it already had, and nothing
/// about the file's *content* is ever logged.
pub fn spawn_load<F>(
    home: std::path::PathBuf,
    session_id: String,
    detail: TranscriptDetail,
    deliver: F,
) where
    F: FnOnce(Vec<(Role, String, Option<SystemTime>)>) + Send + 'static,
{
    std::thread::spawn(move || {
        // Every path delivers, empty on failure: the receiver treats empty as
        // "keep what you have and re-arm", so a transcript that does not exist
        // yet (the first hook can beat the agent's first write) is retried
        // rather than disabling hydration for the rest of the session.
        let Some(path) = crate::agent_resume::claude_transcript_path(&home, &session_id) else {
            crate::logging::transcript_unreadable(&session_id, "no transcript on disk");
            deliver(Vec::new());
            return;
        };
        match ClaudeTranscript.read(&path) {
            Ok(read) => {
                if read.writer_newer_than_tested {
                    crate::logging::transcript_writer_newer_than_tested(
                        read.writer_version.as_deref().unwrap_or("unknown"),
                    );
                }
                deliver(turns_at_level(&read.events, detail));
            }
            Err(err) => {
                crate::logging::transcript_unreadable(&session_id, &format!("{err:?}"));
                deliver(Vec::new());
            }
        }
    });
}

/// Truncate on a char boundary so a multi-MB body can never reach a renderer.
fn cap(text: &str) -> String {
    if text.len() <= MAX_BLOCK_BYTES {
        return text.to_string();
    }
    let mut end = MAX_BLOCK_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let elided = text.len() - end;
    format!("{}\n[{elided} bytes elided]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(src: &str) -> TranscriptRead {
        read_lines(src.as_bytes()).expect("kernel parse")
    }

    #[test]
    fn string_and_block_content_both_parse_to_text() {
        // Both shapes occur in real transcripts for the same role.
        let out = read(concat!(
            r#"{"type":"user","message":{"role":"user","content":"plain string"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"block form"}]}}"#,
            "\n"
        ));
        assert_eq!(
            out.events,
            vec![
                TranscriptEvent::Message {
                    role: Role::User,
                    blocks: vec![Block::Text("plain string".into())],
                    at: None,
                },
                TranscriptEvent::Message {
                    role: Role::Assistant,
                    blocks: vec![Block::Text("block form".into())],
                    at: None,
                },
            ]
        );
    }

    #[test]
    fn unknown_entry_types_and_block_kinds_degrade_instead_of_failing() {
        // A newer writer adds entry types and block kinds; neither may error.
        let out = read(concat!(
            r#"{"type":"pr-link","url":"x"}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"future_kind"},{"type":"text","text":"kept"}]}}"#,
            "\n"
        ));
        assert_eq!(out.events[0], TranscriptEvent::Meta);
        assert_eq!(
            out.events[1],
            TranscriptEvent::Message {
                role: Role::Assistant,
                blocks: vec![Block::Unknown, Block::Text("kept".into())],
                at: None,
            }
        );
    }

    #[test]
    fn meta_and_sidechain_entries_are_hidden_compaction_is_a_divider() {
        let out = read(concat!(
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"caveat"}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":true,"message":{"role":"assistant","content":"subagent"}}"#,
            "\n",
            r#"{"type":"user","isCompactSummary":true,"message":{"role":"user","content":"summary"}}"#,
            "\n"
        ));
        assert_eq!(
            out.events,
            vec![
                TranscriptEvent::Meta,
                TranscriptEvent::Meta,
                TranscriptEvent::Compacted
            ]
        );
    }

    #[test]
    fn tool_calls_keep_their_name_and_results_flatten_both_shapes() {
        let out = read(concat!(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Edit"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"stdout"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}]}}"#,
            "\n"
        ));
        let kinds: Vec<_> = out
            .events
            .iter()
            .filter_map(|e| match e {
                TranscriptEvent::Message { blocks, .. } => blocks.first().cloned(),
                _ => None,
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                Block::ToolCall {
                    name: "Edit".into()
                },
                Block::ToolResult {
                    preview: "stdout".into()
                },
                Block::ToolResult {
                    preview: "a\nb".into()
                },
            ]
        );
    }

    #[test]
    fn oversized_bodies_are_capped_on_a_char_boundary() {
        // A 1.35 MB tool_result was measured in a real transcript; it must not
        // reach a renderer intact.
        let body = "é".repeat(MAX_BLOCK_BYTES);
        let line = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "tool_result", "content": body}]}
        })
        .to_string();
        let out = read(&format!("{line}\n"));
        let TranscriptEvent::Message { blocks, .. } = &out.events[0] else {
            panic!("expected a message");
        };
        let Block::ToolResult { preview } = &blocks[0] else {
            panic!("expected a tool result");
        };
        assert!(preview.len() < MAX_BLOCK_BYTES + 64, "body was not capped");
        assert!(preview.contains("bytes elided"), "no elision marker");
    }

    #[test]
    fn a_partial_trailing_line_is_ignored_not_parsed() {
        // The writer appends live: the last line may be half-written.
        let out = read(concat!(
            r#"{"type":"user","message":{"role":"user","content":"complete"}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","conte"#
        ));
        assert_eq!(out.events.len(), 1, "partial line must not yield an event");
    }

    #[test]
    fn wholesale_garbage_reports_format_moved_rather_than_rendering_nothing() {
        let err =
            read_lines("not json\nalso not json\n".as_bytes()).expect_err("garbage must not parse");
        assert!(matches!(err, TranscriptError::FormatMoved { .. }));
    }

    #[test]
    fn a_newer_writer_still_renders_but_raises_the_drift_flag() {
        let out = read(concat!(
            r#"{"type":"user","version":"9.9.9","message":{"role":"user","content":"hi"}}"#,
            "\n"
        ));
        assert_eq!(out.writer_version.as_deref(), Some("9.9.9"));
        assert!(out.writer_newer_than_tested);
        assert_eq!(out.events.len(), 1, "drift must not suppress content");
    }

    /// Opt-in drift check against the real transcripts on this machine:
    /// `cargo test -- --ignored real_transcripts`. Not part of the normal run
    /// (it depends on `~/.claude`, which CI does not have), but it is the only
    /// thing that catches the format moving under us. Asserts on structure
    /// only — it must never print transcript content.
    #[test]
    #[ignore = "requires ~/.claude/projects transcripts on this machine"]
    fn real_transcripts_still_satisfy_the_kernel() {
        let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
            return;
        };
        let projects = home.join(".claude").join("projects");
        let Ok(dirs) = std::fs::read_dir(&projects) else {
            return;
        };
        let mut files = 0usize;
        let mut messages = 0usize;
        for dir in dirs.flatten() {
            let Ok(entries) = std::fs::read_dir(dir.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|ext| ext != "jsonl") {
                    continue;
                }
                let read = ClaudeTranscript.read(&path).unwrap_or_else(|err| {
                    panic!("kernel parse failed on a real transcript: {err:?}")
                });
                files += 1;
                messages += read
                    .events
                    .iter()
                    .filter(|e| matches!(e, TranscriptEvent::Message { .. }))
                    .count();
            }
        }
        assert!(files > 0, "no transcripts found to check");
        assert!(
            messages > 0,
            "parsed {files} transcripts but found no messages"
        );
        // Structure only -- never transcript content.
        eprintln!("checked {files} transcripts, {messages} messages"); // guardrails-ok(no-debug-leftovers): opt-in drift check run by hand; the count IS the result
    }

    #[test]
    fn timestamps_parse_to_wall_clock_and_bad_ones_cost_only_the_age() {
        let out = read(concat!(
            r#"{"type":"user","timestamp":"2026-08-11T06:51:09.988Z","message":{"role":"user","content":"a"}}"#,
            "\n",
            r#"{"type":"user","timestamp":"not a date","message":{"role":"user","content":"b"}}"#,
            "\n"
        ));
        let ats: Vec<_> = out
            .events
            .iter()
            .filter_map(|e| match e {
                TranscriptEvent::Message { at, .. } => Some(*at),
                _ => None,
            })
            .collect();
        // 2026-08-11T06:51:09Z — checked against the epoch value directly so a
        // broken days-from-civil implementation cannot pass.
        assert_eq!(
            ats[0],
            Some(UNIX_EPOCH + Duration::from_secs(1_786_431_069))
        );
        assert_eq!(ats[1], None, "an unparseable stamp must not drop content");
        assert_eq!(out.events.len(), 2);
    }

    #[test]
    fn reply_detail_keeps_prose_and_drops_tool_traffic() {
        let out = read(concat!(
            r#"{"type":"user","message":{"role":"user","content":"do the thing"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Edit"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking"},{"type":"text","text":"done"}]}}"#,
            "\n"
        ));
        let turns = turns_at_level(&out.events, TranscriptDetail::Reply);
        assert_eq!(
            turns
                .iter()
                .map(|(role, text, _)| (*role, text.as_str()))
                .collect::<Vec<_>>(),
            vec![(Role::User, "do the thing"), (Role::Assistant, "done"),],
            "tool call/result turns carry no prose and must not become entries"
        );
    }

    #[test]
    fn a_known_writer_does_not_raise_the_drift_flag() {
        let out = read(concat!(
            r#"{"type":"user","version":"2.1.220","message":{"role":"user","content":"hi"}}"#,
            "\n"
        ));
        assert!(!out.writer_newer_than_tested);
    }

    /// The three detail levels are the whole feature; assert directly that
    /// each admits exactly the right block kinds, or the panel silently
    /// starts leaking tool traffic into Reply mode again.
    #[test]
    fn detail_levels_gate_which_block_kinds_render() {
        let out = read(concat!(
            r#"{"type":"user","message":{"role":"user","content":"do the thing"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"working"},{"type":"tool_use","name":"Edit"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"stdout body"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}"#,
            "\n"
        ));

        let reply: Vec<_> = turns_at_level(&out.events, TranscriptDetail::Reply)
            .into_iter()
            .map(|(r, t, _)| (r, t))
            .collect();
        assert_eq!(
            reply,
            vec![
                (Role::User, "do the thing".into()),
                (Role::Assistant, "working".into()),
                (Role::Assistant, "done".into()),
            ],
            "Reply must never surface tool traffic"
        );

        let collapsed: Vec<_> = turns_at_level(&out.events, TranscriptDetail::Collapsed)
            .into_iter()
            .map(|(r, t, _)| (r, t))
            .collect();
        assert_eq!(
            collapsed,
            vec![
                (Role::User, "do the thing".into()),
                (Role::Assistant, "working\n\n\u{2699} Edit".into()),
                (Role::Assistant, "done".into()),
            ],
            "Collapsed adds ⚙ tool-call markers but must not include output"
        );

        let full: Vec<_> = turns_at_level(&out.events, TranscriptDetail::Full)
            .into_iter()
            .map(|(r, t, _)| (r, t))
            .collect();
        assert_eq!(
            full,
            vec![
                (Role::User, "do the thing".into()),
                (Role::Assistant, "working\n\n\u{2699} Edit".into()),
                // Tool_result-only user messages read as assistant output,
                // not as "the user typed this".
                (Role::Assistant, "stdout body".into()),
                (Role::Assistant, "done".into()),
            ]
        );
    }

    /// Full mode reads the same capped preview as the parser produced, so an
    /// enormous tool_result can never blow up the render buffer by way of a
    /// detail cycle.
    #[test]
    fn full_detail_inherits_the_per_block_byte_cap() {
        let body = "a".repeat(MAX_BLOCK_BYTES * 2);
        let line = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "tool_result", "content": body}]}
        })
        .to_string();
        let out = read(&format!("{line}\n"));
        let turns = turns_at_level(&out.events, TranscriptDetail::Full);
        assert_eq!(turns.len(), 1);
        assert!(
            turns[0].1.len() < MAX_BLOCK_BYTES + 128,
            "Full mode must inherit the block cap, not resurrect the raw body"
        );
        assert!(turns[0].1.contains("bytes elided"));
    }

    #[test]
    fn detail_next_cycles_through_all_three_and_wraps() {
        assert_eq!(TranscriptDetail::Reply.next(), TranscriptDetail::Collapsed);
        assert_eq!(TranscriptDetail::Collapsed.next(), TranscriptDetail::Full);
        assert_eq!(TranscriptDetail::Full.next(), TranscriptDetail::Reply);
    }
}
