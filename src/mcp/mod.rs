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
//!   - this file  — the blocking read/write loop
//!
//! The loop mirrors [`crate::cli::hook`]'s posture: a blocking `BufReader` on
//! stdin, no tokio, no logging. Newline-delimited JSON is the whole framing
//! protocol; EOF on stdin means the client hung up and we exit 0 cleanly.

use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

mod bridge;
mod framing;
mod resources;
mod tools;

use bridge::{FlockCall, LocalApi};
use framing::{error_response, parse_message, success_response};

/// Run the MCP stdio server on this process's stdin/stdout. Returns when
/// stdin reaches EOF or an IO error interrupts the loop.
pub(crate) fn serve_over_stdio() -> std::io::Result<i32> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let reader = BufReader::new(stdin.lock());
    let writer = stdout.lock();
    serve_loop(reader, writer, &LocalApi)
}

/// The serve loop, parameterised over its IO and the flock transport so
/// tests can drive it end-to-end without stdio or a socket.
fn serve_loop<R: BufRead, W: Write, F: FlockCall>(
    mut reader: R,
    mut writer: W,
    flock: &F,
) -> std::io::Result<i32> {
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
        if let Some(response) = handle_line(&line, flock) {
            write_json_line(&mut writer, &response)?;
        }
    }
}

fn write_json_line<W: Write>(writer: &mut W, value: &Value) -> std::io::Result<()> {
    let mut buf = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    buf.push(b'\n');
    writer.write_all(&buf)?;
    writer.flush()
}

/// Process one line from stdin. Returns `None` when the caller must NOT emit
/// a response (notifications, per JSON-RPC 2.0). Malformed input returns a
/// parse-error response with id `null` — the spec's fallback when we can't
/// recover an id from the client's payload.
fn handle_line<F: FlockCall>(line: &str, flock: &F) -> Option<Value> {
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
    let outcome = bridge::route(&parsed.method, parsed.params, flock);

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
        let flock = MockApi {
            response,
            calls: RefCell::new(Vec::new()),
        };
        let mut output: Vec<u8> = Vec::new();
        let reader = std::io::BufReader::new(input.as_bytes());
        serve_loop(reader, &mut output, &flock).unwrap();
        // Split newline-framed JSON back into values.
        let text = String::from_utf8(output).unwrap();
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
    fn next_call_seq_is_strictly_increasing() {
        assert!(next_call_seq() < next_call_seq());
    }
}
