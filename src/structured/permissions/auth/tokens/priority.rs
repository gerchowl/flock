//! Cross-process revocation intent, separate from the admission lock.
use super::*;
/// The locked entry is published before waiting for admission; a crash unlocks it.
pub(super) struct Intent<'a> {
    namespace: &'a Namespace,
    name: String,
    file: File,
}
impl<'a> Intent<'a> {
    pub(super) fn begin(namespace: &'a Namespace, nonce: String) -> Result<Self, TokenError> {
        let staged = format!(".intent-{nonce}");
        let file = namespace.file(&staged, true)?;
        let mut intent = Self {
            namespace,
            name: staged,
            file,
        };
        intent.file.try_lock().map_err(|_| TokenError::Busy)?;
        let published = format!(".revoke-{nonce}");
        namespace.publish(&intent.name, &published, false)?;
        namespace.remove(&intent.name)?;
        intent.name = published;
        Ok(intent)
    }
}
impl Drop for Intent<'_> {
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
            .and_then(|file| match file.try_lock() {
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
