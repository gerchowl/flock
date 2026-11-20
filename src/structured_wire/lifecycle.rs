//! Lifecycle verbs share the socket API parameter contracts.
use crate::api::schema::{AgentRestartParams, AgentStartParams, AgentTarget};
use serde::{Deserialize, Serialize};

/// Local lifecycle commands admitted only with an operate-scoped credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "verb", content = "args")]
pub enum AgentLifecycleCommand {
    /// Fresh launch only; structured resume uses restart with operator app-session authority.
    #[serde(rename = "agent.start")]
    Start(AgentStartParams),
    /// Restart terminal agents, or resume structured agents with an operate app-session token.
    #[serde(rename = "agent.restart")]
    Restart(AgentRestartParams),
    /// Stop a structured agent with an operate app-session token on its home node.
    #[serde(rename = "agent.stop")]
    Stop(AgentTarget),
}
