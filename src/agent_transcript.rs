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
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Longest block body handed to a renderer. A single `tool_result` line of
/// 1.35 MB was measured in a real transcript; re-wrapping that would stall the
/// UI, so bodies are truncated with an elision marker instead.
pub const MAX_BLOCK_BYTES: usize = 128 * 1024;

/// Give up if more than this fraction of lines fail the kernel parse — that
/// means the format moved, not that one entry is odd.
const MAX_UNPARSED_RATIO: f64 = 0.05;

/// Writer versions this parser has been exercised against. A newer writer
/// still renders (leniently); callers may surface a "newer than tested" hint.
///
/// Bumping this is licensed by EVIDENCE, not by editing the number: run
/// `cargo nextest run --run-ignored all -E 'test(real_transcripts)'`, which
/// parses every transcript on the machine and fails if the kernel chokes.
/// Left stale it stops being a sentinel — at 2.1.227 against a 2.1.238 writer
/// the "untested" warning fired on every current session, so the one signal
/// that the format had moved was indistinguishable from the happy path
/// (#337). If it is behind again, re-run the check and move it; do not
/// silence the warning.
pub const KNOWN_GOOD_WRITER_MAX: (u32, u32, u32) = (2, 1, 238);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// Byte range of the line each event was parsed from, relative to the
    /// start of the reader. Parallel to `events` and pushed with it, so an
    /// event's index is also its span's index. `agent.history` (#276) pages a
    /// transcript by byte offset, and a turn's offset is its line's offset.
    pub line_spans: Vec<std::ops::Range<u64>>,
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
    // Where the next line starts, relative to the reader's own origin. The
    // caller adds the window's absolute offset; this function has no idea
    // where in the file it was handed.
    let mut consumed: u64 = 0;
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
        let line_start = consumed;
        consumed += line.len() as u64;
        let line_end = consumed;
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

        // Pushed as a pair: an event and the span it came from must never
        // get out of step, and the only way to guarantee that is to append
        // them together.
        out.events.push(parse_entry(&value));
        out.line_spans.push(line_start..line_end);
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

/// Largest slice of a transcript one [`read_history`] call parses (#276).
///
/// `agent.history` exists to be polled, and a poll that re-reads a 15 MB file
/// is not pollable: the read runs on the task that also draws the UI. A
/// transcript is newline-delimited JSON, so a window is exact rather than
/// approximate — seek, align to a line boundary, parse forward. A caller that
/// hands `next_cursor` back parses only what was appended since, which is the
/// steady state of a poll and is normally a few kilobytes.
pub const HISTORY_WINDOW_BYTES: u64 = 1024 * 1024;

/// One turn of an agent's history, rendered at the detail the caller asked
/// for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryTurn {
    pub role: Role,
    pub text: String,
    /// When the writer stamped the entry, if it stamped one.
    pub at: Option<SystemTime>,
}

/// One page of an agent's history plus the cursor that resumes after it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HistoryPage {
    pub turns: Vec<HistoryTurn>,
    /// Byte offset the first returned turn was parsed from. Handing this back
    /// as `cursor` re-reads exactly this page.
    pub cursor: u64,
    /// Byte offset to hand back as `cursor` to read what comes next.
    pub next_cursor: u64,
    /// Transcript length when the page was cut. `next_cursor < len` means
    /// more is already on disk.
    pub len: u64,
    /// Turns older than `cursor` exist and this call skipped them, either
    /// because it started at the tail or because `limit` cut them.
    pub truncated: bool,
}

impl HistoryPage {
    /// Whether more transcript already sits after [`Self::next_cursor`]. A
    /// caller seeing this page again immediately, rather than on its next
    /// poll, is the difference between paging and waiting.
    pub fn more(&self) -> bool {
        self.next_cursor < self.len
    }
}

/// Read one bounded page of a transcript at `detail`, starting from `cursor`
/// (or from the tail when there is none).
///
/// The rendering goes through [`turns_at_level`] with the detail this call
/// asked for, never through whatever a panel happens to be hydrated at: an
/// agent's view of history must not change because a human clicked something.
pub fn read_history(
    path: &Path,
    detail: TranscriptDetail,
    cursor: Option<u64>,
    limit: usize,
) -> Result<HistoryPage, TranscriptError> {
    let limit = limit.max(1);
    let mut file = File::open(path).map_err(|_| TranscriptError::Unreadable)?;
    let len = file
        .metadata()
        .map_err(|_| TranscriptError::Unreadable)?
        .len();

    // A cursor past the end means the file shrank under the caller — Claude
    // prunes transcripts, and the same session id can be rewritten. Serving
    // nothing forever would be the silent failure. Restart from the tail: the
    // returned `cursor` is below the one that was handed in, which is how a
    // caller sees it happened.
    let from_tail = cursor.is_none_or(|at| at > len);
    let requested = if from_tail {
        len.saturating_sub(HISTORY_WINDOW_BYTES)
    } else {
        cursor.unwrap_or(0)
    };

    // A tail read starts at the beginning of the entry the offset falls in.
    // A cursored one starts at the first entry at or after it. The difference
    // matters when a single entry is larger than the window: skipping forward
    // out of it would answer "nothing here" for a transcript whose newest
    // turn is one big tool result, which is precisely the turn that was asked
    // for. Reading back into it costs at most that one entry.
    let (window_start, take) = if from_tail {
        let start = align_back_to_line_start(&mut file, requested)?;
        (start, len.saturating_sub(start))
    } else {
        let start = align_to_line_start(&mut file, requested)?;
        (start, len.saturating_sub(start).min(HISTORY_WINDOW_BYTES))
    };
    file.seek(SeekFrom::Start(window_start))
        .map_err(|_| TranscriptError::Unreadable)?;
    let mut buf = Vec::with_capacity(usize::try_from(take).unwrap_or(0));
    (&mut file)
        .take(take)
        .read_to_end(&mut buf)
        .map_err(|_| TranscriptError::Unreadable)?;
    // Never hand a half-written trailing line to the parser: the window edge
    // is as likely to land mid-entry as the live writer is.
    let mut complete = buf
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |i| i + 1);
    if complete == 0 && window_start + (buf.len() as u64) < len {
        // The window ended inside a single entry bigger than itself. Finish
        // that entry rather than return an empty page with an unchanged
        // cursor, which is a poll that never advances.
        let read_to_line_end = BufReader::new(&mut file)
            .read_until(b'\n', &mut buf)
            .map_err(|_| TranscriptError::Unreadable)?;
        if read_to_line_end > 0 && buf.last() == Some(&b'\n') {
            complete = buf.len();
        }
    }
    buf.truncate(complete);
    let window_end = window_start + complete as u64;

    let read = read_lines(&buf[..])?;
    let mut turns: Vec<(HistoryTurn, std::ops::Range<u64>)> = Vec::new();
    for (event, span) in read.events.iter().zip(read.line_spans.iter()) {
        // One event yields at most one turn, so a turn's byte span IS its
        // event's line span — that is what makes the cursor exact.
        for (role, text, at) in turns_at_level(std::slice::from_ref(event), detail) {
            turns.push((
                HistoryTurn { role, text, at },
                window_start + span.start..window_start + span.end,
            ));
        }
    }

    let mut truncated = from_tail && window_start > 0;
    let mut limited = false;
    if turns.len() > limit {
        if from_tail {
            // No cursor means "what happened lately", so the newest turns are
            // the ones to keep.
            turns.drain(..turns.len() - limit);
            truncated = true;
        } else {
            turns.truncate(limit);
            limited = true;
        }
    }

    Ok(HistoryPage {
        cursor: turns.first().map_or(window_start, |(_, span)| span.start),
        // When `limit` cut the page short the cursor must stop at the last
        // turn actually returned, or the next poll skips the rest. Otherwise
        // advance past every complete line read, including the meta ones that
        // rendered no turn — leaving those behind would make a settled
        // transcript look like it always had more.
        next_cursor: if limited {
            turns.last().map_or(window_end, |(_, span)| span.end)
        } else {
            window_end
        },
        turns: turns.into_iter().map(|(turn, _)| turn).collect(),
        len,
        truncated,
    })
}

/// The offset of the first complete line at or after `at`.
///
/// Reads the byte BEFORE `at` rather than scanning forward blind: when that
/// byte is the newline ending the previous line, `at` already starts a line
/// and skipping to the next one would silently drop a whole turn.
fn align_to_line_start(file: &mut File, at: u64) -> Result<u64, TranscriptError> {
    if at == 0 {
        return Ok(0);
    }
    file.seek(SeekFrom::Start(at - 1))
        .map_err(|_| TranscriptError::Unreadable)?;
    let mut probe = [0u8; 1];
    match file.read_exact(&mut probe) {
        Ok(()) if probe[0] == b'\n' => return Ok(at),
        Ok(()) => {}
        Err(_) => return Err(TranscriptError::Unreadable),
    }
    let mut reader = BufReader::new(&mut *file);
    let mut skipped = Vec::new();
    let taken = reader
        .read_until(b'\n', &mut skipped)
        .map_err(|_| TranscriptError::Unreadable)?;
    Ok(at + taken as u64)
}

/// The offset of the start of the line that CONTAINS `at`.
///
/// Scans backwards, bounded to one extra window: a file with no newline
/// within 2 MiB of the tail has no complete entry there to return, and
/// walking a 15 MB transcript to prove it is the cost this whole verb exists
/// to avoid. Falls back to forward alignment in that case.
fn align_back_to_line_start(file: &mut File, at: u64) -> Result<u64, TranscriptError> {
    const CHUNK: u64 = 64 * 1024;
    let floor = at.saturating_sub(HISTORY_WINDOW_BYTES);
    let mut end = at;
    while end > floor {
        let start = end.saturating_sub(CHUNK).max(floor);
        let mut buf = vec![0u8; usize::try_from(end - start).unwrap_or(0)];
        file.seek(SeekFrom::Start(start))
            .map_err(|_| TranscriptError::Unreadable)?;
        file.read_exact(&mut buf)
            .map_err(|_| TranscriptError::Unreadable)?;
        if let Some(index) = buf.iter().rposition(|byte| *byte == b'\n') {
            return Ok(start + index as u64 + 1);
        }
        end = start;
    }
    if floor == 0 {
        return Ok(0);
    }
    align_to_line_start(file, at)
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
            // Expected, not a fault: the first hook routinely beats the agent's
            // first write, and the receiver re-arms on empty.
            crate::logging::transcript_absent(&session_id);
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

    /// #337: the sentinel is only a sentinel while it sits at or above what
    /// agents actually write. Left behind it fires on every current session,
    /// which is how it stopped being a signal. This pins the intent rather
    /// than the number: whatever `KNOWN_GOOD_WRITER_MAX` says, a writer AT
    /// that version must be silent, and only something past it may flag.
    #[test]
    fn the_drift_flag_is_silent_at_the_tested_ceiling() {
        let (major, minor, patch) = KNOWN_GOOD_WRITER_MAX;
        let at_ceiling = read(&format!(
            "{}\n",
            format_args!(
                r#"{{"type":"user","version":"{major}.{minor}.{patch}","message":{{"role":"user","content":"a"}}}}"#
            )
        ));
        assert!(
            !at_ceiling.writer_newer_than_tested,
            "the version the parser is tested against must not warn",
        );

        let past_ceiling = read(&format!(
            "{}\n",
            format_args!(
                r#"{{"type":"user","version":"{major}.{minor}.{next}","message":{{"role":"user","content":"a"}}}}"#,
                next = patch + 1
            )
        ));
        assert!(
            past_ceiling.writer_newer_than_tested,
            "one patch past the ceiling is what the flag exists for",
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

    // ---- #276: bounded, resumable history reads ---------------------------

    /// A transcript on disk, in a directory this test owns.
    fn history_fixture(name: &str, body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("flock-history-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let path = dir.join("transcript.jsonl");
        std::fs::write(&path, body).expect("fixture transcript");
        path
    }

    fn user(text: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({
                "type": "user",
                "message": {"role": "user", "content": text}
            })
        )
    }

    fn assistant_with_tool(text: &str, tool: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({
                "type": "assistant",
                "message": {"role": "assistant", "content": [
                    {"type": "text", "text": text},
                    {"type": "tool_use", "name": tool},
                ]}
            })
        )
    }

    fn texts(page: &HistoryPage) -> Vec<&str> {
        page.turns.iter().map(|turn| turn.text.as_str()).collect()
    }

    #[test]
    fn history_without_a_cursor_returns_the_newest_turns_and_admits_what_it_skipped() {
        let body: String = (0..10).map(|i| user(&format!("turn {i}"))).collect();
        let path = history_fixture("tail", &body);

        let page = read_history(&path, TranscriptDetail::Reply, None, 3).expect("history");

        assert_eq!(texts(&page), vec!["turn 7", "turn 8", "turn 9"]);
        assert!(
            page.truncated,
            "seven older turns were dropped; saying otherwise would read as `that is all there is`"
        );
        assert_eq!(page.next_cursor, page.len, "the tail read reaches the end");
        let _ = std::fs::remove_file(path);
    }

    /// The poll loop the whole verb exists for: hand back `next_cursor` and
    /// get only what was appended since.
    #[test]
    fn history_resumes_from_next_cursor_and_returns_only_what_was_appended() {
        let body: String = (0..3).map(|i| user(&format!("turn {i}"))).collect();
        let path = history_fixture("resume", &body);

        let first = read_history(&path, TranscriptDetail::Reply, None, 20).expect("history");
        assert_eq!(texts(&first), vec!["turn 0", "turn 1", "turn 2"]);
        assert!(!first.truncated, "the whole file fitted");

        let quiet = read_history(&path, TranscriptDetail::Reply, Some(first.next_cursor), 20)
            .expect("history");
        assert!(
            quiet.turns.is_empty(),
            "a poll against an unchanged transcript must return nothing, not repeat itself"
        );
        assert_eq!(quiet.next_cursor, first.next_cursor);
        assert!(!quiet.more());

        let mut appended = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("append");
        std::io::Write::write_all(&mut appended, user("turn 3").as_bytes()).expect("write");
        drop(appended);

        let next = read_history(&path, TranscriptDetail::Reply, Some(first.next_cursor), 20)
            .expect("history");
        assert_eq!(texts(&next), vec!["turn 3"]);
        let _ = std::fs::remove_file(path);
    }

    /// A cursor lands exactly on a line boundary, which is the case a naive
    /// "skip to the next newline" alignment silently eats a turn on.
    #[test]
    fn history_paging_under_a_limit_loses_no_turn() {
        let body: String = (0..5).map(|i| user(&format!("turn {i}"))).collect();
        let path = history_fixture("paging", &body);

        let mut seen: Vec<String> = Vec::new();
        let mut cursor = Some(0);
        for _ in 0..5 {
            let page = read_history(&path, TranscriptDetail::Reply, cursor, 2).expect("history");
            seen.extend(page.turns.iter().map(|turn| turn.text.clone()));
            if !page.more() {
                break;
            }
            cursor = Some(page.next_cursor);
        }

        assert_eq!(
            seen,
            vec!["turn 0", "turn 1", "turn 2", "turn 3", "turn 4"],
            "paging must visit every turn exactly once"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn history_re_reads_the_same_page_when_handed_back_its_own_cursor() {
        let body: String = (0..6).map(|i| user(&format!("turn {i}"))).collect();
        let path = history_fixture("stable", &body);

        let page = read_history(&path, TranscriptDetail::Reply, None, 2).expect("history");
        let again =
            read_history(&path, TranscriptDetail::Reply, Some(page.cursor), 2).expect("history");

        assert_eq!(texts(&page), texts(&again));
        assert_eq!(page.cursor, again.cursor);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn history_never_parses_a_half_written_trailing_line() {
        let mut body = user("complete");
        body.push_str(r#"{"type":"user","message":{"role":"user","content":"half w"#);
        let path = history_fixture("partial", &body);

        let page = read_history(&path, TranscriptDetail::Reply, None, 20).expect("history");

        assert_eq!(texts(&page), vec!["complete"]);
        assert!(
            page.next_cursor < page.len,
            "the cursor must stop before the partial line so the next poll re-reads it whole"
        );
        assert!(page.more(), "there are bytes the reader deliberately left");
        let _ = std::fs::remove_file(path);
    }

    /// The detail is the CALLER's. Nothing about this read consults, or is
    /// affected by, the level a panel happens to be hydrated at.
    #[test]
    fn history_renders_at_the_requested_detail() {
        let path = history_fixture("detail", &assistant_with_tool("thinking about it", "Edit"));

        let reply = read_history(&path, TranscriptDetail::Reply, None, 20).expect("history");
        let collapsed =
            read_history(&path, TranscriptDetail::Collapsed, None, 20).expect("history");

        assert_eq!(texts(&reply), vec!["thinking about it"]);
        assert_eq!(
            texts(&collapsed),
            vec!["thinking about it\n\n\u{2699} Edit"]
        );
        let _ = std::fs::remove_file(path);
    }

    /// Claude prunes transcripts, so a cursor can outlive the bytes it named.
    /// Serving nothing forever would be the silent failure.
    #[test]
    fn history_restarts_from_the_tail_when_the_cursor_outlives_the_file() {
        let path = history_fixture("shrunk", &user("all that is left"));
        let len = std::fs::metadata(&path).expect("meta").len();

        let page =
            read_history(&path, TranscriptDetail::Reply, Some(len + 4096), 20).expect("history");

        assert_eq!(texts(&page), vec!["all that is left"]);
        assert!(
            page.cursor < len + 4096,
            "the returned cursor is below the one handed in — that is how a caller sees the reset"
        );
        let _ = std::fs::remove_file(path);
    }

    /// The window is the poll-safety property: a transcript far larger than
    /// [`HISTORY_WINDOW_BYTES`] must not be parsed whole to answer one call.
    #[test]
    fn history_reads_a_bounded_window_of_a_large_transcript() {
        let filler = "x".repeat(4096);
        let mut body = String::new();
        let mut turns = 0;
        while body.len() as u64 <= HISTORY_WINDOW_BYTES * 2 {
            body.push_str(&user(&format!("turn {turns} {filler}")));
            turns += 1;
        }
        let path = history_fixture("window", &body);
        let len = std::fs::metadata(&path).expect("meta").len();

        let page = read_history(&path, TranscriptDetail::Reply, None, 500).expect("history");

        assert!(
            page.cursor >= len - HISTORY_WINDOW_BYTES - 8192,
            "the read must start near the tail, not at the top of a {len}-byte file"
        );
        assert!(page.truncated);
        assert!(
            page.turns
                .last()
                .is_some_and(|turn| turn.text.starts_with(&format!("turn {}", turns - 1))),
            "the newest turn is the one a caller with no cursor came for"
        );
        let _ = std::fs::remove_file(path);
    }

    /// A single `tool_result` line of 1.35 MB was measured in a real
    /// transcript, so an entry larger than the window is not hypothetical. A
    /// tail read that skipped forward out of it would answer "nothing here"
    /// for the very turn it was asked about.
    #[test]
    fn history_returns_an_entry_larger_than_the_window() {
        let huge = "y".repeat(usize::try_from(HISTORY_WINDOW_BYTES).unwrap() + 4096);
        let body = user("small opener") + &user(&huge);
        let path = history_fixture("oversize", &body);

        let page = read_history(&path, TranscriptDetail::Reply, None, 20).expect("history");

        assert_eq!(
            page.turns.len(),
            1,
            "the newest turn is the over-long one, and it must not vanish"
        );
        assert!(page.turns[0].text.starts_with("yyy"));
        assert_eq!(page.next_cursor, page.len);
        let _ = std::fs::remove_file(path);
    }

    /// The same entry, reached by paging rather than by tailing. A window that
    /// ends inside one entry must still advance, or a poller re-reads the same
    /// offset forever.
    #[test]
    fn history_paging_advances_through_an_entry_larger_than_the_window() {
        let huge = "z".repeat(usize::try_from(HISTORY_WINDOW_BYTES).unwrap() + 4096);
        let body = user(&huge) + &user("after the giant");
        let path = history_fixture("oversize-paging", &body);

        let first = read_history(&path, TranscriptDetail::Reply, Some(0), 20).expect("history");
        assert!(
            first.next_cursor > 0,
            "a page that returned nothing and did not advance is an infinite poll"
        );

        let second = read_history(&path, TranscriptDetail::Reply, Some(first.next_cursor), 20)
            .expect("history");
        let all: Vec<&str> = first
            .turns
            .iter()
            .chain(second.turns.iter())
            .map(|turn| turn.text.as_str())
            .collect();
        assert_eq!(all.len(), 2, "both entries arrive across the two pages");
        assert_eq!(all[1], "after the giant");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn history_on_a_missing_transcript_is_unreadable_not_a_panic() {
        let missing = std::env::temp_dir().join(format!(
            "flock-history-missing-{}.jsonl",
            std::process::id()
        ));
        let err = read_history(&missing, TranscriptDetail::Reply, None, 20);
        assert!(matches!(err, Err(TranscriptError::Unreadable)));
    }
}
