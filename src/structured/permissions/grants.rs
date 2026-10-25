//! Operator management of exact live-runtime grants (ADR-0036 S6, #2362).
use super::*;
use serde::Serialize;

/// A grant's originating decision; identity is fenced by agent and generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantHandle {
    pub request_id: String,
    pub request_seq: u64,
}
/// Installed grant metadata. The key is canonical JSON `[tool_name, input]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGrant {
    #[serde(flatten)]
    pub handle: GrantHandle,
    pub tool: String,
    pub canonical_input_key: String,
    pub installed_ms: u128,
}
/// Exact structured agent identity; names and pane IDs are not grant authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantsList {
    pub agent_id: String,
}
/// Revocation must use the generation and grant handle observed by the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantsRevoke {
    pub agent_id: String,
    pub generation: u64,
    pub request_id: String,
    pub request_seq: u64,
}

impl Book {
    pub fn grants(&self) -> Vec<SessionGrant> {
        self.grants
            .iter()
            .map(|(key, grant)| SessionGrant {
                handle: GrantHandle {
                    request_id: grant.request_id.clone(),
                    request_seq: grant.request_seq,
                },
                tool: grant.tool.clone(),
                canonical_input_key: key.clone(),
                installed_ms: grant.installed_ms,
            })
            .collect()
    }
    pub fn revoke(&mut self, handle: &GrantHandle) -> Result<(), ErrorCode> {
        let key = self
            .grants
            .iter()
            .find(|(_, grant)| {
                grant.request_id == handle.request_id && grant.request_seq == handle.request_seq
            })
            .map(|(key, _)| key.clone())
            .or_else(|| {
                self.open
                    .get(&handle.request_id)
                    .filter(|open| open.request_seq == handle.request_seq)
                    .and_then(|open| open.grant.clone())
            })
            .ok_or(ErrorCode::PermissionGrantUnknown)?;
        // Only in-flight installs need epochs; prune completed keys before growing
        // the map. Live requests bound retained entries independently of old grants.
        self.grant_epochs.retain(|key, _| {
            self.open
                .values()
                .any(|open| open.grant.as_ref() == Some(key))
        });
        let epoch = self
            .grant_epochs
            .get(&key)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(ErrorCode::InternalError)?;
        self.grant_epochs.insert(key.clone(), epoch);
        self.grants.remove(&key);
        Ok(())
    }
}
