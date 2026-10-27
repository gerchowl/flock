//! Cross-process revocation intent, separate from the admission lock.
use super::*;
/// The locked entry is published before waiting for admission; a crash unlocks it.
/// Synchronous management borrows the namespace; worker handoffs retain its Arc.
pub(super) struct Intent<N: std::ops::Deref<Target = Namespace>> {
    namespace: N,
    name: String,
    file: LockFile,
}
impl<N: std::ops::Deref<Target = Namespace>> Intent<N> {
    pub(super) fn begin(namespace: N, nonce: String) -> Result<Self, TokenError> {
        let staged = format!(".intent-{nonce}");
        let file = LockFile::new(namespace.file(&staged, true)?);
        let mut intent = Self {
            namespace,
            name: staged,
            file,
        };
        intent.file.try_lock().map_err(|_| TokenError::Busy)?;
        let published = format!(".revoke-{nonce}");
        intent.namespace.publish(&intent.name, &published, false)?;
        intent.namespace.remove(&intent.name)?;
        intent.name = published;
        Ok(intent)
    }
}
impl<N: std::ops::Deref<Target = Namespace>> Drop for Intent<N> {
    fn drop(&mut self) {
        if self.namespace.remove(&self.name).is_err() {
            crate::logging::client_token_entry_unsafe(&hex(self.name.as_bytes()));
        }
    }
}
pub(super) fn pending(namespace: &Namespace) -> Result<bool, TokenError> {
    for entry in namespace.names()? {
        let Some(name) = entry.name.as_deref() else {
            continue;
        };
        if !name
            .strip_prefix(".revoke-")
            .is_some_and(|s| valid_hex(s, 32))
        {
            continue;
        }
        match namespace
            .file(name, false)
            .map(LockFile::new)
            .and_then(|file| match file.try_lock_shared() {
                Ok(()) => Ok(false),
                Err(TryLockError::WouldBlock) => Ok(true),
                Err(TryLockError::Error(error)) => Err(error),
            }) {
            Ok(true) => return Ok(true),
            Ok(false) => (),
            Err(_) => crate::logging::client_token_entry_unsafe(&entry.id),
        }
    }
    namespace.check()?;
    Ok(false)
}

/// Await live revocation intents using owned kernel locks, never a polling delay.
pub(super) fn wait(
    namespace: &Namespace,
    executable: &Path,
    deadline: std::time::Instant,
    mut on_wait: impl FnMut(),
) -> Result<(), TokenError> {
    for entry in namespace.names()? {
        let Some(name) = entry.name.as_deref().filter(|name| {
            name.strip_prefix(".revoke-")
                .is_some_and(|nonce| valid_hex(nonce, 32))
        }) else {
            continue;
        };
        let file = match namespace.file(name, false) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => {
                crate::logging::client_token_entry_unsafe(&entry.id);
                continue;
            }
        };
        let pending = PendingLock::begin_shared(file, executable)?;
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| TokenError::Io(io::ErrorKind::TimedOut.into()))?;
        drop(pending.finish_notifying(remaining, &mut on_wait)?);
    }
    namespace.check().map_err(TokenError::from)
}

/// Reclaim unlocked published intents under the store's exclusive mutation lock.
/// Each removal also owns the intent exclusively, so a newly published live
/// revoker remains protected even if it appeared after the directory listing.
pub(super) fn sweep(namespace: &Namespace) -> Result<(), TokenError> {
    for entry in namespace.names()? {
        let Some(name) = entry.name.as_deref() else {
            continue;
        };
        if !name
            .strip_prefix(".revoke-")
            .is_some_and(|s| valid_hex(s, 32))
        {
            continue;
        }
        if let Err(error) = namespace.remove_unheld_intent(name) {
            if error.kind() != io::ErrorKind::NotFound {
                crate::logging::client_token_entry_unsafe(&entry.id);
                return Err(error.into());
            }
        }
    }
    Ok(())
}
