//! opencode's conversation, read from its SQLite store (#575).
//!
//! opencode writes no transcript file; a session lives in rows of its own
//! database (`opencode-stable.db` under the XDG data dir). Supervisors were
//! reading final reports out of it with `sqlite3` against a private schema
//! and a `substr` offset. This reads the same rows, read-only, and maps them
//! onto [`crate::agent_transcript`]'s turn model, so `agent.history` and
//! `agent.result` treat an opencode session the way they treat a Claude one.
//!
//! The schema (verified on opencode 1.18):
//!
//! * `message(id, session_id, time_created, data)`: `data.role` is `user` or
//!   `assistant`, and `data.time.completed` is set once an assistant turn
//!   has finished.
//! * `part(id, message_id, session_id, time_created, data)`: `data.type` is
//!   `text`, `reasoning`, `tool` (with `tool` and `state.output`), or
//!   bookkeeping (`step-start`, `step-finish`, `patch`, `file`) that carries
//!   no conversation.
//!
//! Compaction is mapped from the two shapes opencode's schema defines for it
//! (a `compaction` part, an assistant message with `summary: true`); no
//! compacted session was available to observe, so a fixture pins it.
//!
//! The same rules as the Claude kernel: read-only, bounded, never panic,
//! never log content. The database is opened `SQLITE_OPEN_READ_ONLY` and NOT
//! `immutable`, because opencode runs in WAL mode and `immutable` would read
//! around everything not yet checkpointed — the newest turns, which are the
//! ones asked for.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rusqlite::{params, Connection, OpenFlags};
use serde_json::Value;

use crate::agent_transcript::{cap, Block, Role, TranscriptError, TranscriptEvent};

/// Database file names, newest release channel first.
const DB_NAMES: &[&str] = &["opencode-stable.db", "opencode.db"];

/// How long a read waits on opencode's own write lock before giving up. The
/// caller runs on the task that also draws the UI, so this is a bound, not a
/// politeness.
const BUSY_TIMEOUT: Duration = Duration::from_millis(250);

/// The opencode database for this user: `$XDG_DATA_HOME/opencode/…`, else
/// `~/.local/share/opencode/…`. `None` when neither holds one.
pub fn db_path(home: &Path, xdg_data_home: Option<&Path>) -> Option<PathBuf> {
    let data = xdg_data_home
        .filter(|dir| dir.is_absolute())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".local/share"));
    DB_NAMES
        .iter()
        .map(|name| data.join("opencode").join(name))
        .find(|path| path.is_file())
}

/// Which messages of a session to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    /// The newest `n`, oldest first.
    Last(usize),
    /// Up to `n` created strictly after `ms`, oldest first.
    After { ms: u64, n: usize },
}

/// One opencode message, mapped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub blocks: Vec<Block>,
    pub created_ms: u64,
    /// An assistant message opencode has marked finished.
    pub completed: bool,
    /// An assistant message that ended its TURN, not just a step: opencode
    /// writes one assistant message per step, and every step but the last
    /// finishes with `finish: "tool-calls"` (8904 of 9076 on a real store).
    pub final_step: bool,
    /// This message starts a new epoch: a compaction summary.
    pub compaction: bool,
}

/// A session's messages in `window`, plus the newest message time in the
/// whole session (the paging horizon).
#[derive(Debug, Default)]
pub struct SessionRead {
    pub messages: Vec<Message>,
    pub newest_ms: u64,
    pub oldest_ms: u64,
    /// An `After` read stopped at its limit: more messages follow this page.
    pub has_more: bool,
}

impl SessionRead {
    /// Where an `After` read of this page should resume so nothing is lost:
    /// the last message's time, but never past an assistant message that is
    /// still being written — opencode inserts the message row when a step
    /// STARTS and its parts as they stream, so a cursor past it would never
    /// see its text. That message is sent again until it completes.
    pub fn resume_after(&self) -> Option<u64> {
        let last = self.messages.last()?.created_ms;
        let open = self
            .messages
            .iter()
            .find(|m| m.role == Role::Assistant && !m.completed)
            .map(|m| m.created_ms.saturating_sub(1));
        Some(open.map_or(last, |open| open.min(last)))
    }
}

pub fn read_session(
    db: &Path,
    session_id: &str,
    window: Window,
) -> Result<SessionRead, TranscriptError> {
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| TranscriptError::Unreadable)?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|_| TranscriptError::Unreadable)?;
    read_session_in(&conn, session_id, window)
}

fn moved() -> TranscriptError {
    TranscriptError::FormatMoved {
        parsed: 0,
        failed: 1,
    }
}

/// Split from [`read_session`] so tests drive it over an in-memory database.
fn read_session_in(
    conn: &Connection,
    session_id: &str,
    window: Window,
) -> Result<SessionRead, TranscriptError> {
    let (newest_ms, oldest_ms): (Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT MAX(time_created), MIN(time_created) FROM message WHERE session_id = ?1",
            params![session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| moved())?;
    let to_ms = |v: Option<i64>| v.and_then(|v| u64::try_from(v).ok()).unwrap_or(0);
    let mut out = SessionRead {
        messages: Vec::new(),
        newest_ms: to_ms(newest_ms),
        oldest_ms: to_ms(oldest_ms),
        has_more: false,
    };

    let rows: Vec<(String, i64, String)> = match window {
        Window::Last(n) => {
            let mut stmt = conn
                .prepare(
                    "SELECT id, time_created, data FROM message WHERE session_id = ?1 \
                     ORDER BY time_created DESC, id DESC LIMIT ?2",
                )
                .map_err(|_| moved())?;
            let mut rows = stmt
                .query_map(params![session_id, limit(n)], row_triple)
                .map_err(|_| moved())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| moved())?;
            rows.reverse();
            rows
        }
        Window::After { ms, n } => {
            let mut stmt = conn
                .prepare(
                    "SELECT id, time_created, data FROM message WHERE session_id = ?1 \
                     AND time_created > ?2 ORDER BY time_created, id LIMIT ?3",
                )
                .map_err(|_| moved())?;
            // One row past the page: if it shares the page's last
            // millisecond, a cursor of that millisecond would skip it, so the
            // page ends before the tie instead (unless the tie IS the page).
            let mut rows = stmt
                .query_map(
                    params![
                        session_id,
                        i64::try_from(ms).unwrap_or(i64::MAX),
                        limit(n.saturating_add(1))
                    ],
                    row_triple,
                )
                .map_err(|_| moved())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| moved())?;
            if rows.len() > n.max(1) {
                out.has_more = true;
                if let Some((_, beyond, _)) = rows.pop() {
                    let tied = rows
                        .iter()
                        .rev()
                        .take_while(|(_, at, _)| *at == beyond)
                        .count();
                    if tied < rows.len() {
                        rows.truncate(rows.len() - tied);
                    }
                }
            }
            rows
        }
    };

    // Every part of the page in one query, rather than one per message.
    let mut parts_of: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    if !rows.is_empty() {
        let marks = vec!["?"; rows.len()].join(",");
        let mut stmt = conn
            .prepare(&format!(
                "SELECT message_id, data FROM part WHERE message_id IN ({marks}) ORDER BY id"
            ))
            .map_err(|_| moved())?;
        let found = stmt
            .query_map(
                rusqlite::params_from_iter(rows.iter().map(|(id, _, _)| id)),
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|_| moved())?;
        for part in found {
            let (message_id, data) = part.map_err(|_| moved())?;
            parts_of.entry(message_id).or_default().push(data);
        }
    }
    let mut failed = 0usize;
    for (id, created, data) in rows {
        let Ok(data) = serde_json::from_str::<Value>(&data) else {
            failed += 1;
            continue;
        };
        let role = match data.get("role").and_then(Value::as_str) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };
        let mut message = Message {
            role,
            blocks: Vec::new(),
            created_ms: u64::try_from(created).unwrap_or(0),
            completed: data
                .pointer("/time/completed")
                .is_some_and(|v| !v.is_null()),
            // Only `tool-calls` marks a step the turn continues past; a
            // completed message with no `finish` (older writers, an abort)
            // ended its turn.
            final_step: data.get("finish").and_then(Value::as_str) != Some("tool-calls"),
            compaction: role == Role::Assistant
                && data.get("summary").and_then(Value::as_bool) == Some(true),
        };
        for raw in parts_of.remove(&id).unwrap_or_default() {
            let Ok(part) = serde_json::from_str::<Value>(&raw) else {
                failed += 1;
                continue;
            };
            map_part(&part, &mut message);
        }
        out.messages.push(message);
    }
    // The same drift rule as the JSONL kernel: a few unreadable rows are a
    // torn write, most of them is a format that moved.
    if failed > 0 && failed * 20 > out.messages.len() + failed {
        return Err(TranscriptError::FormatMoved {
            parsed: out.messages.len(),
            failed,
        });
    }
    Ok(out)
}

fn limit(n: usize) -> i64 {
    i64::try_from(n.max(1)).unwrap_or(i64::MAX)
}

fn row_triple(row: &rusqlite::Row<'_>) -> rusqlite::Result<(String, i64, String)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
}

fn map_part(part: &Value, message: &mut Message) {
    let text = |key: &str| part.get(key).and_then(Value::as_str).unwrap_or_default();
    match part.get("type").and_then(Value::as_str) {
        // `synthetic` text is opencode talking to the model on the user's
        // behalf (a file read inlined into the prompt), not something anyone
        // said.
        Some("text") if part.get("synthetic").and_then(Value::as_bool) != Some(true) => {
            message.blocks.push(Block::Text(cap(text("text"))));
        }
        Some("reasoning") => message.blocks.push(Block::Thinking),
        Some("tool") => {
            message.blocks.push(Block::ToolCall {
                name: text("tool").to_string(),
            });
            if let Some(output) = part.pointer("/state/output").and_then(Value::as_str) {
                message.blocks.push(Block::ToolResult {
                    preview: cap(output),
                });
            }
        }
        Some("compaction") => message.compaction = true,
        _ => {}
    }
}

/// The messages as the kernel's events: a compaction becomes the
/// [`TranscriptEvent::Compacted`] boundary before the message that carries it.
pub fn events(messages: &[Message]) -> Vec<TranscriptEvent> {
    let mut out = Vec::with_capacity(messages.len());
    for message in messages {
        if message.compaction {
            out.push(TranscriptEvent::Compacted);
        }
        out.push(TranscriptEvent::Message {
            role: message.role,
            blocks: message.blocks.clone(),
            at: Some(SystemTime::UNIX_EPOCH + Duration::from_millis(message.created_ms)),
        });
    }
    out
}

/// Whether the session's newest turn has finished: the newest message is an
/// assistant message opencode marked completed AND that ended the turn. A
/// user message last means a turn is pending; an uncompleted message, or a
/// completed `tool-calls` step, means one is running.
pub fn finished(messages: &[Message]) -> bool {
    messages.last().is_some_and(|message| {
        message.role == Role::Assistant && message.completed && message.final_step
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// opencode's own DDL, trimmed to the columns this reads.
    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, \
             time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, \
             session_id TEXT NOT NULL, time_created INTEGER NOT NULL, \
             time_updated INTEGER NOT NULL, data TEXT NOT NULL);",
        )
        .unwrap();
        conn
    }

    fn message(conn: &Connection, id: &str, at: i64, data: &str, parts: &[&str]) {
        conn.execute(
            "INSERT INTO message VALUES (?1, 'ses_a', ?2, ?2, ?3)",
            params![id, at, data],
        )
        .unwrap();
        for (i, part) in parts.iter().enumerate() {
            conn.execute(
                "INSERT INTO part VALUES (?1, ?2, 'ses_a', ?3, ?3, ?4)",
                params![format!("{id}-p{i}"), id, at + i as i64, part],
            )
            .unwrap();
        }
    }

    fn conversation() -> Connection {
        let conn = fixture();
        message(
            &conn,
            "m1",
            100,
            r#"{"role":"user"}"#,
            &[r#"{"type":"text","text":"fix the parser"}"#],
        );
        message(
            &conn,
            "m2",
            200,
            r#"{"role":"assistant","finish":"stop","time":{"created":200,"completed":290}}"#,
            &[
                r#"{"type":"step-start"}"#,
                r#"{"type":"reasoning","text":"private"}"#,
                r#"{"type":"tool","tool":"bash","state":{"status":"completed","output":"3 passed"}}"#,
                r#"{"type":"text","text":"Fixed.\nDONE: parser green"}"#,
                r#"{"type":"step-finish","reason":"stop"}"#,
            ],
        );
        message(
            &conn,
            "m3",
            300,
            r#"{"role":"user"}"#,
            &[r#"{"type":"text","text":"inlined file","synthetic":true}"#],
        );
        conn
    }

    #[test]
    fn maps_text_reasoning_and_tools_onto_the_kernel_blocks() {
        let read = read_session_in(&conversation(), "ses_a", Window::Last(10)).unwrap();
        assert_eq!((read.oldest_ms, read.newest_ms), (100, 300));
        assert_eq!(read.messages.len(), 3);
        assert_eq!(
            read.messages[1].blocks,
            vec![
                Block::Thinking,
                Block::ToolCall {
                    name: "bash".into()
                },
                Block::ToolResult {
                    preview: "3 passed".into()
                },
                Block::Text("Fixed.\nDONE: parser green".into()),
            ]
        );
        assert!(read.messages[1].completed);
        assert!(
            read.messages[2].blocks.is_empty(),
            "synthetic text is nobody's words"
        );
    }

    #[test]
    fn the_kernel_detail_levels_apply_unchanged() {
        use crate::agent_transcript::{turns_at_level, TranscriptDetail};
        let read = read_session_in(&conversation(), "ses_a", Window::Last(10)).unwrap();
        let events = events(&read.messages);
        let reply: Vec<String> = turns_at_level(&events, TranscriptDetail::Reply)
            .into_iter()
            .map(|(_, text, _)| text)
            .collect();
        assert_eq!(reply, vec!["fix the parser", "Fixed.\nDONE: parser green"]);
        let full = turns_at_level(&events, TranscriptDetail::Full);
        assert!(full[1].1.contains("\u{2699} bash") && full[1].1.contains("3 passed"));
    }

    #[test]
    fn windows_page_by_creation_time() {
        let conn = conversation();
        let last = read_session_in(&conn, "ses_a", Window::Last(1)).unwrap();
        assert_eq!(last.messages.len(), 1);
        assert_eq!(last.messages[0].created_ms, 300);
        let after = read_session_in(&conn, "ses_a", Window::After { ms: 100, n: 10 }).unwrap();
        assert_eq!(
            after
                .messages
                .iter()
                .map(|m| m.created_ms)
                .collect::<Vec<_>>(),
            vec![200, 300]
        );
        let other = read_session_in(&conn, "ses_other", Window::Last(10)).unwrap();
        assert!(other.messages.is_empty());
    }

    #[test]
    fn a_turn_is_finished_only_when_opencode_marked_it_completed() {
        let conn = fixture();
        message(
            &conn,
            "m1",
            100,
            r#"{"role":"user"}"#,
            &[r#"{"type":"text","text":"go"}"#],
        );
        message(
            &conn,
            "m2",
            200,
            r#"{"role":"assistant","time":{"created":200}}"#,
            &[r#"{"type":"text","text":"working"}"#],
        );
        let running = read_session_in(&conn, "ses_a", Window::Last(10)).unwrap();
        assert!(!finished(&running.messages), "still running");
        let read = read_session_in(&conversation(), "ses_a", Window::Last(10)).unwrap();
        assert!(
            !finished(&read.messages),
            "a user message is the newest: a turn is pending"
        );
        assert!(finished(&read.messages[..2]));
    }

    #[test]
    fn a_compaction_opens_an_epoch() {
        let conn = fixture();
        message(
            &conn,
            "m1",
            100,
            r#"{"role":"user"}"#,
            &[r#"{"type":"compaction","auto":true}"#],
        );
        message(
            &conn,
            "m2",
            200,
            r#"{"role":"assistant","summary":true,"finish":"stop","time":{"completed":250}}"#,
            &[r#"{"type":"text","text":"Summary of the work so far."}"#],
        );
        let read = read_session_in(&conn, "ses_a", Window::Last(10)).unwrap();
        let events = events(&read.messages);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, TranscriptEvent::Compacted))
                .count(),
            2,
            "each shape opencode uses for a compaction marks a boundary"
        );
    }

    #[test]
    fn a_completed_tool_calls_step_is_not_the_end_of_the_turn() {
        let conn = fixture();
        message(
            &conn,
            "m1",
            100,
            r#"{"role":"user"}"#,
            &[r#"{"type":"text","text":"go"}"#],
        );
        message(
            &conn,
            "m2",
            200,
            r#"{"role":"assistant","finish":"tool-calls","time":{"completed":250}}"#,
            &[r#"{"type":"tool","tool":"bash","state":{"output":"ok"}}"#],
        );
        let read = read_session_in(&conn, "ses_a", Window::Last(10)).unwrap();
        assert!(!finished(&read.messages), "a step, not the turn");
    }

    #[test]
    fn a_cursor_never_passes_a_message_still_being_written() {
        // Review finding: opencode inserts the message row when a step
        // starts and its parts as they stream. A cursor past it would never
        // deliver its text.
        let conn = conversation();
        message(
            &conn,
            "m4",
            400,
            r#"{"role":"assistant","time":{"created":400}}"#,
            &[],
        );
        let read = read_session_in(&conn, "ses_a", Window::After { ms: 0, n: 10 }).unwrap();
        assert_eq!(read.resume_after(), Some(399));
        let done =
            read_session_in(&conversation(), "ses_a", Window::After { ms: 0, n: 10 }).unwrap();
        assert_eq!(done.resume_after(), Some(300));
    }

    #[test]
    fn a_page_never_ends_inside_a_millisecond() {
        // Review finding: a user message and its reply can share a ms; a
        // page cut between them and resumed with `> ms` skipped the second.
        let conn = fixture();
        message(&conn, "a", 100, r#"{"role":"user"}"#, &[]);
        message(&conn, "b", 200, r#"{"role":"user"}"#, &[]);
        message(
            &conn,
            "c",
            200,
            r#"{"role":"assistant","finish":"stop","time":{"completed":201}}"#,
            &[],
        );
        let page = read_session_in(&conn, "ses_a", Window::After { ms: 0, n: 2 }).unwrap();
        assert_eq!(
            page.messages.len(),
            1,
            "the tie at 200 is left for the next page"
        );
        let next = read_session_in(
            &conn,
            "ses_a",
            Window::After {
                ms: page.resume_after().unwrap(),
                n: 2,
            },
        )
        .unwrap();
        assert_eq!(
            next.messages.len(),
            2,
            "both messages at 200 arrive together"
        );
    }

    #[test]
    fn mostly_unreadable_rows_are_a_moved_format_not_an_empty_session() {
        let conn = fixture();
        for i in 0..5 {
            message(&conn, &format!("m{i}"), i, "not json", &[]);
        }
        assert!(matches!(
            read_session_in(&conn, "ses_a", Window::Last(10)),
            Err(TranscriptError::FormatMoved { .. })
        ));
    }

    #[test]
    fn the_db_is_found_under_xdg_data_home_or_the_default() {
        let base = std::env::temp_dir().join(format!("flock-opencode-db-{}", std::process::id()));
        let default = base.join("home/.local/share/opencode");
        std::fs::create_dir_all(&default).unwrap();
        std::fs::write(default.join("opencode-stable.db"), b"").unwrap();
        assert_eq!(
            db_path(&base.join("home"), None),
            Some(default.join("opencode-stable.db"))
        );
        let xdg = base.join("xdg");
        std::fs::create_dir_all(xdg.join("opencode")).unwrap();
        std::fs::write(xdg.join("opencode/opencode.db"), b"").unwrap();
        assert_eq!(
            db_path(&base.join("home"), Some(&xdg)),
            Some(xdg.join("opencode/opencode.db"))
        );
        assert_eq!(db_path(&base.join("nowhere"), None), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The opencode twin of `real_transcripts_still_satisfy_the_kernel`:
    /// every session in this machine's real database must still read, and a
    /// finished one must yield a reply. Structure only — never prints content.
    #[test]
    #[ignore = "requires an opencode database on this machine"]
    fn the_real_opencode_database_still_reads() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        let xdg = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
        let Some(db) = db_path(&home, xdg.as_deref()) else {
            return;
        };
        let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let sessions: Vec<String> = conn
            .prepare("SELECT id FROM session")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let (mut read_ok, mut finished_with_reply, mut finished_total) = (0usize, 0usize, 0usize);
        for session in &sessions {
            let read = read_session(&db, session, Window::Last(64))
                .unwrap_or_else(|err| panic!("a real session did not read: {err:?}"));
            read_ok += 1;
            if finished(&read.messages) {
                finished_total += 1;
                if crate::agent_transcript::final_reply(&events(&read.messages)).is_some() {
                    finished_with_reply += 1;
                }
            }
        }
        assert!(read_ok > 0, "no sessions to check");
        // A finished turn can legitimately end on a tool call with no closing
        // text, so not every one has a reply — but most must.
        assert!(
            finished_with_reply * 2 >= finished_total,
            "{finished_with_reply}/{finished_total} finished sessions yielded a reply"
        );
    }

    #[test]
    fn a_missing_database_is_unreadable() {
        let missing = std::env::temp_dir().join("flock-opencode-missing/opencode-stable.db");
        assert!(matches!(
            read_session(&missing, "ses_a", Window::Last(1)),
            Err(TranscriptError::Unreadable)
        ));
    }
}
