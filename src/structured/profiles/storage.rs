//! Inactive native storage preparation. Published roots are retained as evidence.
use super::{
    bindings::{write_private_json, BindingStore},
    ProfileAdmission,
};
use crate::{
    api::error_code::ErrorCode as E,
    platform::profile_bindings::private_directory,
    structured::{queue::admission::canonical_uuid, registry::BindingIdentity},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug, Default, Serialize)]
pub(super) struct Status {
    sessions: Vec<Allocation>,
    retained_unknown: usize,
    quarantined: Vec<String>,
    unavailable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Allocation {
    schema: u32,
    session_id: String,
    repo_id: String,
    node_id: String,
    agent_id: String,
    binding: BindingIdentity,
}

impl Allocation {
    fn valid(&self) -> bool {
        self.schema == 1
            && canonical_uuid(&self.session_id)
            && !self.binding.binding_id.trim().is_empty()
            && self.binding.revision > 0
            && [&self.repo_id, &self.node_id, &self.agent_id]
                .into_iter()
                .all(|s| !s.trim().is_empty())
    }
}

impl ProfileAdmission {
    /// Prepare private native storage against current authority and an exact pin.
    /// No credential access, launch or registry/mailbox transfer is performed.
    // Slice 4 consumes this preparation seam; production admission remains separate.
    #[allow(dead_code)]
    pub(crate) fn allocate_session(
        &mut self,
        session_id: &str,
        repo: &str,
        node: &str,
        agent: &str,
        pin: &BindingIdentity,
    ) -> Result<Value, E> {
        if self.path.is_none() {
            return Err(E::PolicyRevisionUnavailable);
        }
        let requested = Allocation {
            schema: 1,
            session_id: session_id.into(),
            repo_id: repo.into(),
            node_id: node.into(),
            agent_id: agent.into(),
            binding: pin.clone(),
        };
        if !requested.valid() {
            return Err(E::InvalidParams);
        }
        let result = self.edit(|disk, path| {
            let binding = disk
                .resolve(repo, node, agent)
                .ok_or(E::ProfileBindingNotFound)?;
            if binding.binding_id != pin.binding_id || binding.revision != pin.revision {
                return Err(E::StructuredProfileMismatch);
            }
            let root = allocate(path, &requested).map_err(|_| E::InternalError)?;
            Ok(json!({"session_id":session_id,"binding":pin,"native_root":root.join("config")}))
        });
        if let (Some(path), Ok(_)) = (&self.path, &result) {
            self.storage = load(path, false);
        }
        result
    }
}

pub(super) fn load(policy: &Path, recover: bool) -> Status {
    match BindingStore::transaction(policy, || scan(policy, recover)) {
        Ok(status) => status,
        Err(_) => {
            tracing::warn!(
                event = "profile_session_storage_unavailable",
                "native session storage unavailable"
            );
            Status {
                unavailable: true,
                ..Status::default()
            }
        }
    }
}

fn base(policy: &Path) -> io::Result<PathBuf> {
    Ok(policy
        .parent()
        .ok_or_else(|| io::Error::other("missing server state"))?
        .canonicalize()?
        .join("profile-storage"))
}

fn read(root: &Path) -> io::Result<Allocation> {
    private_directory(root, false)?;
    private_directory(&root.join("config"), false)?;
    let file =
        crate::platform::profile_bindings::open_owned_regular(&root.join("record.json"), false)?;
    let record: Allocation = serde_json::from_reader(file).map_err(io::Error::other)?;
    if !record.valid() || root.file_name().and_then(|s| s.to_str()) != Some(&record.session_id) {
        return Err(io::Error::other("invalid session allocation"));
    }
    Ok(record)
}

fn scan(policy: &Path, recover: bool) -> io::Result<Status> {
    let base = base(policy)?;
    match private_directory(&base, false) {
        Ok(_) => (),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Status::default()),
        Err(e) => return Err(e),
    }
    let roots = base.join("sessions");
    let directory = private_directory(&roots, false)?;
    let mut status = Status::default();
    for entry in fs::read_dir(&roots)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if recover && name.strip_prefix(".alloc-").is_some_and(canonical_uuid) {
            let quarantine = base.join("quarantine");
            let dest = private_directory(&quarantine, true)?;
            fs::File::open(&base)?.sync_all()?;
            // A fresh name preserves earlier crash evidence rather than replacing it.
            let destination = quarantine.join(format!("{}-{}", name, nonce()?));
            fs::rename(entry.path(), destination)?;
            dest.sync_all()?;
            directory.sync_all()?;
            fs::File::open(&base)?.sync_all()?;
            tracing::warn!(
                event = "profile_session_staging_quarantined",
                "unpublished native session staging quarantined"
            );
        } else {
            match read(&entry.path()) {
                Ok(record) => status.sessions.push(record),
                Err(_) => {
                    status.retained_unknown += 1;
                    tracing::warn!(
                        event = "profile_session_record_retained",
                        "unusable native session record retained inactive"
                    );
                }
            }
        }
    }
    let quarantine = base.join("quarantine");
    match private_directory(&quarantine, false) {
        Ok(_) => {
            for entry in fs::read_dir(quarantine)? {
                status
                    .quarantined
                    .push(entry?.file_name().to_string_lossy().into_owned());
            }
            status.quarantined.sort();
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
        Err(e) => return Err(e),
    }
    status
        .sessions
        .sort_by(|a, b| a.session_id.cmp(&b.session_id));
    Ok(status)
}

fn nonce() -> io::Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(format!("{:032x}", u128::from_le_bytes(bytes)))
}

fn allocate(policy: &Path, requested: &Allocation) -> io::Result<PathBuf> {
    allocate_with(policy, requested, || Ok(()))
}

fn allocate_with(
    policy: &Path,
    requested: &Allocation,
    before_publish: impl FnOnce() -> io::Result<()>,
) -> io::Result<PathBuf> {
    let base = base(policy)?;
    let base_dir = private_directory(&base, true)?;
    let roots = base.join("sessions");
    let roots_dir = private_directory(&roots, true)?;
    base_dir.sync_all()?;
    fs::File::open(
        base.parent()
            .ok_or_else(|| io::Error::other("missing state"))?,
    )?
    .sync_all()?;
    let root = roots.join(&requested.session_id);
    match fs::symlink_metadata(&root) {
        Ok(_) => {
            return if read(&root)? == *requested {
                roots_dir.sync_all()?;
                Ok(root)
            } else {
                Err(io::Error::other("session identity mismatch"))
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
        Err(e) => return Err(e),
    }
    let staging = roots.join(format!(".alloc-{}", requested.session_id));
    // create_dir is exclusive: an earlier call's staging never belongs to this call.
    crate::platform::structured::create_private_dir(&staging)?;
    let result = (|| {
        private_directory(&staging.join("config"), true)?.sync_all()?;
        write_private_json(&staging.join("record.json"), &json!(requested))?;
        private_directory(&staging, false)?.sync_all()?;
        before_publish()?;
        fs::rename(&staging, &root)?;
        // A sync failure after rename retains the complete, published evidence.
        roots_dir.sync_all()?;
        Ok(root)
    })();
    if result.is_err()
        && staging.exists()
        && fs::remove_dir_all(&staging)
            .and_then(|()| roots_dir.sync_all())
            .is_err()
    {
        tracing::warn!(
            event = "profile_session_staging_rollback_failed",
            "unpublished staging retained for recovery"
        );
    }
    result
}

#[cfg(test)]
mod tests;
