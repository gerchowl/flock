//! Shared diagnosis for CLI and MCP calls to older API servers.
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use super::client::ApiClient;
use super::schema::{Method, PingParams, Request};

pub const EXIT_CODE: i32 = crate::cli::exit::PROTOCOL_GAP;
pub const ERROR_CODE: &str = super::error_code::ErrorCode::ServerVersionGap.as_str();

#[derive(Clone)]
pub struct ServerVersion {
    pub version: String,
    pub protocol: u32,
}

impl ServerVersion {
    pub fn message(&self) -> String {
        format!(
            "server flk {} (protocol {}) is incompatible with client flk {} (protocol {}); hand it off with 'flk server live-handoff' or restart the server",
            self.version,
            self.protocol,
            crate::build_info::version(),
            crate::protocol::PROTOCOL_VERSION,
        )
    }
}

#[derive(Default)]
struct Invocation {
    target: Option<PathBuf>,
    server: Option<ServerVersion>,
    probed: bool,
    // Refuse state-changing requests to an older server (#868).
    gate: bool,
    warn: Option<fn(&ServerVersion)>,
    failure: Option<String>,
}

thread_local! {
    // Only the synchronous CLI invocation caches a probe. Long-lived MCP
    // processes diagnose against the current server on each failed call.
    static INVOCATION: RefCell<Invocation> = RefCell::new(Invocation::default());
}

pub fn begin_cli(client: &ApiClient, gate: bool, warn: fn(&ServerVersion)) {
    INVOCATION.with_borrow_mut(|state| {
        *state = Invocation {
            target: Some(client.socket_path()),
            gate,
            warn: Some(warn),
            ..Default::default()
        };
    });
}

pub fn observe_ping(client: &ApiClient, response: &Value) {
    let server = parse_ping(response);
    let warning = INVOCATION.with_borrow_mut(|state| {
        if state.target.as_ref() != Some(&client.socket_path()) {
            return None;
        }
        let warning = (!state.probed).then_some(state.warn).flatten();
        state.probed = true;
        state.server = server.clone();
        warning
    });
    if let (Some(warn), Some(server)) = (warning, server) {
        if server.protocol != crate::protocol::PROTOCOL_VERSION {
            warn(&server);
        }
    }
}

pub fn end_cli() -> Option<String> {
    INVOCATION.with_borrow_mut(|state| std::mem::take(state).failure)
}

fn cached(path: &Path) -> Option<Option<ServerVersion>> {
    INVOCATION.with_borrow(|state| {
        (state.target.as_deref() == Some(path) && state.probed).then(|| state.server.clone())
    })
}

fn probe(client: &ApiClient) -> Option<ServerVersion> {
    let response = client
        .request_value_with_timeout(
            &Request {
                id: "api:compatibility".into(),
                method: Method::Ping(PingParams::default()),
            },
            Duration::from_millis(500),
        )
        .ok()?;
    parse_ping(&response)
}

fn parse_ping(response: &Value) -> Option<ServerVersion> {
    Some(ServerVersion {
        version: response["result"]["version"].as_str()?.to_owned(),
        protocol: response["result"]["protocol"].as_u64()?.try_into().ok()?,
    })
}

pub fn is_capability_failure(reason: &str) -> bool {
    reason.contains("unknown variant")
        || reason.contains("unknown method")
        || reason.contains("method not found")
}

pub fn normalize(client: &ApiClient, response: &mut Value) {
    let Some(error) = response.get("error") else {
        return;
    };
    if !matches!(
        error["code"].as_str(),
        Some("unknown_method" | "method_not_found")
    ) && error["code"].as_i64() != Some(-32601)
        && !is_capability_failure(error["message"].as_str().unwrap_or_default())
    {
        return;
    }
    let path = client.socket_path();
    let server = server_version(client);
    let Some(server) = server else { return };
    if server.protocol >= crate::protocol::PROTOCOL_VERSION {
        return;
    }
    response["error"] = serde_json::json!({
        "code": ERROR_CODE,
        "message": server.message(),
    });
    record_failure(&path, &server);
}

fn record_failure(path: &Path, server: &ServerVersion) {
    INVOCATION.with_borrow_mut(|state| {
        if state.target.as_deref() == Some(path) {
            state.failure = Some(server.message());
        }
    });
}

/// The refusal a CLI answers itself with instead of sending `request` to a
/// server older than the client (#868).
///
/// An older server takes the first steps of `flk delegate start` (a worktree,
/// a workspace, an agent) and then rejects a later method it has never heard
/// of, so the skew surfaced as a raw serde error over a half-started delegate.
/// Every state-changing request now checks first, through the one probe this
/// invocation caches, so the whole command costs a single extra round trip
/// and the refusal lands before anything is allocated. Only a CLI invocation
/// that asked for the gate is checked: `flk server` and `flk status` must keep
/// reaching an older server, since handing it off is the fix, and a server
/// whose version cannot be read is let through to fail as it would have.
pub fn refuse_skew(client: &ApiClient, request: &Request) -> Option<Value> {
    if !request.method.needs_current_protocol() {
        return None;
    }
    let path = client.socket_path();
    let gated = INVOCATION.with_borrow(|state| state.gate && state.target.as_ref() == Some(&path));
    if !gated {
        return None;
    }
    let server = server_version(client)?;
    if server.protocol >= crate::protocol::PROTOCOL_VERSION {
        return None;
    }
    record_failure(&path, &server);
    Some(serde_json::json!({
        "id": request.id,
        "error": {"code": ERROR_CODE, "message": server.message()},
    }))
}

pub fn server_version(client: &ApiClient) -> Option<ServerVersion> {
    let path = client.socket_path();
    if let Some(server) = cached(&path) {
        return server;
    }
    let server = probe(client);
    INVOCATION.with_borrow_mut(|state| {
        if state.target.as_ref() == Some(&path) {
            state.probed = true;
            state.server = server.clone();
        }
    });
    server
}

/// An absent or malformed capability never permits a structured selector.
pub(crate) fn advertises_structured(pong: &Value) -> bool {
    pong.pointer("/result/capabilities/structured_agents")
        .and_then(Value::as_bool)
        == Some(true)
}
