//! MCP → flock dispatch.
//!
//! Turns a parsed JSON-RPC request into either an MCP `result` payload or an
//! [`McpError`]. Pure w.r.t. IO — the caller (`super::mod::serve`) owns
//! stdin/stdout, and the socket edge is abstracted behind [`FlockCall`] so
//! unit tests can drive the router without a running server.

use serde_json::{json, Value};

use crate::api::client::{ApiClient, ApiClientError};
use crate::api::schema::{Method, Request};

use super::channel::{self, ChannelOptions};
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
    channel: &ChannelOptions,
) -> Result<Value, McpError> {
    match method {
        "initialize" => Ok(initialize_result(channel)),
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

fn initialize_result(channel: &ChannelOptions) -> Value {
    // The `2024-11-05` protocol version is what the Anthropic MCP spec
    // documents for the capability set we advertise. Bumping this is a wire
    // change — update the golden test alongside.
    //
    // `resources` advertises the handed-over-file surface (#286). No
    // `subscribe`/`listChanged` sub-capability: flock does not push
    // resource notifications, and claiming otherwise would make a client
    // wait for an update that never comes.
    let mut result = json!({
        "protocolVersion": "2024-11-05",
        "capabilities": { "tools": {}, "resources": {} },
        "serverInfo": {
            "name": "flock",
            "version": env!("CARGO_PKG_VERSION"),
        },
    });
    // #438: the channel capability is declared ONLY with `[msg]
    // channel_push` on. Its presence is what makes Claude Code register a
    // listener, and a server that declares it without pushing would leave a
    // listener waiting on nothing.
    if channel.push {
        result["capabilities"]["experimental"] = json!({ channel::CAPABILITY: {} });
        result["instructions"] = json!(channel::INSTRUCTIONS);
    }
    result
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
    // The verbs the CLI reports through `print_send_response`: their `later`
    // refusals exit `REFUSED` there, so `data.exit` says 10 here too.
    let send_like = matches!(
        method,
        Method::PaneSubmit(_) | Method::MsgSend(_) | Method::MsgReply(_) | Method::AgentRestart(_)
    );
    let message_send = matches!(method, Method::MsgSend(_));
    let raw = flock.call(method).map_err(McpError::transport)?;
    let target_data = message_send
        .then(|| raw.pointer("/error/data").cloned())
        .flatten();
    extract_flock_result(raw, send_like).map_err(|mut error| {
        if let (Some(data), Some(detail)) = (
            error.data.as_mut().and_then(Value::as_object_mut),
            target_data.and_then(|v| v.as_object().cloned()),
        ) {
            for key in ["kind", "expected", "candidates"] {
                if let Some(value) = detail.get(key) {
                    data.insert(key.into(), value.clone());
                }
            }
        }
        error
    })
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

    let method = (tool.build)(arguments.clone())?;
    // #1179 / #868: an older server would drop `questions` without a word and
    // start the child with no policy. An agent cannot read a stderr note, so
    // the call is refused before anything starts, and says why.
    let asks_questions = matches!(
        &method,
        Method::AgentSpawn(crate::api::schema::AgentSpawnParams {
            questions: Some(_),
            ..
        }) | Method::AgentFork(crate::api::schema::AgentForkParams {
            questions: Some(_),
            ..
        })
    );
    if asks_questions && !server_takes_questions(flock) {
        return Err(McpError::from_flock_error(
            crate::api::compatibility::ERROR_CODE,
            "the server does not record a question policy (it predates #1179): hand it off \
             with `flk server live-handoff`, or call again without `questions`",
            crate::api::compatibility::EXIT_CODE,
        ));
    }
    let pane = match &method {
        Method::PaneArmSelfCompact(params) if params.instructions.is_some() => {
            Some(params.pane.clone())
        }
        _ => None,
    };
    let flock_result = call_flock(method, flock).map_err(|mut error| {
        if name == "flock_msg_send" {
            if let Some(data) = error.data.as_mut() {
                // Branch on stable socket codes, never diagnostic wording.
                use crate::api::error_code::ErrorCode;
                if matches!(
                    data["refusal"].as_str().and_then(ErrorCode::from_code),
                    Some(
                        ErrorCode::MsgTargetNotFound
                            | ErrorCode::MsgTargetAmbiguous
                            | ErrorCode::AgentTargetAmbiguous
                    )
                ) {
                    data["kind"] = data["refusal"].clone();
                    if data.get("expected").is_none() {
                        data["expected"] = crate::api::schema::MessageTarget::expected_shapes();
                        error.message.push_str(&format!(
                            "; expected {}",
                            crate::api::schema::MESSAGE_TARGET_EXPECTED
                        ));
                    }
                }
            }
        }
        error
    })?;
    let flock_result = tools::annotate(name, &arguments, flock_result);
    // #854: an older server ignores `instructions` and arms a plain `/compact`.
    // Undo that arming rather than report a success that lost them.
    if let Some(pane) = pane {
        if crate::agent_self_compact::instructions_dropped(true, &flock_result) {
            // A failed abort must not replace the explanation with a bare
            // transport error: the agent still needs to know why, and that
            // the plain arming is still there to drop.
            let message =
                match call_flock(crate::agent_self_compact::abort_dropped_arming(pane), flock) {
                    Ok(_) => crate::agent_self_compact::INSTRUCTIONS_DROPPED.to_string(),
                    Err(err) => format!(
                        "{} ({})",
                        crate::agent_self_compact::INSTRUCTIONS_DROPPED_ABORT_FAILED,
                        err.message
                    ),
                };
            return Err(McpError::from_flock_error(
                crate::api::compatibility::ERROR_CODE,
                &message,
                crate::api::compatibility::EXIT_CODE,
            ));
        }
    }

    // MCP's tool-call result shape: `content` is an array of blocks. We JSON-
    // encode the flock result and hand it back as a single text block so the
    // agent can `json.loads` it without another round-trip.
    let text = serde_json::to_string(&flock_result).map_err(|e| McpError {
        code: -32000,
        message: format!("failed to serialize flock result: {e}"),
        data: Some(super::framing::refusal(
            "serialize_error",
            crate::cli::exit::FAILURE,
        )),
    })?;
    Ok(json!({
        "content": [ { "type": "text", "text": text } ]
    }))
}

/// Does the server advertise `agent_questions`? A ping that fails reads as no.
fn server_takes_questions<F: FlockCall>(flock: &F) -> bool {
    flock
        .call(Method::Ping(crate::api::schema::PingParams::default()))
        .is_ok_and(|pong| crate::cli::questions::advertises_questions(&pong))
}

/// Unwrap the raw flock socket response — an already-parsed JSON envelope
/// with either a `result` field (success) or an `error: {code, message}`
/// field (failure). The failure branch translates to an MCP refusal so the
/// flock error code lands in `data.refusal`.
fn extract_flock_result(mut raw: Value, send_like: bool) -> Result<Value, McpError> {
    if let Some(error) = raw.get("error") {
        let code = error.get("code").and_then(Value::as_str).unwrap_or("error");
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("flock refused the request");
        let exit = crate::cli::error_code_exit(code, send_like);
        return Err(McpError::from_flock_error(code, message, exit));
    }
    // Take() rather than clone() — the raw envelope isn't needed again.
    let result = raw
        .get_mut("result")
        .map(Value::take)
        .unwrap_or(Value::Null);
    if let Some(code) = crate::api::effect_exit_code(&result) {
        let message = result["outcome"].as_str().unwrap_or_default().to_owned();
        let mut data = result;
        if let Some(fields) = data.as_object_mut() {
            let retry = crate::cli::output::exit_retry(code).as_str();
            fields.insert("retry".into(), retry.into());
            fields.insert("exit".into(), code.into());
        }
        return Err(McpError {
            code,
            message,
            data: Some(data),
        });
    }
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

    #[tokio::test]
    async fn message_targets_reach_the_real_app_inbox() {
        struct AppApi(RefCell<crate::app::App>);
        impl FlockCall for AppApi {
            fn call(&self, method: Method) -> Result<Value, ApiClientError> {
                let raw = self.0.borrow_mut().handle_api_request(Request {
                    id: "mcp-test".into(),
                    method,
                });
                Ok(serde_json::from_str(&raw).unwrap())
            }
        }
        let _store = crate::mesh::runtime_store::TestStore::new();
        let (_, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("recipient")];
        app.state.ensure_test_terminals();
        let ws = &app.state.workspaces[0];
        let pane = ws.focused_pane_id().unwrap();
        let pane_id = app.state.public_pane_id(0, pane).unwrap();
        let terminal_id = ws.tabs[0].panes[&pane].attached_terminal_id.clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.agent_name = Some("reviewer".into());
        let agent_id = terminal.agent_id.to_string();
        let api = AppApi(RefCell::new(app));
        for (index, to) in [
            json!(pane_id),
            json!(terminal_id),
            json!("reviewer"),
            json!(agent_id),
            json!({"type":"pane","pane":pane_id}),
            json!({"type":"agent","agent":agent_id}),
        ]
        .into_iter()
        .enumerate()
        {
            let result = route(
                "tools/call",
                json!({"name":"flock_msg_send", "arguments":{
                "to":to,"body":"hello","intent":"fyi","correlation_id":format!("shape-{index}")}}),
                &api,
                &ChannelOptions::off(),
            )
            .unwrap();
            let queued: Value =
                serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
            assert_eq!(queued["state"], "queued");
        }
        let read = api
            .call(Method::MsgRead(crate::api::schema::MsgReadParams {
                pane: Some(pane_id),
            }))
            .unwrap();
        assert_eq!(read["result"]["messages"].as_array().unwrap().len(), 6);
        let unknown = route(
            "tools/call",
            json!({"name":"flock_msg_send", "arguments":{
            "to":"missing-recipient","body":"x","intent":"fyi"}}),
            &api,
            &ChannelOptions::off(),
        )
        .unwrap_err();
        assert_eq!(unknown.data.unwrap()["kind"], "msg_target_not_found");
        {
            let mut app = api.0.borrow_mut();
            app.state
                .workspaces
                .push(crate::workspace::Workspace::test_new("other-recipient"));
            app.state.ensure_test_terminals();
            let other = app.state.workspaces[1].tabs[0]
                .panes
                .values()
                .next()
                .unwrap()
                .attached_terminal_id
                .clone();
            app.state.terminals.get_mut(&other).unwrap().agent_name = Some("reviewer".into());
        }
        let ambiguous = route(
            "tools/call",
            json!({"name":"flock_msg_send", "arguments":{
            "to":"reviewer","body":"x","intent":"fyi"}}),
            &api,
            &ChannelOptions::off(),
        )
        .unwrap_err();
        assert_eq!(ambiguous.data.unwrap()["kind"], "msg_target_ambiguous");
        assert!(ambiguous.message.contains("terminal_id="));
    }

    #[test]
    fn message_target_resolution_errors_keep_typed_kinds_and_shapes() {
        for (kind, exit) in [
            ("msg_target_not_found", crate::cli::exit::FAILURE),
            ("msg_target_ambiguous", crate::cli::exit::FAILURE),
        ] {
            let flock = MockApi::ok(
                json!({"error":{"code":kind,"message":"diagnostic wording is irrelevant"}}),
            );
            let err = route(
                "tools/call",
                json!({"name":"flock_msg_send",
                "arguments":{"to":"reviewer","body":"x","intent":"fyi"}}),
                &flock,
                &ChannelOptions::off(),
            )
            .unwrap_err();
            let data = err.data.unwrap();
            assert_eq!(data["kind"], kind);
            assert_eq!(data["refusal"], kind);
            assert_eq!(data["exit"], exit);
            assert!(data["expected"].is_array());
            assert!(err.message.contains("pane id"));
        }
    }

    #[test]
    fn send_outcomes_match_cli_and_mcp_with_evidence() {
        for (outcome, code) in [
            ("delivered", 0),
            ("accepted", 0),
            ("observed_accepted", 0),
            ("unconfirmed", 8),
            ("abandoned", 9),
        ] {
            let result = json!({"type":"guarded_submit", "outcome":outcome,
                "reason":"test_evidence", "attempt":{"state":outcome}});
            let raw = json!({"result":result});
            assert_eq!(crate::cli::print_response(&raw).unwrap(), code);
            let flock = MockApi::ok(raw);
            let response = route(
                "tools/call",
                json!({"name":"flock_pane_submit",
                "arguments":{"pane_id":"1-1","text":"hello"}}),
                &flock,
                &ChannelOptions::off(),
            );
            if code == 0 {
                let response = response.unwrap();
                let content: Value =
                    serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
                assert_eq!(content, result);
            } else {
                let error = response.unwrap_err();
                assert_eq!(error.code, code);
                assert_eq!(error.message, outcome);
                let retry = if code == 8 { "after_inspecting" } else { "yes" };
                let mut data = result.clone();
                data["retry"] = json!(retry);
                data["exit"] = json!(code);
                assert_eq!(error.data, Some(data));
            }
        }
    }

    /// #854: an older server ignores `instructions`, answers `armed` without
    /// the flag, and would compact without them. The bridge aborts that
    /// arming and says why instead of reporting success.
    #[test]
    fn self_compact_instructions_an_older_server_dropped_are_aborted() {
        let old = MockApi::ok(json!({"result": {
            "type": "pane_self_compact", "pane": "1-1", "state": "armed", "detail": "armed."
        }}));
        let err = route(
            "tools/call",
            json!({"name": "flock_self_compact", "arguments": {
                "continuation": "carry on", "instructions": "keep GOAL.md"}}),
            &old,
            &ChannelOptions::off(),
        )
        .expect_err("a dropped arming is not a success");
        assert_eq!(
            err.data,
            Some(json!({"refusal": crate::api::compatibility::ERROR_CODE,
                        "retry": "after_handoff", "exit": 78}))
        );
        let calls = old.calls.borrow();
        assert_eq!(calls.len(), 2, "the arming, then its abort");
        assert!(matches!(&calls[1], Method::PaneArmSelfCompact(p) if p.abort));
        drop(calls);

        // The same answer without instructions sent is just a plain arming.
        let route_ok = route(
            "tools/call",
            json!({"name": "flock_self_compact", "arguments": {"continuation": "carry on"}}),
            &old,
            &ChannelOptions::off(),
        );
        assert!(route_ok.is_ok());
        assert_eq!(old.calls.borrow().len(), 3, "no abort for a bare arming");

        let current = MockApi::ok(json!({"result": {
            "type": "pane_self_compact", "pane": "1-1", "state": "armed",
            "detail": "armed.", "instructions": true
        }}));
        route(
            "tools/call",
            json!({"name": "flock_self_compact", "arguments": {
                "continuation": "carry on", "instructions": "keep GOAL.md"}}),
            &current,
            &ChannelOptions::off(),
        )
        .expect("a server that stored them is a success");
        assert_eq!(current.calls.borrow().len(), 1);
    }

    /// #1179 / #868: `questions` goes only to a server that advertises it. An
    /// older one gets nothing started, and the agent is told why.
    #[test]
    fn a_question_policy_is_never_sent_to_a_server_that_does_not_advertise_it() {
        let fork = json!({"name": "flock_agent_fork",
                          "arguments": {"target": "w1:p1", "questions": "deny"}});
        let older = MockApi::ok(json!({"result": {"type": "pong", "capabilities": {
            "live_handoff": true, "agent_start_label": true
        }}}));
        let err = route("tools/call", fork.clone(), &older, &ChannelOptions::off())
            .expect_err("an older server must not get the start");
        assert_eq!(
            err.data,
            Some(json!({"refusal": crate::api::compatibility::ERROR_CODE,
                        "retry": "after_handoff", "exit": 78}))
        );
        let calls = older.calls.borrow();
        assert_eq!(calls.len(), 1, "only the ping");
        assert!(matches!(calls[0], Method::Ping(_)));
        drop(calls);

        let current = MockApi::ok(json!({"result": {"type": "pong", "capabilities": {
            "agent_questions": true
        }}}));
        let _ = route("tools/call", fork, &current, &ChannelOptions::off());
        let calls = current.calls.borrow();
        assert!(
            matches!(&calls[1], Method::AgentFork(p) if p.questions == Some(crate::api::schema::QuestionPolicy::Deny)),
            "{calls:?}"
        );
    }

    /// An older server whose abort then fails: the explanation survives, and
    /// says the plain arming is still there to drop.
    #[test]
    fn a_failed_abort_keeps_the_dropped_instructions_explanation() {
        struct AbortFails;
        impl FlockCall for AbortFails {
            fn call(&self, method: Method) -> Result<Value, ApiClientError> {
                match method {
                    Method::PaneArmSelfCompact(params) if params.abort => Ok(json!({
                        "error": {"code": "internal", "message": "socket closed"}
                    })),
                    _ => Ok(json!({"result": {
                        "type": "pane_self_compact", "pane": "1-1",
                        "state": "armed", "detail": "armed."
                    }})),
                }
            }
        }
        let err = route(
            "tools/call",
            json!({"name": "flock_self_compact", "arguments": {
                "continuation": "carry on", "instructions": "keep GOAL.md"}}),
            &AbortFails,
            &ChannelOptions::off(),
        )
        .expect_err("a dropped arming is not a success");
        assert_eq!(
            err.data,
            Some(json!({"refusal": crate::api::compatibility::ERROR_CODE,
                        "retry": "after_handoff", "exit": 78}))
        );
        assert!(
            err.message.contains("still armed") && err.message.contains("socket closed"),
            "{}",
            err.message
        );
    }

    #[test]
    fn initialize_returns_server_info() {
        let flock = MockApi::ok(json!({}));
        let result = route("initialize", json!({}), &flock, &ChannelOptions::off()).unwrap();
        assert_eq!(result["protocolVersion"], "2024-11-05");
        assert_eq!(result["capabilities"]["tools"], json!({}));
        assert_eq!(result["capabilities"]["resources"], json!({}));
        assert_eq!(result["serverInfo"]["name"], "flock");
        assert_eq!(result["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
        assert!(flock.calls.borrow().is_empty(), "initialize is client-only");
    }

    #[test]
    fn the_channel_capability_is_declared_only_with_the_flag_on() {
        let flock = MockApi::ok(json!({}));
        let off = route("initialize", json!({}), &flock, &ChannelOptions::off()).unwrap();
        assert!(off["capabilities"].get("experimental").is_none());
        assert!(off.get("instructions").is_none());

        let on_opts = ChannelOptions {
            push: true,
            ..ChannelOptions::off()
        };
        let on = route("initialize", json!({}), &flock, &on_opts).unwrap();
        assert_eq!(
            on["capabilities"]["experimental"]["claude/channel"],
            json!({})
        );
        assert!(on["instructions"]
            .as_str()
            .unwrap()
            .contains("flock_msg_read"));
        // Nothing else about the handshake moves with the flag.
        assert_eq!(on["capabilities"]["tools"], json!({}));
        assert_eq!(on["protocolVersion"], off["protocolVersion"]);
        // Permission relay is a separate opt-in flock does not make: it would
        // let whoever reaches the channel approve tool calls.
        assert!(on["capabilities"]["experimental"]
            .get("claude/channel/permission")
            .is_none());
    }

    #[test]
    fn notifications_initialized_produces_no_flock_call() {
        let flock = MockApi::ok(json!({}));
        route(
            "notifications/initialized",
            json!({}),
            &flock,
            &ChannelOptions::off(),
        )
        .unwrap();
        assert!(flock.calls.borrow().is_empty());
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let flock = MockApi::ok(json!({}));
        // `prompts/list` is a real MCP method flock does not implement —
        // `resources/list` stopped being one in #286.
        let err = route("prompts/list", json!({}), &flock, &ChannelOptions::off()).unwrap_err();
        assert_eq!(err.code, -32601);
    }

    #[test]
    fn tools_list_matches_the_table() {
        let flock = MockApi::ok(json!({}));
        let result = route("tools/list", json!({}), &flock, &ChannelOptions::off()).unwrap();
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
            &ChannelOptions::off(),
        )
        .unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        let inner: Value = serde_json::from_str(text).unwrap();
        assert_eq!(inner["type"], "agent_list");
        assert_eq!(flock.calls.borrow().len(), 1);
        assert!(matches!(flock.calls.borrow()[0], Method::AgentList(_)));
    }

    /// #960: the MCP tool and `flk delegate list` send the same request for
    /// the same scope and return the same rows for the same answer.
    #[test]
    fn flock_delegate_list_and_the_cli_return_the_same_rows() {
        let row = crate::api::schema::DelegateRow {
            architect: "agent_lead".into(),
            delegate: "agent_worker".into(),
            name: Some("bunny-614".into()),
            via: crate::api::schema::DelegationVia::Start,
            state: "closed, not merged".into(),
            label: Some("#614".into()),
            harness: Some("claude".into()),
            agent_status: Some(crate::api::schema::AgentStatus::Idle),
            status_age_secs: Some(12),
            edge_age_secs: 300,
            workspace_id: "w2".into(),
            pane_id: Some("2:p1".into()),
            worktree: None,
            branch: Some("feat/614".into()),
            repo: None,
            pr: Some(crate::api::schema::PrInfo {
                number: 624,
                state: "closed".into(),
            }),
            questions: Some(crate::api::schema::QuestionPolicy::Route),
        };
        let raw =
            json!({"id": "mcp:1", "result": {"type": "delegate_list", "delegates": [row.clone()]}});
        for (cli, mcp) in [
            (
                vec!["--all", "--branch", "feat/614"],
                json!({"architect": "all", "branch": "feat/614"}),
            ),
            (vec!["--mine"], json!({})),
            (
                vec!["--architect", "agent_lead"],
                json!({"architect": "agent_lead"}),
            ),
        ] {
            let flock = MockApi::ok(raw.clone());
            let result = route(
                "tools/call",
                json!({"name": "flock_delegate_list", "arguments": mcp}),
                &flock,
                &ChannelOptions::off(),
            )
            .unwrap();
            let text = result["content"][0]["text"].as_str().unwrap();
            let mcp_result: Value = serde_json::from_str(text).unwrap();
            let mcp_rows = crate::cli::delegate_list::rows(&mcp_result).expect("mcp rows");
            let cli_rows =
                crate::cli::delegate_list::rows(&crate::cli::delegate_list::json_output(&raw))
                    .expect("cli rows");
            assert_eq!(mcp_rows, cli_rows);
            assert_eq!(cli_rows, std::slice::from_ref(&row));

            let words: Vec<String> = cli.iter().map(|word| word.to_string()).collect();
            let (params, _) = crate::cli::delegate_list::parse_list_args(&words).unwrap();
            assert_eq!(
                serde_json::to_value(&flock.calls.borrow()[0]).unwrap(),
                serde_json::to_value(Method::DelegateList(params)).unwrap(),
                "{cli:?}"
            );
        }
    }

    #[test]
    fn flock_msg_status_routes_correlation_and_preserves_attempt_evidence() {
        let payload = json!({"type": "msg_status", "correlation_id": "mail-1", "state": "queued",
            "attempts": [{"attempt_id": "attempt-1", "pane": "fixture-pane", "state": "unconfirmed",
                "correlation_ids": ["mail-1"], "reason": "confirm_timeout"}]});
        let flock = MockApi::ok(json!({"id": "mcp:1", "result": payload}));
        let result = route(
            "tools/call",
            json!({"name": "flock_msg_status",
            "arguments": {"correlation_id": "mail-1"}}),
            &flock,
            &ChannelOptions::off(),
        )
        .unwrap();
        let inner: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(inner, payload);
        assert!(
            matches!(&flock.calls.borrow()[0], Method::MsgStatus(params) if params.correlation_id == "mail-1")
        );
    }

    #[test]
    fn tools_call_unknown_name_refuses_not_exposed() {
        // pane.close-shaped name — the design's canary for the hidden verbs.
        let flock = MockApi::ok(json!({}));
        let err = route(
            "tools/call",
            json!({ "name": "flock_pane_close", "arguments": { "pane_id": "p1" } }),
            &flock,
            &ChannelOptions::off(),
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
            &ChannelOptions::off(),
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
        let result = route("resources/list", json!({}), &flock, &ChannelOptions::off()).unwrap();
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
            &ChannelOptions::off(),
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
            &ChannelOptions::off(),
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
            &ChannelOptions::off(),
        )
        .unwrap_err();
        assert_eq!(err.code, -32602);
        assert!(flock.calls.borrow().is_empty(), "refusal short-circuits");
    }

    /// ADR-0029 §2 (#1024): every tool error carries the retry class and the
    /// exit status the CLI returns for the same failure.
    #[test]
    fn tool_errors_carry_retry_and_exit_from_the_registries() {
        let call = |tool: &str, arguments: Value, code: &str| {
            let flock = MockApi::ok(json!({"error": {"code": code, "message": "m"}}));
            let request = json!({"name": tool, "arguments": arguments});
            route("tools/call", request, &flock, &ChannelOptions::off())
                .unwrap_err()
                .data
        };
        let get = || json!({"target": "nosuch"});
        let submit = || json!({"pane_id": "1-1", "text": "hello"});
        assert_eq!(
            call("flock_agent_get", get(), "agent_not_found"),
            Some(json!({"refusal": "agent_not_found", "retry": "never", "exit": 1}))
        );
        // A `later` refusal exits 10 on a send-like tool, as `pane run` does,
        // and 1 on a read, as `agent get` does.
        assert_eq!(
            call("flock_pane_submit", submit(), "not_settled"),
            Some(json!({"refusal": "not_settled", "retry": "later", "exit": 10}))
        );
        assert_eq!(
            call("flock_agent_get", get(), "not_settled"),
            Some(json!({"refusal": "not_settled", "retry": "later", "exit": 1}))
        );
        assert_eq!(
            call("flock_agent_get", get(), "server_version_gap"),
            Some(json!({"refusal": "server_version_gap", "retry": "after_handoff", "exit": 78}))
        );

        struct Down;
        impl FlockCall for Down {
            fn call(&self, _: Method) -> Result<Value, ApiClientError> {
                Err(ApiClientError::Io(std::io::ErrorKind::NotFound.into()))
            }
        }
        let request = json!({"name": "flock_agent_get", "arguments": get()});
        let err = route("tools/call", request, &Down, &ChannelOptions::off()).unwrap_err();
        assert_eq!(
            err.data,
            Some(json!({"refusal": "transport_error", "retry": "maybe", "exit": 1}))
        );
        let hidden = json!({"name": "pane.close", "arguments": {}});
        let err = route("tools/call", hidden, &Down, &ChannelOptions::off()).unwrap_err();
        assert_eq!(
            err.data,
            Some(json!({"refusal": "not_exposed_via_mcp", "retry": "never", "exit": 2}))
        );
    }

    #[test]
    fn tools_call_missing_arguments_is_invalid_params() {
        let flock = MockApi::ok(json!({}));
        let err = route(
            "tools/call",
            json!({ "name": "flock_agent_get", "arguments": {} }),
            &flock,
            &ChannelOptions::off(),
        )
        .unwrap_err();
        assert_eq!(err.code, -32602);
        assert!(flock.calls.borrow().is_empty());
    }
}
