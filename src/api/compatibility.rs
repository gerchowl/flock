//! Shared diagnosis for CLI and MCP calls to older API servers.
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use super::client::ApiClient;
use super::schema::{Method, PingParams, Request};

pub const EXIT_CODE: i32 = 78;
pub const ERROR_CODE: &str = "server_version_gap";
// pane.submit shipped with durable submission evidence in protocol 27.
pub const SUBMIT_PROTOCOL: u32 = 27;

#[derive(Clone)]
pub struct ServerVersion {
    pub version: String,
    pub protocol: u32,
}

impl ServerVersion {
    pub fn message(&self, required: u32) -> String {
        format!(
            "server is flk {} (protocol {}); this command needs protocol {required}. Hand off with 'flk server live-handoff' or restart the server",
            self.version, self.protocol,
        )
    }
}

#[derive(Default)]
struct Invocation {
    target: Option<PathBuf>,
    server: Option<ServerVersion>,
    probed: bool,
    warn: Option<fn(&ServerVersion)>,
    failure: Option<String>,
}

thread_local! {
    // Only the synchronous CLI invocation caches a probe. Long-lived MCP
    // processes diagnose against the current server on each failed call.
    static INVOCATION: RefCell<Invocation> = RefCell::new(Invocation::default());
}

pub fn begin_cli(client: &ApiClient, warn: fn(&ServerVersion)) {
    INVOCATION.with_borrow_mut(|state| {
        *state = Invocation {
            target: Some(client.socket_path()),
            warn: Some(warn),
            ..Default::default()
        };
    });
}

pub fn before_request(client: &ApiClient, request: &Request) {
    if matches!(request.method, Method::Ping(_)) {
        return;
    }
    let needs_probe = INVOCATION
        .with_borrow(|state| state.target.as_ref() == Some(&client.socket_path()) && !state.probed);
    if needs_probe {
        let _ = server_version(client);
    }
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
    let server = cached(&path).unwrap_or_else(|| probe(client));
    let Some(server) = server else { return };
    response["error"] = serde_json::json!({
        "code": ERROR_CODE,
        "message": server.message(crate::protocol::PROTOCOL_VERSION),
    });
    INVOCATION.with_borrow_mut(|state| {
        if state.target.as_deref() == Some(&path) {
            state.failure = response["error"]["message"].as_str().map(str::to_owned);
        }
    });
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

pub fn require_submit(client: &ApiClient) -> Option<String> {
    let server = server_version(client)?;
    (server.protocol < SUBMIT_PROTOCOL).then(|| server.message(SUBMIT_PROTOCOL))
}
