//! Model Context Protocol stdio server implementation.
//!
//! Bridges MCP `tools/call` invocations to flock's local socket API. Speaks
//! JSON-RPC 2.0 over newline-delimited stdin/stdout (NOT LSP framing) so it
//! drops into every MCP client (Claude Desktop, `mcp` CLI, agent runners).
//! CLI dispatch lives in [`crate::cli::mcp`]; this module owns the wire
//! implementation.
//!
//! Structure:
//!   - [`framing`] — JSON-RPC decode + error envelopes
//!   - [`tools`]   — the closed tool table (names, schemas, method builders)
//!   - [`resources`] — the resource surface: handed-over files (#286)
//!   - [`bridge`]  — pure dispatcher from parsed method → MCP result
//!   - [`channel`] — the opt-in channel push of arriving mail (#438)
//!   - this file  — the blocking read/write loop
//!
//! The loop mirrors [`crate::cli::hook`]'s posture: a blocking `BufReader` on
//! stdin, no tokio. Newline-delimited JSON is the whole framing protocol; EOF
//! on stdin means the client hung up and we exit 0 cleanly. It logs nothing —
//! except with channel push on (#438), when a long-lived feed thread runs
//! and its failures would otherwise be invisible, so the process then writes
//! `flock-mcp.log` the way the relay writes its own file.

use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

mod bridge;
mod channel;
mod framing;
mod resources;
mod tools;

use bridge::{FlockCall, LocalApi};
use channel::{ChannelOptions, SharedOut};
use framing::{error_response, parse_message, success_response};

/// Run the MCP stdio server on this process's stdin/stdout. Returns when
/// stdin reaches EOF or an IO error interrupts the loop.
pub(crate) fn serve_over_stdio() -> std::io::Result<i32> {
    let stdin = std::io::stdin();
    let reader = BufReader::new(stdin.lock());
    // Shared, not locked for the process lifetime: with channel push on, a
    // second thread writes notifications between responses.
    let out: SharedOut = Arc::new(Mutex::new(std::io::stdout()));
    let channel = ChannelOptions::from_config(&crate::config::Config::load().config.msg);
    if channel.push {
        crate::logging::init_file_logging("flock-mcp.log");
    }
    let feed_out = out.clone();
    let feed_opts = channel.clone();
    serve_loop(reader, &out, &LocalApi, &channel, move || {
        channel::spawn_push_feed(feed_out, feed_opts);
    })
}

/// The serve loop, parameterised over its IO and the flock transport so
/// tests can drive it end-to-end without stdio or a socket. `start_feed`
/// runs at most once, when the client finishes initializing and channel push
/// is on.
fn serve_loop<R: BufRead, F: FlockCall>(
    mut reader: R,
    out: &SharedOut,
    flock: &F,
    channel: &ChannelOptions,
    start_feed: impl FnOnce(),
) -> std::io::Result<i32> {
    let mut start_feed = Some(start_feed);
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            // EOF: the MCP client (or its parent) closed the pipe. Exit
            // cleanly so a supervisor never treats the disconnect as an error.
            return Ok(0);
        }
        // Blank/whitespace-only lines are ignored (some clients pretty-print
        // with trailing newlines). A malformed line is different — it needs a
        // parse-error response with id null.
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_line(&line, flock, channel) {
            channel::emit(out, &response)?;
        }
        // A push before the client has initialized has no listener to reach,
        // so the feed attaches on `notifications/initialized`, once.
        if channel.push && is_initialized_notification(&line) {
            if let Some(start) = start_feed.take() {
                start();
            }
        }
    }
}

fn is_initialized_notification(line: &str) -> bool {
    parse_message(line).is_ok_and(|m| m.id.is_none() && m.method == "notifications/initialized")
}

/// Process one line from stdin. Returns `None` when the caller must NOT emit
/// a response (notifications, per JSON-RPC 2.0). Malformed input returns a
/// parse-error response with id `null` — the spec's fallback when we can't
/// recover an id from the client's payload.
fn handle_line<F: FlockCall>(line: &str, flock: &F, channel: &ChannelOptions) -> Option<Value> {
    let parsed = match parse_message(line) {
        Ok(parsed) => parsed,
        Err(err) => {
            // A parse error must always be reported (id null) so the client
            // knows its message was rejected. An invalid-request that
            // *happens* to be missing an id is a coin flip in the spec —
            // reporting is friendlier than silent drop.
            return Some(error_response(Value::Null, err));
        }
    };

    let is_notification = parsed.id.is_none();
    let outcome = bridge::route(&parsed.method, parsed.params, flock, channel);

    if is_notification {
        // Per spec: no response for notifications, even on error. The bridge
        // still ran (side-effect free for the notification methods we accept)
        // so any downstream logging can happen there.
        return None;
    }

    let id = parsed.id.unwrap_or(Value::Null);
    match outcome {
        Ok(result) => Some(success_response(id, result)),
        Err(err) => Some(error_response(id, err)),
    }
}

/// Monotonic per-process id for the flock-side [`Request::id`] the bridge
/// mints. Bumped on every call so overlapping in-flight requests never share
/// an id even under a burst of tool calls.
pub(super) fn next_call_seq() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos.wrapping_add(COUNTER.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::json;

    use super::*;
    use crate::api::client::ApiClientError;
    use crate::api::schema::Method;

    /// In-memory mock — same shape as `bridge::tests::MockApi` but usable
    /// from the loop-level test.
    struct MockApi {
        response: Value,
        calls: RefCell<Vec<Method>>,
    }

    impl FlockCall for MockApi {
        fn call(&self, method: Method) -> Result<Value, ApiClientError> {
            self.calls.borrow_mut().push(method);
            Ok(self.response.clone())
        }
    }

    fn drive(input: &str, response: Value) -> Vec<Value> {
        drive_with(input, response, &ChannelOptions::off(), || {})
    }

    fn drive_with(
        input: &str,
        response: Value,
        channel: &ChannelOptions,
        start_feed: impl FnOnce(),
    ) -> Vec<Value> {
        let flock = MockApi {
            response,
            calls: RefCell::new(Vec::new()),
        };
        let sink = Arc::new(Mutex::new(Vec::<u8>::new()));
        let out: SharedOut = sink.clone();
        let reader = std::io::BufReader::new(input.as_bytes());
        serve_loop(reader, &out, &flock, channel, start_feed).unwrap();
        // Split newline-framed JSON back into values.
        let text = String::from_utf8(sink.lock().unwrap().clone()).unwrap();
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .collect()
    }

    #[test]
    fn initialize_handshake_returns_server_info() {
        let out = drive(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            json!({}),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], 1);
        assert_eq!(out[0]["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(out[0]["result"]["serverInfo"]["name"], "flock");
    }

    #[test]
    fn notification_produces_no_response() {
        // notifications/initialized has no id — the loop must NOT emit a line.
        let out = drive(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            json!({}),
        );
        assert!(out.is_empty(), "notifications must not receive responses");
    }

    #[test]
    fn malformed_line_yields_parse_error_with_null_id() {
        let out = drive("{not valid json", json!({}));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], Value::Null);
        assert_eq!(out[0]["error"]["code"], -32700);
    }

    #[test]
    fn unknown_tool_refuses_not_exposed_via_mcp() {
        let input = r#"{"jsonrpc":"2.0","id":42,"method":"tools/call","params":{"name":"flock_pane_close","arguments":{"pane_id":"p1"}}}"#;
        let out = drive(input, json!({}));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], 42);
        assert_eq!(out[0]["error"]["code"], -32000);
        assert_eq!(out[0]["error"]["data"]["refusal"], "not_exposed_via_mcp");
    }

    #[test]
    fn blank_lines_are_skipped() {
        // Two blank lines then a request — should still get one response.
        let input =
            "\n   \n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{}}\n";
        let out = drive(input, json!({}));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], 1);
        assert!(out[0]["result"]["tools"].is_array());
    }

    #[test]
    fn eof_exits_cleanly() {
        // No input at all — the loop returns Ok(0) and emits nothing.
        let out = drive("", json!({}));
        assert!(out.is_empty());
    }

    #[test]
    fn the_push_feed_starts_once_on_initialized_and_only_with_the_flag() {
        let handshake = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
        );
        let on = ChannelOptions {
            push: true,
            ..ChannelOptions::off()
        };
        let started = std::cell::Cell::new(0);
        let out = drive_with(handshake, json!({}), &on, || started.set(started.get() + 1));
        assert_eq!(started.get(), 1, "one feed per session");
        assert_eq!(
            out[0]["result"]["capabilities"]["experimental"]["claude/channel"],
            json!({})
        );

        let started = std::cell::Cell::new(0);
        drive_with(handshake, json!({}), &ChannelOptions::off(), || {
            started.set(1)
        });
        assert_eq!(
            started.get(),
            0,
            "flag off: no feed, no socket subscription"
        );
    }

    #[test]
    fn next_call_seq_is_strictly_increasing() {
        assert!(next_call_seq() < next_call_seq());
    }
}
