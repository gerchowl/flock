//! Lifecycle verbs share the socket API parameter contracts.
use crate::api::schema::{AgentRestartParams, AgentStartParams, AgentTarget};
use serde::{Deserialize, Serialize};

/// Local lifecycle commands admitted only with an operate-scoped credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "verb", content = "args")]
pub enum AgentLifecycleCommand {
    /// Fresh launch only; structured resume authority remains socket-only.
    #[serde(rename = "agent.start")]
    Start(AgentStartParams),
    /// Preserve the backend's terminal-only restart contract.
    #[serde(rename = "agent.restart")]
    Restart(AgentRestartParams),
    /// Preserve structured stop's independent operator identity boundary.
    #[serde(rename = "agent.stop")]
    Stop(AgentTarget),
}
