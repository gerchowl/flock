//! Pane-free read/control records (ADR-0036 §1/S3a-3).
use super::{
    agent_state::{AgentState, Lifecycle},
    schema::AgentStatus,
};
use serde::{Deserialize, Serialize};

/// Optional placement of an attached view, absent until ADR-0036 S10.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct StructuredAgentView {
    /// Workspace containing the view, not the runtime.
    pub workspace_id: String,
    /// Tab containing the view.
    pub tab_id: String,
    /// Pane rendering the view; it is not a PTY process identity.
    pub pane_id: String,
}

/// A session process has identity and state, but no fabricated PTY placement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct StructuredAgentInfo {
    /// Closed runtime tag, always structured.
    pub runtime: crate::structured::registry::StructuredRuntime,
    /// Stable registered agent identity.
    pub agent_id: String,
    /// Optional locally unique name.
    pub name: Option<String>,
    /// Supported harness adapter.
    pub harness: String,
    /// Host-owned process lifecycle.
    pub lifecycle: Lifecycle,
    /// Presentation only; supervisors use the complete state below.
    pub status: AgentStatus,
    /// Canonical ADR-0028 §1 projection, never inferred from a screen.
    pub state: AgentState,
    /// Native session identity.
    pub session_id: String,
    /// Registered launch working directory.
    pub cwd: String,
    /// Requested model, absent when provider-selected.
    pub model: Option<String>,
    /// Registry binding evidence, not caller authority or proof of liveness.
    pub lease_state: crate::structured::lease::LeaseState,
    /// No view is created by registration or process launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub view: Option<StructuredAgentView>,
}
