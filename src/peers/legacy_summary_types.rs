//! Frozen v5 summary types from origin/dev at 11a3be37119172ddc32d5e73c6756b6f5e3fb13a.
// Keep unused fields/methods: this fixture pins the full old decoder contract.
#![allow(dead_code)]
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerSystemSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_free: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thermal: Option<ThermalReport>,
}

impl PeerSystemSummary {
    pub fn sanitized(mut self) -> Self {
        self.thermal = self.thermal.map(ThermalReport::sanitized);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThermalComponent {
    #[default]
    Cpu,
    Gpu,
    Node,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThermalReport {
    pub severity: u8,
    #[serde(default)]
    pub component: ThermalComponent,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
}

pub const THERMAL_TINT_MIN_SEVERITY: u8 = 2;

pub const THERMAL_SEVERITY_MAX: u8 = 3;

pub const THERMAL_LABEL_MAX_BYTES: usize = 16;

impl ThermalReport {
    pub fn sanitized(mut self) -> Self {
        self.severity = self.severity.min(THERMAL_SEVERITY_MAX);
        if self.label.len() > THERMAL_LABEL_MAX_BYTES {
            let mut end = THERMAL_LABEL_MAX_BYTES;
            while end > 0 && !self.label.is_char_boundary(end) {
                end -= 1;
            }
            self.label.truncate(end);
        }
        self
    }

    pub fn tints(&self) -> bool {
        self.severity >= THERMAL_TINT_MIN_SEVERITY
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerWorkspaceSummary {
    #[serde(default)]
    pub id: String,
    pub workspace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default)]
    pub is_linked_worktree: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "crate::terminal::agent_name::deserialize")]
    pub agent: Option<String>,
    pub status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_age_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<PeerAgentSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerAgentSummary {
    pub agent_id: String,
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "crate::terminal::agent_name::deserialize")]
    pub agent: Option<String>,
    pub status: AgentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayedFleetPeer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    pub name: String,
    pub ssh_target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<PeerSystemSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(default)]
    pub workspaces: Vec<PeerWorkspaceSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_last_ok_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_jump: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dial: Option<PeerDialReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerDialReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default)]
    pub consecutive_failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failing_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
    Hibernated,
    Offline,
}
