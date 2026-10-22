pub mod agent_state;
pub mod client;
pub(crate) mod client_listener;
pub mod client_tokens;
pub(crate) mod compatibility;
pub(crate) mod error_code;
mod event_hub;
mod events_read;
pub mod question;
mod reply_wait;
pub mod schema;
pub(crate) mod scoped;
pub(crate) mod server;
mod status;
pub mod structured_agent;
mod subscriptions;
mod wait;
pub mod wait_predicate;

pub use event_hub::EventHub;
pub(crate) use reply_wait::{best_answer, Answer};
pub(crate) use server::socket_peer_pid;
pub(crate) use server::start_server_for_mode;
pub use server::ServerHandle;
pub(crate) use server::{start_server, start_server_with_capabilities};
pub use status::{read_runtime_status_at, RuntimeStatus};

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::api::schema::{Method, Request};

pub const SOCKET_PATH_ENV_VAR: &str = "FLOCK_SOCKET_PATH";

pub(crate) fn request_changes_ui(request: &Request) -> bool {
    if request.method.is_allocation_preview() {
        return false;
    }
    matches!(
        &request.method,
        Method::ServerReloadConfig(_)
            | Method::NotificationShow(_)
            | Method::WorkspaceCreate(_)
            | Method::WorkspaceFocus(_)
            | Method::WorkspaceRename(_)
            | Method::WorkspaceClose(_)
            | Method::WorktreeCreate(_)
            | Method::WorktreeOpen(_)
            | Method::WorktreeRemove(_)
            | Method::WorktreeKill(_)
            | Method::TabCreate(_)
            | Method::TabFocus(_)
            | Method::TabRename(_)
            | Method::TabClose(_)
            | Method::AgentRename(_)
            | Method::AgentFocus(_)
            | Method::AgentStart(_)
            | Method::AgentFork(_)
            | Method::AgentSpawn(_)
            | Method::AgentRestart(_)
            | Method::MsgSend(_)
            | Method::MsgRead(_)
            | Method::MsgReply(_)
            | Method::PeersHubFleet(_)
            | Method::PaneSplit(_)
            | Method::PaneMove(_)
            | Method::PaneRename(_)
            | Method::PaneReportAgent(_)
            | Method::PaneReportAgentSession(_)
            | Method::PaneReportMetadata(_)
            | Method::PaneSetHeaderField(_)
            | Method::PaneClearHeaderField(_)
            | Method::PaneClearAgentAuthority(_)
            | Method::PaneReleaseAgent(_)
            | Method::PaneClose(_)
    )
}

pub struct ApiRequestMessage {
    /// Scoped transport attribution; None is an ordinary legacy request.
    pub(crate) scope: Option<scoped::Scope>,
    pub request: Request,
    pub respond_to: std::sync::mpsc::Sender<String>,
    /// PID of the process on the other end of the API socket, when the
    /// platform exposes it. Lets pane reports resolve by process ancestry
    /// when their env-baked pane id has gone stale.
    pub peer_pid: Option<u32>,
    /// Process birth captured with socket credentials, before queueing the request.
    pub peer_birth: Option<u64>,
    /// Effective UID captured from kernel socket credentials.
    pub peer_uid: Option<u32>,
}

pub type ApiRequestSender = mpsc::UnboundedSender<ApiRequestMessage>;

pub fn socket_path() -> PathBuf {
    crate::session::active_api_socket_path()
}

/// Shared CLI and MCP effect-failure contract.
pub(crate) fn effect_exit_code(result: &serde_json::Value) -> Option<i32> {
    match result.get("outcome").and_then(serde_json::Value::as_str) {
        Some("unconfirmed") => Some(crate::cli::exit::UNCONFIRMED),
        Some("abandoned") => Some(crate::cli::exit::ABANDONED),
        _ => None,
    }
}

#[cfg(test)]
mod poller_compat_tests;
