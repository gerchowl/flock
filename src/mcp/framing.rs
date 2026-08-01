//! JSON-RPC 2.0 framing types for the MCP stdio server.
//!
//! MCP speaks newline-delimited JSON — one JSON object per line on stdin/
//! stdout (NOT LSP Content-Length framing). Requests carry an `id`;
//! notifications don't (and never get a response, per the JSON-RPC 2.0 spec,
//! even on error). This module keeps the wire vocabulary in one place so
//! [`super::bridge`] can be a pure request → response function.

use serde_json::{json, Value};

/// The JSON-RPC error object attached to a response.
///
/// Reserved codes we produce: `-32700` (parse error), `-32600` (invalid
/// request), `-32601` (method not found), `-32602` (invalid params), and
/// `-32000` (implementation-defined server error — the bucket the design
/// dedicates to flock refusals, tagged with `data.refusal = <flock code>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct McpError {
    pub code: i32,
    pub message: String,
    pub data: Option<Value>,
}

impl McpError {
    pub(super) fn parse_error() -> Self {
        Self {
            code: -32700,
            message: "Parse error".into(),
            data: None,
        }
    }

    pub(super) fn invalid_request(reason: impl Into<String>) -> Self {
        Self {
            code: -32600,
            message: reason.into(),
            data: None,
        }
    }

    pub(super) fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("Method not found: {method}"),
            data: None,
        }
    }

    pub(super) fn invalid_params(reason: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: reason.into(),
            data: None,
        }
    }

    /// Anything not in the closed MCP tool table — including the deliberately
    /// hidden verbs (`pane.close`, `worktree.remove`/`create`, `server.*`,
    /// `integration.*`, `agent.start`/`rename`/`focus`, pane `send_*`,
    /// `notification.*`) — refuses uniformly with this code.
    pub(super) fn not_exposed(name: &str) -> Self {
        Self {
            code: -32000,
            message: format!("tool `{name}` is not exposed via MCP"),
            data: Some(json!({ "refusal": "not_exposed_via_mcp" })),
        }
    }

    /// Translate a flock `ErrorBody { code, message }` into an MCP refusal:
    /// the flock code lands in `data.refusal` so an agent can branch on it
    /// (`unsupported_for_agent`, `no_agent_session`, …).
    pub(super) fn from_flock_error(code: &str, message: &str) -> Self {
        Self {
            code: -32000,
            message: message.into(),
            data: Some(json!({ "refusal": code })),
        }
    }

    /// Transport-level failure talking to the local socket — the flock server
    /// is down, the socket path is wrong, the connection dropped. Distinct
    /// refusal tag so it's not confused with a modelled server refusal.
    pub(super) fn transport(err: impl std::fmt::Display) -> Self {
        Self {
            code: -32000,
            message: err.to_string(),
            data: Some(json!({ "refusal": "transport_error" })),
        }
    }
}

/// A decoded request or notification from stdin. Notifications have `id =
/// None`; requests carry the client's id (any JSON type — usually a string or
/// number — echoed back in the response).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ParsedMessage {
    pub id: Option<Value>,
    pub method: String,
    pub params: Value,
}

/// Parse a single newline-delimited JSON-RPC message.
///
/// Returns `Err(_)` on any framing violation: non-JSON, non-object, missing
/// `method`, or wrong types. The caller responds with a parse-error (id
/// `null`) for the JSON case and an invalid-request otherwise.
pub(super) fn parse_message(line: &str) -> Result<ParsedMessage, McpError> {
    let value: Value = serde_json::from_str(line.trim()).map_err(|_| McpError::parse_error())?;
    if !value.is_object() {
        return Err(McpError::invalid_request("request must be a JSON object"));
    }
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| McpError::invalid_request("missing `method`"))?
        .to_string();
    // Missing `id` means notification. An explicit `id: null` is unusual but
    // accepted per the spec's "id … MAY be Null" clause; we still treat it as
    // a notification (nothing meaningful to echo, and no client will read a
    // response for a null id).
    let id = match value.get("id") {
        None => None,
        Some(id) if id.is_null() => None,
        Some(id) => Some(id.clone()),
    };
    let params = value
        .get("params")
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    Ok(ParsedMessage { id, method, params })
}

/// Build the success response envelope for a given id + result payload.
pub(super) fn success_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Build the error response envelope. `data` is omitted when unset, matching
/// the JSON-RPC 2.0 wire shape.
pub(super) fn error_response(id: Value, err: McpError) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("code".into(), json!(err.code));
    object.insert("message".into(), json!(err.message));
    if let Some(data) = err.data {
        object.insert("data".into(), data);
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": Value::Object(object) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_notification_has_no_id() {
        let m = parse_message(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();
        assert_eq!(m.id, None);
        assert_eq!(m.method, "notifications/initialized");
    }

    #[test]
    fn parse_request_captures_id() {
        let m =
            parse_message(r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{}}"#).unwrap();
        assert_eq!(m.id, Some(json!(7)));
        assert_eq!(m.method, "initialize");
    }

    #[test]
    fn explicit_null_id_is_treated_as_notification() {
        let m = parse_message(r#"{"jsonrpc":"2.0","id":null,"method":"x"}"#).unwrap();
        assert_eq!(m.id, None);
    }

    #[test]
    fn missing_method_is_invalid_request() {
        let err = parse_message(r#"{"jsonrpc":"2.0","id":1}"#).unwrap_err();
        assert_eq!(err.code, -32600);
    }

    #[test]
    fn non_object_is_invalid_request() {
        // Valid JSON but not an object — batch requests are out of scope for
        // this implementation, and we don't accept top-level arrays/scalars.
        let err = parse_message(r#"[1,2,3]"#).unwrap_err();
        assert_eq!(err.code, -32600);
    }

    #[test]
    fn malformed_json_is_parse_error() {
        let err = parse_message("{not json").unwrap_err();
        assert_eq!(err.code, -32700);
    }

    #[test]
    fn error_response_omits_absent_data() {
        let value = error_response(json!(1), McpError::method_not_found("x"));
        assert!(value["error"].get("data").is_none());
        assert_eq!(value["error"]["code"], -32601);
    }

    #[test]
    fn error_response_includes_refusal_data() {
        let value = error_response(json!(1), McpError::not_exposed("pane.close"));
        assert_eq!(value["error"]["data"]["refusal"], "not_exposed_via_mcp");
    }

    #[test]
    fn success_response_shape() {
        let value = success_response(json!("abc"), json!({"ok": true}));
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], "abc");
        assert_eq!(value["result"]["ok"], true);
    }
}
