//! Receive policy for peer summaries (ADR-0014 §4). Presentation is sanitized
//! and bounded; opaque routing identifiers are preserved or discarded, never
//! rewritten into another identity. No rejected content enters diagnostics.
use crate::api::schema::{PeerAgentSummary, PeerWorkspaceSummary};

/// Maximum Unicode scalars retained in any peer presentation field. 256 leaves
/// room for project keys and activity while bounding per-row render/search work.
/// Identity labels keep their stricter 64-scalar policy. Thermal labels retain
/// their additional 16-byte bound. Routing tokens use a separate 256-byte bound.
pub(crate) const MAX_TEXT_CHARS: usize = 256;
/// Maximum bytes in an opaque ASCII identifier or SSH route. Oversized values
/// are discarded or refused, never truncated into a different destination.
pub(crate) const MAX_TOKEN_BYTES: usize = 256;

/// Sanitize and bound a peer presentation field without an intermediate unbounded copy.
pub(crate) fn text(value: &str) -> String {
    crate::terminal::agent_name::presentation_limited(value, MAX_TEXT_CHARS)
}

/// Admit a reported hostname without rewriting it (ADR-0014 §4). A-labels
/// remain ASCII; SSH destinations and relay edge names have separate grammars.
fn host(value: &str) -> bool {
    let valid = !value.is_empty()
        && value.len() <= 253
        && value.split('.').all(|label| {
            let bytes = label.as_bytes();
            bytes.len() <= 63
                && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
                && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
                && bytes
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
        });
    if !valid {
        crate::logging::peer_summary_identifier_dropped("host");
    }
    valid
}

/// Validate a relay edge identity without rewriting it. Unlike presentation,
/// identities may not lose controls, format characters or an oversized suffix.
/// The byte bound matches other opaque routing tokens; warnings carry no value.
pub(super) fn identity(value: &str, field: &'static str) -> bool {
    if value.is_empty()
        || value.len() > MAX_TOKEN_BYTES
        || value.chars().any(crate::terminal::agent_name::unsafe_char)
    {
        crate::logging::peer_summary_identifier_dropped(field);
        false
    } else {
        true
    }
}

fn optional(value: &mut Option<String>) {
    if let Some(text_value) = value {
        *text_value = text(text_value);
    }
}

fn token(value: &mut String, field: &'static str) {
    if value.len() > MAX_TOKEN_BYTES || !value.bytes().all(|b| b.is_ascii_graphic()) {
        crate::logging::peer_summary_identifier_dropped(field);
        *value = String::new();
    }
}

impl PeerAgentSummary {
    /// Admit a received directory row. Invalid opaque identifiers become empty
    /// and are omitted from workspace directories; valid identifiers stay exact.
    pub(crate) fn received(mut self) -> Self {
        token(&mut self.agent_id, "agent_id");
        token(&mut self.pane_id, "pane_id");
        self.agent = crate::terminal::agent_name::received(self.agent, "peer_summary");
        self
    }
}

impl PeerWorkspaceSummary {
    /// One receive step shared by JSON, binary fleet and relay admission.
    /// Project keys are presentation/grouping input too. An invalid workspace
    /// identifier is cleared; legacy id-less peers already use name-based focus.
    pub(crate) fn received(mut self) -> Self {
        token(&mut self.id, "workspace_id");
        self.workspace = text(&self.workspace);
        for field in [
            &mut self.project_key,
            &mut self.project_label,
            &mut self.branch,
            &mut self.activity,
        ] {
            optional(field);
        }
        self.agent = crate::terminal::agent_name::received(self.agent, "peer_summary");
        self.agents = self
            .agents
            .into_iter()
            .map(PeerAgentSummary::received)
            .filter(|agent| !agent.agent_id.is_empty() && !agent.pane_id.is_empty())
            .collect();
        self
    }
}

impl super::PeerSummaryPayload {
    /// Admit the parsed JSON envelope before it can populate state or snapshots.
    pub(super) fn received(mut self) -> Result<Self, String> {
        if !self.host.is_empty() && !host(&self.host) {
            return Err(crate::api::error_code::ErrorCode::InvalidParams
                .as_str()
                .into());
        }
        optional(&mut self.version);
        optional(&mut self.icon);
        if let Some(node) = &mut self.node_id {
            token(node, "node_id");
            if node.is_empty() {
                self.node_id = None;
            }
        }
        self.workspaces = self
            .workspaces
            .into_iter()
            .map(PeerWorkspaceSummary::received)
            .collect();
        self.relayed_fleet = self
            .relayed_fleet
            .into_iter()
            .filter_map(crate::api::schema::RelayedFleetPeer::received)
            .collect();
        Ok(self)
    }
}

impl super::PeerSummaryState {
    /// Admit a materialized fleet row. Invalid SSH routes are disabled, never edited.
    pub(super) fn received(mut self) -> Self {
        let invalid_host = self
            .host
            .as_deref()
            .filter(|host| !host.is_empty())
            .is_some_and(|value| !host(value));
        if invalid_host {
            self.host = None;
            self.ssh_target.clear();
            self.proxy_jump = None;
            self.error = Some(
                crate::api::error_code::ErrorCode::InvalidParams
                    .as_str()
                    .into(),
            );
        }
        self.peer = text(&self.peer);
        for field in [
            &mut self.host,
            &mut self.version,
            &mut self.error,
            &mut self.icon,
        ] {
            optional(field);
        }
        if let Some(node) = &mut self.node_id {
            token(node, "node_id");
            if node.is_empty() {
                self.node_id = None;
            }
        }
        let valid_target = self.ssh_target == crate::protocol::HOME_SWITCH_TARGET
            || (self.ssh_target.len() <= MAX_TOKEN_BYTES
                && crate::remote::is_valid_ssh_destination(&self.ssh_target));
        let valid_jump = self.proxy_jump.as_deref().is_none_or(|jump| {
            jump.is_empty()
                || (jump.len() <= MAX_TOKEN_BYTES && crate::remote::is_valid_ssh_proxy_jump(jump))
        });
        if !valid_target || !valid_jump {
            if !invalid_host {
                crate::logging::peer_summary_identifier_dropped("ssh_route");
            }
            self.ssh_target = String::new();
            self.proxy_jump = None;
            self.error = Some(
                crate::api::error_code::ErrorCode::InvalidParams
                    .as_str()
                    .into(),
            );
        }
        self.workspaces = self
            .workspaces
            .into_iter()
            .map(PeerWorkspaceSummary::received)
            .collect();
        self
    }
}

impl crate::api::schema::RelayedFleetPeer {
    /// Admit a raw relayed row before keying its cache or exposing its fields.
    /// Refuse unsafe host identities before cleaning any presentation text.
    /// SSH destinations remain untouched here and are checked before admission.
    pub(crate) fn received(mut self) -> Option<Self> {
        if self
            .host
            .as_deref()
            .filter(|host| !host.is_empty())
            .is_some_and(|value| !host(value))
        {
            return None;
        }
        self.name = text(&self.name);
        self.origin = text(&self.origin);
        for field in [
            &mut self.host,
            &mut self.version,
            &mut self.error,
            &mut self.icon,
        ] {
            optional(field);
        }
        if let Some(node) = &mut self.node_id {
            token(node, "node_id");
            if node.is_empty() {
                self.node_id = None;
            }
        }
        if let Some(dial) = &mut self.dial {
            for field in [&mut dial.reason, &mut dial.stream_reason] {
                *field = field
                    .as_ref()
                    .map(|reason| super::SshFailureReason::classify(reason).as_str().into());
            }
        }
        self.workspaces = self
            .workspaces
            .into_iter()
            .map(PeerWorkspaceSummary::received)
            .collect();
        Some(self)
    }
}
