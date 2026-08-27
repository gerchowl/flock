//! MCP → flock dispatch.
//!
//! Turns a parsed JSON-RPC request into either an MCP `result` payload or an
//! [`McpError`]. Pure w.r.t. IO — the caller (`super::mod::serve`) owns
//! stdin/stdout, and the socket edge is abstracted behind [`FlockCall`] so
//! unit tests can drive the router without a running server.

use serde_json::{json, Value};

use crate::api::client::{ApiClient, ApiClientError};
use crate::api::schema::{Method, Request};

use super::framing::McpError;
use super::{resources, tools};

/// The one dependency the router has on the outside world: send a flock
/// [`Method`] and get back the raw response value (success OR error
/// envelope). The trait exists so [`route`] is unit-testable without a socket.
pub(super) trait FlockCall {
    fn call(&self, method: Method) -> Result<Value, ApiClientError>;
}

/// Production wire: talks to the local flock socket via [`ApiClient`].
pub(super) struct LocalApi;

impl FlockCall for LocalApi {
    fn call(&self, method: Method) -> Result<Value, ApiClientError> {
        let request = Request {
            id: format!("mcp:{}", crate::mcp::next_call_seq()),
            method,
        };
        ApiClient::local().request_value(&request)
    }
}

/// Route one parsed MCP method. Notification arm-through is the caller's
/// job — we return whatever this method would produce as a `result` and let
/// the loop discard it if the client didn't want a response.
pub(super) fn route<F: FlockCall>(
    method: &str,
    params: Value,
    flock: &F,
) -> Result<Value, McpError> {
    match method {
        "initialize" => Ok(initialize_result()),
        // Notifications the MCP spec mandates a client sends after
        // `initialize`. We accept them silently (and their result is
        // discarded by the loop anyway — never sent on the wire).
        "notifications/initialized" | "notifications/cancelled" => Ok(Value::Null),
        "tools/list" => Ok(tools_list_result()),
        "tools/call" => tools_call(params, flock),
        // #286 / ADR-0017: files a human handed to an agent on this server.
        // A different MCP primitive from tools, for content the server
        // already holds and can name.
        "resources/list" => resources_list(flock),
        "resources/read" => resources_read(params, flock),
        // Templates are how a server advertises PARAMETERISED uris. flock's
        // resources are minted, never constructed by the client, so the
        // honest answer is an empty list — and a client that asks deserves
        // that rather than a method-not-found it has to special-case.
        "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
        // `ping` is a spec-level keepalive some clients send; respond with an
        // empty object per the MCP spec.
        "ping" => Ok(json!({})),
        _ => Err(McpError::method_not_found(method)),
    }
}

fn initialize_result() -> Value {
    // The `2024-11-05` protocol version is what the Anthropic MCP spec
    // documents for the capability set we advertise. Bumping this is a wire
    // change — update the golden test alongside.
    json!({
        "protocolVersion": "2024-11-05",
        // `resources` advertises the handed-over-file surface (#286). No
        // `subscribe`/`listChanged` sub-capability: flock does not push
        // resource notifications, and claiming otherwise would make a client
        // wait for an update that never comes.
        "capabilities": { "tools": {}, "resources": {} },
        "serverInfo": {
            "name": "flock",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

fn tools_list_result() -> Value {
    let list: Vec<Value> = tools::table().iter().map(tools::Tool::descriptor).collect();
    json!({ "tools": list })
}

fn resources_list<F: FlockCall>(flock: &F) -> Result<Value, McpError> {
    let method = Method::HandoffList(crate::api::schema::HandoffListParams::default());
    Ok(resources::list_result(&call_flock(method, flock)?))
}

fn resources_read<F: FlockCall>(params: Value, flock: &F) -> Result<Value, McpError> {
    let file_id = resources::file_id_from_params(&params)?;
    let method = Method::HandoffRead(crate::api::schema::HandoffReadParams { file_id });
    Ok(resources::read_result(&call_flock(method, flock)?))
}

/// Send one flock method and unwrap its envelope. Shared by the tool and
/// resource paths so a flock refusal translates identically on both.
fn call_flock<F: FlockCall>(method: Method, flock: &F) -> Result<Value, McpError> {
    let raw = flock.call(method).map_err(McpError::transport)?;
    extract_flock_result(raw)
}

fn tools_call<F: FlockCall>(params: Value, flock: &F) -> Result<Value, McpError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| McpError::invalid_params("tools/call missing `name`"))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

    let Some(tool) = tools::find(name) else {
        return Err(McpError::not_exposed(name));
    };

    let method = (tool.build)(arguments)?;
    let flock_result = call_flock(method, flock)?;

    // MCP's tool-call result shape: `content` is an array of blocks. We JSON-
    // encode the flock result and hand it back as a single text block so the
    // agent can `json.loads` it without another round-trip.
    let text = serde_json::to_string(&flock_result).map_err(|e| McpError {
        code: -32000,
        message: format!("failed to serialize flock result: {e}"),
        data: Some(json!({ "refusal": "serialize_error" })),
    })?;
    Ok(json!({
        "content": [ { "type": "text", "text": text } ]
    }))
}

/// Unwrap the raw flock socket response — an already-parsed JSON envelope
/// with either a `result` field (success) or an `error: {code, message}`
/// field (failure). The failure branch translates to an MCP refusal so the
/// flock error code lands in `data.refusal`.
fn extract_flock_result(mut raw: Value) -> Result<Value, McpError> {
    if let Some(error) = raw.get("error") {
        let code = error.get("code").and_then(Value::as_str).unwrap_or("error");
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("flock refused the request");
        return Err(McpError::from_flock_error(code, message));
    }
    // Take() rather than clone() — the raw envelope isn't needed again.
    let result = raw
        .get_mut("result")
        .map(Value::take)
        .unwrap_or(Value::Null);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// Records every call and replays a scripted response — the whole point
    /// of the [`FlockCall`] trait.
    struct MockApi {
        response: Value,
        calls: RefCell<Vec<Method>>,
    }

    impl MockApi {
        fn ok(response: Value) -> Self {
            Self {
                response,
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl FlockCall for MockApi {
        fn call(&self, method: Method) -> Result<Value, ApiClientError> {
            self.calls.borrow_mut().push(method);
            Ok(self.response.clone())
        }
    }

    #[test]
    fn initialize_returns_server_info() {
        let flock = MockApi::ok(json!({}));
        let result = route("initialize", json!({}), &flock).unwrap();
        assert_eq!(result["protocolVersion"], "2024-11-05");
        assert_eq!(result["capabilities"]["tools"], json!({}));
        assert_eq!(result["capabilities"]["resources"], json!({}));
        assert_eq!(result["serverInfo"]["name"], "flock");
        assert_eq!(result["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
        assert!(flock.calls.borrow().is_empty(), "initialize is client-only");
    }

    #[test]
    fn notifications_initialized_produces_no_flock_call() {
        let flock = MockApi::ok(json!({}));
        route("notifications/initialized", json!({}), &flock).unwrap();
        assert!(flock.calls.borrow().is_empty());
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let flock = MockApi::ok(json!({}));
        // `prompts/list` is a real MCP method flock does not implement —
        // `resources/list` stopped being one in #286.
        let err = route("prompts/list", json!({}), &flock).unwrap_err();
        assert_eq!(err.code, -32601);
    }

    #[test]
    fn tools_list_matches_the_table() {
        let flock = MockApi::ok(json!({}));
        let result = route("tools/list", json!({}), &flock).unwrap();
        let names: Vec<&str> = result["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            tools::table().iter().map(|t| t.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn tools_call_wraps_flock_result_as_text_block() {
        // Mock the raw wire success envelope the socket would produce.
        let flock = MockApi::ok(json!({
            "id": "mcp:1",
            "result": { "type": "agent_list", "agents": [] }
        }));
        let result = route(
            "tools/call",
            json!({ "name": "flock_agent_list", "arguments": {} }),
            &flock,
        )
        .unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        let inner: Value = serde_json::from_str(text).unwrap();
        assert_eq!(inner["type"], "agent_list");
        assert_eq!(flock.calls.borrow().len(), 1);
        assert!(matches!(flock.calls.borrow()[0], Method::AgentList(_)));
    }

    #[test]
    fn tools_call_unknown_name_refuses_not_exposed() {
        // pane.close-shaped name — the design's canary for the hidden verbs.
        let flock = MockApi::ok(json!({}));
        let err = route(
            "tools/call",
            json!({ "name": "flock_pane_close", "arguments": { "pane_id": "p1" } }),
            &flock,
        )
        .unwrap_err();
        assert_eq!(err.code, -32000);
        assert_eq!(
            err.data.as_ref().and_then(|d| d.get("refusal")),
            Some(&json!("not_exposed_via_mcp"))
        );
        assert!(flock.calls.borrow().is_empty(), "refusal short-circuits");
    }

    #[test]
    fn tools_call_translates_flock_error_envelope() {
        // Mock the raw wire ERROR envelope: the server would emit this for a
        // fork of a non-Claude agent.
        let flock = MockApi::ok(json!({
            "id": "mcp:1",
            "error": {
                "code": "unsupported_for_agent",
                "message": "agent.fork only supports Claude sessions"
            }
        }));
        let err = route(
            "tools/call",
            json!({ "name": "flock_agent_fork", "arguments": { "target": "codex" } }),
            &flock,
        )
        .unwrap_err();
        assert_eq!(err.code, -32000);
        assert_eq!(
            err.data.as_ref().and_then(|d| d.get("refusal")),
            Some(&json!("unsupported_for_agent"))
        );
        assert!(err.message.contains("only supports Claude"));
    }

    #[test]
    fn resources_list_maps_handed_over_files_onto_the_resource_shape() {
        let flock = MockApi::ok(json!({
            "id": "mcp:1",
            "result": {
                "type": "handoff_list",
                "files": [{
                    "file_id": "file:abc:0",
                    "name": "spec.pdf",
                    "mime": "application/pdf",
                    "bytes": 9,
                    "path": "/tmp/staged/spec.pdf",
                    "pane_id": "ws_1:p1",
                    "agent_id": "agent_host_abc",
                    "origin_host": "host",
                    "received_at_ms": 1
                }],
                "total": 1
            }
        }));
        let result = route("resources/list", json!({}), &flock).unwrap();
        assert_eq!(result["resources"][0]["uri"], "flock://handoff/file:abc:0");
        assert_eq!(result["resources"][0]["name"], "spec.pdf");
        assert!(matches!(flock.calls.borrow()[0], Method::HandoffList(_)));
    }

    #[test]
    fn resources_read_addresses_a_file_id_never_a_path() {
        // The uri is the only way in, and it carries a minted id — there is
        // no parameter anywhere on this path that names a filesystem path.
        let flock = MockApi::ok(json!({
            "id": "mcp:1",
            "result": {
                "type": "handoff_read",
                "file_id": "file:abc:0",
                "name": "notes.md",
                "mime": "text/markdown",
                "bytes": 7,
                "path": "/tmp/staged/notes.md",
                "encoding": "utf8",
                "content": "# hello"
            }
        }));
        let result = route(
            "resources/read",
            json!({ "uri": "flock://handoff/file:abc:0" }),
            &flock,
        )
        .unwrap();
        assert_eq!(result["contents"][0]["text"], "# hello");
        assert_eq!(
            flock.calls.borrow()[0],
            Method::HandoffRead(crate::api::schema::HandoffReadParams {
                file_id: "file:abc:0".into()
            })
        );
    }

    #[test]
    fn resources_read_of_an_unknown_file_propagates_the_flock_refusal() {
        let flock = MockApi::ok(json!({
            "id": "mcp:1",
            "error": { "code": "handoff_not_found", "message": "no handed-over file with id x" }
        }));
        let err = route(
            "resources/read",
            json!({ "uri": "flock://handoff/x" }),
            &flock,
        )
        .unwrap_err();
        assert_eq!(err.code, -32000);
        assert_eq!(
            err.data.as_ref().and_then(|d| d.get("refusal")),
            Some(&json!("handoff_not_found"))
        );
    }

    #[test]
    fn resources_read_of_a_foreign_uri_never_reaches_the_server() {
        let flock = MockApi::ok(json!({}));
        let err = route(
            "resources/read",
            json!({ "uri": "file:///etc/passwd" }),
            &flock,
        )
        .unwrap_err();
        assert_eq!(err.code, -32602);
        assert!(flock.calls.borrow().is_empty(), "refusal short-circuits");
    }

    #[test]
    fn tools_call_missing_arguments_is_invalid_params() {
        let flock = MockApi::ok(json!({}));
        let err = route(
            "tools/call",
            json!({ "name": "flock_agent_get", "arguments": {} }),
            &flock,
        )
        .unwrap_err();
        assert_eq!(err.code, -32602);
        assert!(flock.calls.borrow().is_empty());
    }
}
