//! Inactive operator binding data and durable storage (ADR-0052 §1–2).

pub(crate) mod transactions;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

/// Explicit target identities, never an implicit fleet wildcard.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Scope {
    /// Bound repository identity.
    pub repo_id: String,
    /// Authorized node identities.
    pub node_ids: Vec<String>,
    /// Authorized agent identities.
    pub agent_ids: Vec<String>,
}

/// Durable, terminal revocation evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Revocation {
    /// Operator-supplied reason.
    pub reason: String,
    /// Authenticated operator identity.
    pub principal: String,
    /// Idempotent mutation identity.
    pub operation_id: String,
    /// Global policy revision that revoked the binding.
    pub revision: u64,
}

/// Data-only Claude binding view, containing references rather than credentials.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BindingView {
    /// Stable operator binding identity.
    pub binding_id: String,
    /// Explicit authorized targets.
    pub target: Scope,
    /// Presentation alias, never a grant.
    pub account_alias: String,
    /// Opaque setup-token reference, never opened by this module.
    pub credential_ref: String,
    /// Authorized credential generation.
    pub credential_generation: u64,
    /// Requested immutable content identities; verification belongs to later slices.
    pub bundle_digests: Vec<String>,
    /// Operator-issued profile-selection grant identifiers.
    pub grants: Vec<String>,
    /// Global policy revision at the last change.
    pub revision: u64,
    /// Terminal revocation, retained across restarts.
    pub revoked: Option<Revocation>,
}

fn explicit(s: &str) -> bool {
    !s.trim().is_empty() && !s.contains('*')
}

impl BindingView {
    fn valid(&self, revision: u64) -> bool {
        [
            &self.binding_id,
            &self.target.repo_id,
            &self.account_alias,
            &self.credential_ref,
        ]
        .into_iter()
        .all(|s| explicit(s))
            && [
                &self.target.node_ids,
                &self.target.agent_ids,
                &self.bundle_digests,
            ]
            .into_iter()
            .all(|v| !v.is_empty() && v.iter().all(|s| explicit(s)))
            && self.grants.iter().all(|s| explicit(s))
            && self.credential_generation > 0
            && self.revision > 0
            && self.revision <= revision
            && self.revoked.as_ref().is_none_or(|r| {
                r.revision == self.revision
                    && [&r.reason, &r.principal, &r.operation_id]
                        .into_iter()
                        .all(|s| explicit(s))
            })
    }
}

/// Server-owned persisted policy. Unknown data is retained without conferring authority.
#[derive(Debug, Default)]
pub(crate) struct BindingStore {
    /// Global monotonic revision, serialized by the operator authority.
    pub policy_revision: u64,
    /// Default-false production switch, still constrained by build readiness.
    pub execution_switch: bool,
    bindings: Vec<BindingView>,
    unknown: Vec<Value>,
    root: Value,
    opaque: bool,
    fault: Option<super::ProfileRefusal>,
}

impl BindingStore {
    /// Load operator state without consulting repository or agent configuration.
    /// Older snapshots cannot lower a revision already observed by this authority.
    pub fn load(&mut self, path: &Path) {
        if Self::transaction(path, || {
            self.load_locked(path);
            Ok(())
        })
        .is_err()
        {
            self.refuse(super::ProfileRefusal::PolicyRevisionUnavailable);
        }
    }

    fn load_locked(&mut self, path: &Path) {
        let floor = match floor_revision(path) {
            Ok(floor) => self.policy_revision.max(floor.unwrap_or_default()),
            Err(_) => {
                self.refuse(super::ProfileRefusal::PolicyRevisionUnavailable);
                return;
            }
        };
        self.policy_revision = floor;
        let loaded = read_policy_json(path);
        let raw = match loaded {
            Ok(raw) => raw,
            Err(e) if e.kind() == io::ErrorKind::NotFound && floor == 0 => return,
            Err(_) => {
                self.refuse(super::ProfileRefusal::PolicyRevisionUnavailable);
                return;
            }
        };
        let Some(revision) = raw.get("policy_revision").and_then(Value::as_u64) else {
            self.refuse(super::ProfileRefusal::PolicyRevisionUnavailable);
            return;
        };
        if revision < floor {
            tracing::warn!(
                event = "policy_revision_regressed",
                store = revision,
                floor,
                "profile policy rollback refused"
            );
            self.refuse(super::ProfileRefusal::PolicyRevisionRegressed {
                store: revision,
                floor,
            });
            return;
        }
        if advance_floor(path, revision).is_err() {
            self.refuse(super::ProfileRefusal::PolicyRevisionUnavailable);
            return;
        }
        self.fault = None;
        self.decode(raw);
    }

    fn refuse(&mut self, fault: super::ProfileRefusal) {
        self.fault = Some(fault);
        self.opaque = true;
        self.execution_switch = false;
        self.bindings.clear();
        tracing::warn!("profile policy unavailable; bindings withheld");
    }

    /// A retained policy fault precedes switch and build-readiness checks.
    pub(super) fn refusal(&self) -> Option<super::ProfileRefusal> {
        self.fault.or(self
            .opaque
            .then_some(super::ProfileRefusal::PolicyRevisionUnavailable))
    }

    fn decode(&mut self, raw: Value) {
        let revision = raw.get("policy_revision").and_then(Value::as_u64);
        if revision.is_some_and(|r| r < self.policy_revision) {
            tracing::warn!("older profile policy revision refused");
            return;
        }
        let supported = raw.get("schema") == Some(&json!(1))
            && revision.is_some()
            && raw.get("bindings").is_some_and(Value::is_array)
            && raw.get("unknown").is_none_or(Value::is_array)
            && raw.get("execution_switch").is_none_or(Value::is_boolean);
        if !supported {
            self.policy_revision = self.policy_revision.max(revision.unwrap_or_default());
            self.execution_switch = false;
            self.bindings.clear();
            self.root = raw;
            self.opaque = true;
            tracing::warn!("unsupported profile binding store retained");
            return;
        }
        self.policy_revision = revision.unwrap_or_default();
        self.execution_switch = raw
            .get("execution_switch")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.bindings.clear();
        self.unknown = raw
            .get("unknown")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let entries = raw["bindings"].as_array().cloned().unwrap_or_default();
        let mut seen = BTreeSet::new();
        let conflicts: BTreeSet<_> = entries
            .iter()
            .filter_map(|v| v["binding_id"].as_str())
            .filter(|id| !seen.insert(*id))
            .collect();
        for entry in &entries {
            match serde_json::from_value::<BindingView>(entry.clone()) {
                Ok(b)
                    if b.valid(self.policy_revision)
                        && !conflicts.contains(b.binding_id.as_str()) =>
                {
                    self.bindings.push(b)
                }
                _ => {
                    tracing::warn!("unusable profile binding retained");
                    self.unknown.push(entry.clone());
                }
            }
        }
        self.root = raw;
        transactions::inspect_receipts(&self.root, self.policy_revision);
        self.opaque = false;
    }

    /// Persist with private temp-file fsync, atomic rename and parent-directory fsync.
    /// Unsupported envelopes are never overwritten; higher disk revisions refuse.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        self.save_with_store_write(path, write_private_json)
    }

    // The store writer is injectable so tests can fail exactly after the floor commit.
    fn save_with_store_write(
        &self,
        path: &Path,
        write: impl FnOnce(&Path, &Value) -> io::Result<()>,
    ) -> io::Result<()> {
        Self::transaction(path, || {
            let mut disk = Self::default();
            disk.load_locked(path);
            if self.refusal().is_some()
                || disk.refusal().is_some()
                || disk.policy_revision > self.policy_revision
            {
                return Err(io::Error::other("profile policy persistence refused"));
            }
            advance_floor(path, self.policy_revision)?;
            write(path, &self.snapshot())
        })
    }

    // All authority observations and effects, including future revision allocation,
    // belong inside this section. Never delete or replace the stable lock file.
    pub(super) fn transaction<T>(
        path: &Path,
        apply: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        static OWNER: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _owner = OWNER.try_lock().map_err(|_| {
            io::Error::new(io::ErrorKind::WouldBlock, "profile policy transaction busy")
        })?;
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("missing state directory"))?;
        fs::create_dir_all(parent)?;
        let file = crate::platform::profile_bindings::open_owned_regular(
            &path.with_extension("lock"),
            true,
        )?;
        let lock = crate::platform::file_lock::LockFile::new(file);
        lock.try_lock().map_err(|error| match error {
            fs::TryLockError::WouldBlock => io::ErrorKind::WouldBlock.into(),
            fs::TryLockError::Error(error) => error,
        })?;
        apply()
    }

    fn snapshot(&self) -> Value {
        let mut root = if self.root.is_object() {
            self.root.clone()
        } else {
            json!({})
        };
        root["schema"] = json!(1);
        root["policy_revision"] = json!(self.policy_revision);
        root["execution_switch"] = json!(self.execution_switch);
        root["bindings"] = json!(self.bindings);
        root["unknown"] = json!(self.unknown);
        root
    }

    /// Resolve explicit target identities using in-memory data alone.
    /// Revoked, missing and ambiguous matches all withhold authority.
    pub fn resolve(&self, repo: &str, node: &str, agent: &str) -> Option<BindingView> {
        if self.refusal().is_some() {
            return None;
        }
        let mut matches = self.bindings.iter().filter(|b| {
            b.revoked.is_none()
                && b.target.repo_id == repo
                && b.target.node_ids.iter().any(|n| n == node)
                && b.target.agent_ids.iter().any(|a| a == agent)
        });
        let binding = matches.next()?;
        matches.next().is_none().then(|| binding.clone())
    }

    /// Project reviewed binding data only to the authenticated local operator.
    /// No account usage exists in this store; others receive no authority metadata.
    pub fn status(&self, operator: bool) -> Value {
        if !operator {
            return json!({});
        }
        let mut status = json!({"policy_revision":self.policy_revision,"execution_switch":self.execution_switch,"bindings":self.bindings});
        if let Some(fault) = self.refusal() {
            status["policy_error"] = json!(fault);
        }
        status
    }
}

fn read_policy_json(path: &Path) -> io::Result<Value> {
    serde_json::from_reader(crate::platform::profile_bindings::open_owned_regular(
        path, false,
    )?)
    .map_err(io::Error::other)
}

fn floor_revision(path: &Path) -> io::Result<Option<u64>> {
    match read_policy_json(&path.with_extension("floor.json")) {
        Ok(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| io::Error::other("invalid profile revision floor")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

// The transaction owns both locks throughout this read/compare/write.
fn advance_floor(path: &Path, revision: u64) -> io::Result<()> {
    let current = floor_revision(path)?;
    let next = current.unwrap_or_default().max(revision);
    if current == Some(next) {
        return Ok(());
    }
    write_private_json(&path.with_extension("floor.json"), &json!(next))
}

pub(super) fn write_private_json(path: &Path, value: &Value) -> io::Result<()> {
    match crate::platform::profile_bindings::open_owned_regular(path, false) {
        Ok(_) => (),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error),
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing state directory"))?;
    fs::create_dir_all(parent)?;
    let mut entropy = [0; 16];
    getrandom::fill(&mut entropy).map_err(io::Error::other)?;
    let temp = parent.join(format!(
        ".profile-bindings-{:032x}.tmp",
        u128::from_le_bytes(entropy)
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let result = (|| {
        serde_json::to_writer(&mut file, value)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        fs::File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests;
