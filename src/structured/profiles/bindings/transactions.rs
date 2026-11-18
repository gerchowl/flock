//! Inactive, operator-reviewed binding transactions (ADR-0052 §9).

use super::super::ProfileAdmission;
use super::*;
use crate::api::error_code::ErrorCode as E;
use crate::platform::suspend_clock::{now, Kernel};
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Closed preparation operations; none authorizes execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PlanRequest {
    /// Review an exact binding change without committing it.
    Bind(BindingSpec),
}

/// Operator-requested binding data, excluding server-allocated revisions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingSpec {
    /// Existing binding identity, or absent to allocate a fresh identity.
    #[serde(default)]
    pub binding_id: Option<String>,
    /// Explicit repository, nodes and agents.
    pub target: Scope,
    /// Account presentation alias.
    pub account_alias: String,
    /// Opaque credential identifier, never dereferenced.
    pub credential_ref: String,
    /// Authorized credential generation.
    pub credential_generation: u64,
    /// Exact content identities, without adoption or verification.
    pub bundle_digests: Vec<String>,
    /// Operator-issued selection grant identifiers.
    pub grants: Vec<String>,
}

/// Emergency terminal binding revocation, without a preview.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeRequest {
    /// Binding identity to revoke.
    pub binding: String,
    /// Nonempty operator reason, retained in the tombstone.
    pub reason: String,
}

/// Server-private immutable review bytes and a monotonic expiration fence.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    review: Value,
    deadline: Duration,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    operation_id: String,
    action: String,
    principal: u32,
    expect: String,
    binding: BindingView,
}

impl ProfileAdmission {
    /// Prepare an immutable review under current policy, without executing assets.
    pub(crate) fn plan_bind(&mut self, principal: u32, request: PlanRequest) -> Result<Value, E> {
        let PlanRequest::Bind(spec) = request;
        let now = monotonic()?;
        let deadline = now + Duration::from_secs(300);
        let expires = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| E::PolicyRevisionUnavailable)?
            .as_millis()
            + 300_000;
        let mut binding = json!(spec);
        if binding["binding_id"].is_null() {
            binding["binding_id"] = json!(identifier()?);
        }
        binding["revision"] = json!(1);
        binding["revoked"] = Value::Null;
        let binding: BindingView = serde_json::from_value(binding).map_err(|_| E::InvalidParams)?;
        if !binding.valid(1) {
            return Err(E::InvalidParams);
        }
        let id = identifier()?;
        let review = self.edit(|disk, _| {
            let mut binding = binding;
            binding.revision = disk.policy_revision.checked_add(1).ok_or(E::PolicyRevisionUnavailable)?;
            let old = disk.current(&binding.binding_id)?;
            if old.is_some_and(|b| b.revoked.is_some()) { return Err(E::ProfileBindingRevoked); }
            Ok(json!({"action":"bind","plan":id,"principal":principal,"expected_revision":disk.policy_revision,"expires_at_ms":expires,"binding":binding,"previous":old}))
        })?;
        let expect = digest(&review)?;
        self.plans.retain(|_, plan| plan.deadline > now);
        self.plans.insert(
            id.clone(),
            Plan {
                review: review.clone(),
                deadline,
            },
        );
        Ok(json!({"plan":id,"expect":expect,"review":review}))
    }

    /// Commit exactly the reviewed plan, or return its durable prior receipt.
    pub(crate) fn bind(&mut self, principal: u32, id: &str, expect: &str) -> Result<Value, E> {
        let plan = self.plans.get(id).cloned();
        self.edit(|disk, path| {
            if let Some(receipt) = disk.receipt(id, "bind", principal, expect)? {
                return Ok(json!(receipt));
            }
            let plan = plan.ok_or(E::ProfilePlanInvalid)?;
            if plan.review["principal"] != principal
                || plan.review["plan"] != id
                || digest(&plan.review)? != expect
            {
                return Err(E::ProfilePlanInvalid);
            }
            if monotonic()? >= plan.deadline {
                return Err(E::ProfilePlanExpired);
            }
            if plan.review["expected_revision"] != disk.policy_revision {
                return Err(E::ProfileRevisionConflict);
            }
            let mut binding: BindingView = serde_json::from_value(plan.review["binding"].clone())
                .map_err(|_| E::ProfilePlanInvalid)?;
            if disk
                .current(&binding.binding_id)?
                .is_some_and(|b| b.revoked.is_some())
            {
                return Err(E::ProfileBindingRevoked);
            }
            disk.policy_revision = disk
                .policy_revision
                .checked_add(1)
                .ok_or(E::PolicyRevisionUnavailable)?;
            binding.revision = disk.policy_revision;
            if !binding.valid(disk.policy_revision) {
                return Err(E::ProfilePlanInvalid);
            }
            disk.bindings.retain(|b| b.binding_id != binding.binding_id);
            disk.bindings.push(binding.clone());
            disk.commit(
                path,
                Receipt {
                    operation_id: id.into(),
                    action: "bind".into(),
                    principal,
                    expect: expect.into(),
                    binding,
                },
            )
        })
    }

    /// Revoke before acknowledging; repeating the same terminal operation is inert.
    pub(crate) fn revoke(&mut self, principal: u32, request: RevokeRequest) -> Result<Value, E> {
        if !explicit(&request.reason) {
            return Err(E::InvalidParams);
        }
        let expect = digest(&json!({"action":"revoke","principal":principal,"request":request}))?;
        self.edit(|disk, path| {
            let mut binding = disk
                .current(&request.binding)?
                .cloned()
                .ok_or(E::ProfileBindingNotFound)?;
            if let Some(revoked) = &binding.revoked {
                return disk
                    .receipt(&revoked.operation_id, "revoke", principal, &expect)?
                    .map(|r| json!(r))
                    .ok_or(E::PolicyRevisionUnavailable);
            }
            disk.policy_revision = disk
                .policy_revision
                .checked_add(1)
                .ok_or(E::PolicyRevisionUnavailable)?;
            let id = identifier()?;
            binding.revision = disk.policy_revision;
            binding.revoked = Some(Revocation {
                reason: request.reason,
                principal: format!("uid:{principal}"),
                operation_id: id.clone(),
                revision: disk.policy_revision,
            });
            disk.bindings.retain(|b| b.binding_id != binding.binding_id);
            disk.bindings.push(binding.clone());
            disk.commit(
                path,
                Receipt {
                    operation_id: id,
                    action: "revoke".into(),
                    principal,
                    expect,
                    binding,
                },
            )
        })
    }

    fn edit(
        &mut self,
        apply: impl FnOnce(&mut BindingStore, &Path) -> Result<Value, E>,
    ) -> Result<Value, E> {
        let path = self
            .path
            .clone()
            .unwrap_or_else(|| crate::session::data_dir().join("profile-bindings.json"));
        let parent = path.parent().ok_or(E::PolicyRevisionUnavailable)?;
        let result = BindingStore::transaction(&path, || {
            let mut disk = BindingStore {
                policy_revision: self.store.policy_revision,
                ..BindingStore::default()
            };
            disk.load_locked(&path);
            let previous = disk.policy_revision;
            let result = match disk.refusal() {
                Some(fault) => Err(fault.code()),
                None => apply(&mut disk, &path),
            };
            if result.is_err() {
                disk = BindingStore {
                    policy_revision: previous,
                    ..BindingStore::default()
                };
                disk.load_locked(&path);
            }
            if result.is_ok() {
                fs::File::open(parent)?.sync_all()?;
            }
            Ok((result, disk))
        });
        match result {
            Ok((result, disk)) => {
                self.store = disk;
                result
            }
            Err(_) => {
                self.store
                    .refuse(super::super::ProfileRefusal::PolicyRevisionUnavailable);
                Err(E::PolicyRevisionUnavailable)
            }
        }
    }
}

impl BindingStore {
    fn current(&self, id: &str) -> Result<Option<&BindingView>, E> {
        if self.unknown.iter().any(|raw| raw["binding_id"] == id) {
            return Err(E::PolicyRevisionUnavailable);
        }
        Ok(self.bindings.iter().find(|b| b.binding_id == id))
    }

    fn receipt(
        &self,
        id: &str,
        action: &str,
        principal: u32,
        expect: &str,
    ) -> Result<Option<Receipt>, E> {
        let Some(raw) = self.root.get("operations") else {
            return Ok(None);
        };
        let entries = raw.as_array().ok_or(E::PolicyRevisionUnavailable)?;
        let mut matches = entries.iter().filter(|r| r["operation_id"] == id);
        let Some(raw) = matches.next() else {
            return Ok(None);
        };
        let receipt: Receipt =
            serde_json::from_value(raw.clone()).map_err(|_| E::PolicyRevisionUnavailable)?;
        if matches.next().is_some() || !receipt.valid(self.policy_revision) {
            return Err(E::PolicyRevisionUnavailable);
        }
        if receipt.action != action || receipt.principal != principal || receipt.expect != expect {
            return Err(E::ProfilePlanInvalid);
        }
        Ok(Some(receipt))
    }

    fn commit(&mut self, path: &Path, receipt: Receipt) -> Result<Value, E> {
        self.root = self.snapshot();
        if self.root.get("operations").is_none() {
            self.root["operations"] = json!([]);
        }
        let operations = self.root["operations"]
            .as_array_mut()
            .ok_or(E::PolicyRevisionUnavailable)?;
        if operations
            .iter()
            .any(|raw| raw["operation_id"] == receipt.operation_id)
        {
            return Err(E::PolicyRevisionUnavailable);
        }
        operations.push(json!(receipt));
        if advance_floor(path, self.policy_revision)
            .and_then(|_| write_private_json(path, &self.snapshot()))
            .is_err()
        {
            self.refuse(super::super::ProfileRefusal::PolicyRevisionUnavailable);
            return Err(E::PolicyRevisionUnavailable);
        }
        Ok(json!(receipt))
    }
}

impl Receipt {
    fn valid(&self, revision: u64) -> bool {
        self.binding.valid(revision) && matches!(self.action.as_str(), "bind" | "revoke")
    }
}

/// Warn about retained unusable operation entries without logging their contents.
pub(super) fn inspect_receipts(root: &Value, revision: u64) {
    let entries = root.get("operations").and_then(Value::as_array);
    if root.get("operations").is_some() && entries.is_none() {
        tracing::warn!("unsupported profile operation collection retained");
    }
    for raw in entries.into_iter().flatten() {
        if !serde_json::from_value::<Receipt>(raw.clone())
            .is_ok_and(|receipt| receipt.valid(revision))
        {
            tracing::warn!("unusable profile operation retained");
        }
    }
}

fn monotonic() -> Result<Duration, E> {
    now(&Kernel).map_err(|_| E::PolicyRevisionUnavailable)
}

fn digest(value: &Value) -> Result<String, E> {
    Ok(
        Sha256::digest(serde_json::to_vec(value).map_err(|_| E::SerializationError)?)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

fn identifier() -> Result<String, E> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| E::InternalError)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

#[cfg(test)]
mod tests;
