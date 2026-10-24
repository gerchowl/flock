//! Private shared content store and structured persistence projection (ADR-0036 §4).
//! No deletion API: shared-root retention and concurrent GC are undecided (#1712).
#![deny(missing_docs)]
use crate::platform::blobs::Directory;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

/// Guard against a supplied clock/sleeper that fails to advance. Normal
/// contention is bounded by elapsed time after backoff, not by attempt count.
const MAX_REPAIR_LOCK_ATTEMPTS: usize = 8;
/// Start with a short sleep, doubling only up to the remaining clock budget.
const REPAIR_LOCK_BACKOFF: Duration = Duration::from_millis(1);
/// Stop retrying contention after this much supplied monotonic time. Individual
/// filesystem operations retain their ordinary I/O failure behavior.
const REPAIR_LOCK_WAIT: Duration = Duration::from_millis(20);

/// Session persistence settings; the threshold is independent of each put's cap.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// Replace eligible JSON blocks strictly larger than this many encoded bytes.
    pub blob_min_bytes: usize,
    /// Silence observation policy in milliseconds, inclusive range 5000..=3600000.
    #[serde(deserialize_with = "stall_ms")]
    pub stall_ms: u64,
}
impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            blob_min_bytes: 2048,
            stall_ms: 120_000,
        }
    }
}
/// Shared validation for the session silence threshold.
pub(crate) fn stall_ms<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let value = u64::deserialize(deserializer)?;
    if !(5_000..=3_600_000).contains(&value) {
        return Err(serde::de::Error::invalid_value(
            serde::de::Unexpected::Unsigned(value),
            &"sessions.stall_ms in 5000..=3600000 milliseconds",
        ));
    }
    Ok(value)
}
/// Explicit failures, retained as the source when transport wraps them in io::Error.
#[derive(Debug)]
pub enum BlobError {
    /// A hash is not 64 lowercase hexadecimal characters.
    InvalidHash,
    /// A namespace, file type, owner, mode or symlink is unsafe.
    UnsafePath,
    /// A valid hash has no stored object.
    Missing,
    /// Stored bytes do not match their address.
    Corrupt,
    /// A put exceeded its caller-supplied byte cap.
    TooLarge,
    /// An underlying read, write, rename or sync failed.
    Io(io::Error),
}
impl std::fmt::Display for BlobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "blob storage: {self:?}")
    }
}
impl std::error::Error for BlobError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}
impl From<io::Error> for BlobError {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            _ if crate::platform::blobs::unsafe_path(&error) => Self::UnsafePath,
            _ => Self::Io(error),
        }
    }
}
/// Reference to exactly the stored bytes; size is in bytes, not characters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    /// Lowercase SHA-256 address.
    pub blob: String,
    /// Length of the complete stored content.
    pub size: u64,
    /// Bounded, lossy UTF-8 preview (up to 80 characters).
    pub preview: String,
}
/// Shared, content-only store. No kind, title or content type is stored here.
#[derive(Debug)]
pub struct BlobStore(Directory);
impl BlobStore {
    /// Open/create `blobs` under an existing trusted data root; pin the namespace.
    pub fn open(data_dir: &Path) -> Result<Self, BlobError> {
        let root = data_dir.canonicalize()?;
        Ok(Self(Directory::open(&root.join("blobs"))?))
    }
    fn check(hash: &str) -> Result<(), BlobError> {
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(BlobError::InvalidHash);
        }
        Ok(())
    }
    fn lookup_error(error: io::Error) -> BlobError {
        if error.kind() == io::ErrorKind::NotFound {
            BlobError::Missing
        } else {
            error.into()
        }
    }
    /// Return the size without reading content. Missing and unsafe entries fail.
    pub fn stat(&self, hash: &str) -> Result<u64, BlobError> {
        Self::check(hash)?;
        Ok(self
            .0
            .file(hash, false)
            .map_err(Self::lookup_error)?
            .metadata()?
            .len())
    }
    /// Read and verify content, distinguishing absence from hash corruption.
    pub fn get(&self, hash: &str) -> Result<Vec<u8>, BlobError> {
        Self::check(hash)?;
        let mut bytes = Vec::new();
        self.0
            .file(hash, false)
            .map_err(Self::lookup_error)?
            .read_to_end(&mut bytes)
            .map_err(BlobError::Io)?;
        if Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
            != hash
        {
            return Err(BlobError::Corrupt);
        }
        Ok(bytes)
    }
    /// Hash precisely the bytes read and written, atomically publishing only a
    /// complete, synced object. Cap failures clean up the private temporary file.
    /// Only corrupt-object repair attempts advisory serialization and rechecks
    /// the current bytes before rename. Missing objects publish without locking:
    /// racing writers have identical verified bytes and emit no repair event.
    /// On contention the supplied clock sleeps for 1 ms, then doubles the
    /// backoff, clipping every request to the remaining 20 ms budget. Eight
    /// attempts guard only against a clock/sleeper that fails to advance.
    /// Clock expiry warns with TimedOut; attempt exhaustion warns with WouldBlock.
    /// Other lock errors warn immediately. Each warns once and falls back to
    /// unlocked publication; duplicate diagnostics are then possible, but locking
    /// never loses content. Sleep requests are bounded, not OS scheduling or I/O.
    /// Non-cooperating filesystem mutations are outside this advisory lock.
    /// Read/write/rename/sync failures remain explicit; reads report corruption.
    pub fn put(&self, reader: impl Read, max_bytes: u64) -> Result<BlobRef, BlobError> {
        self.put_with_repair_lock(
            reader,
            max_bytes,
            &mut Instant::now,
            &mut std::thread::sleep,
            &mut || self.0.try_publication_lock(),
        )
    }
    // Keep time and lock acquisition injectable for filesystem failure tests.
    fn put_with_repair_lock(
        &self,
        mut reader: impl Read,
        max_bytes: u64,
        now: &mut impl FnMut() -> Instant,
        sleep: &mut impl FnMut(Duration),
        lock: &mut impl FnMut() -> io::Result<File>,
    ) -> Result<BlobRef, BlobError> {
        let mut entropy = [0; 16];
        getrandom::fill(&mut entropy).map_err(io::Error::other)?;
        let name = format!(".tmp-{:x}", u128::from_ne_bytes(entropy));
        let mut file = self.0.file(&name, true)?;
        let result = (|| {
            let mut hash = Sha256::new();
            let mut size = 0_u64;
            let mut preview = Vec::new();
            let mut buffer = [0; 8192];
            loop {
                let n = reader.read(&mut buffer).map_err(BlobError::Io)?;
                if n == 0 {
                    break;
                }
                if n as u64 > max_bytes.saturating_sub(size) {
                    return Err(BlobError::TooLarge);
                }
                file.write_all(&buffer[..n]).map_err(BlobError::Io)?;
                hash.update(&buffer[..n]);
                size += n as u64;
                preview
                    .extend_from_slice(&buffer[..n.min(320_usize.saturating_sub(preview.len()))]);
            }
            file.sync_all().map_err(BlobError::Io)?;
            let blob = hash
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let reference = BlobRef {
                blob,
                size,
                preview: String::from_utf8_lossy(&preview).chars().take(80).collect(),
            };
            match self.get(&reference.blob) {
                Ok(_) => self.0.sync()?,
                Err(BlobError::Missing) => self.publish(&name, &reference.blob)?,
                Err(BlobError::Corrupt) => {
                    #[cfg(test)]
                    tests::corrupt_observed();
                    let _publication = match Self::repair_lock(now, sleep, lock) {
                        Ok(guard) => Some(guard),
                        Err(error) => {
                            crate::logging::structured_blob_repair_lock_unavailable(
                                &reference,
                                error.kind(),
                            );
                            None
                        }
                    };
                    // Recheck even on fallback: another writer may have repaired it.
                    match self.get(&reference.blob) {
                        Ok(_) => self.0.sync()?,
                        Err(BlobError::Missing) => self.publish(&name, &reference.blob)?,
                        Err(BlobError::Corrupt) => {
                            self.publish(&name, &reference.blob)?;
                            crate::logging::structured_blob_repaired(&reference);
                        }
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            }
            Ok(reference)
        })();
        // A successful rename consumed the name. Every other path removes it.
        match self.0.remove(&name) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(BlobError::Io(error)),
        }
        result
    }
    /// A bounded, best-effort wait using the caller's matching now/sleep pair.
    /// Only contention backs off; unsupported locking immediately falls back.
    fn repair_lock(
        now: &mut impl FnMut() -> Instant,
        sleep: &mut impl FnMut(Duration),
        lock: &mut impl FnMut() -> io::Result<File>,
    ) -> io::Result<File> {
        let started = now();
        let mut backoff = REPAIR_LOCK_BACKOFF;
        for attempt in 0..MAX_REPAIR_LOCK_ATTEMPTS {
            match lock() {
                Ok(guard) => return Ok(guard),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => (),
                Err(error) => return Err(error),
            }
            let elapsed = now().saturating_duration_since(started);
            let remaining = REPAIR_LOCK_WAIT.saturating_sub(elapsed);
            if remaining.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            if attempt + 1 == MAX_REPAIR_LOCK_ATTEMPTS {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            sleep(backoff.min(remaining));
            if now().saturating_duration_since(started) >= REPAIR_LOCK_WAIT {
                return Err(io::ErrorKind::TimedOut.into());
            }
            backoff = backoff.saturating_mul(2).min(REPAIR_LOCK_WAIT);
        }
        Err(io::ErrorKind::WouldBlock.into())
    }
    fn publish(&self, temporary: &str, blob: &str) -> Result<(), BlobError> {
        #[cfg(test)]
        tests::before_publish();
        self.0.rename(temporary, blob)?;
        Ok(())
    }
    /// Project only complete tool-result, attachment, image/document and thinking
    /// blocks. Small/unrelated lines preserve exact spelling. Partial SSE events
    /// return None and must never consume durable stream sequence numbers.
    pub fn project(&self, raw: &str, threshold: usize) -> Result<Option<String>, BlobError> {
        let Ok(mut value) = serde_json::from_str::<Value>(raw) else {
            return Ok(Some(raw.into()));
        };
        if value.get("type").and_then(Value::as_str) == Some("stream_event") {
            return Ok(None);
        }
        // Only complete harness message blocks carry this storage meaning.
        // Tool inputs and unknown diagnostic payloads remain opaque.
        if !matches!(
            value.get("type").and_then(Value::as_str),
            Some("assistant" | "user")
        ) {
            return Ok(Some(raw.into()));
        }
        let mut changed = false;
        if let Some(blocks) = value
            .pointer_mut("/message/content")
            .and_then(Value::as_array_mut)
        {
            for block in blocks {
                if matches!(
                    block.get("type").and_then(Value::as_str),
                    Some("tool_result" | "attachment" | "image" | "document" | "thinking")
                ) {
                    let bytes = serde_json::to_vec(block).map_err(io::Error::other)?;
                    if bytes.len() > threshold {
                        *block =
                            serde_json::to_value(self.put(bytes.as_slice(), bytes.len() as u64)?)
                                .map_err(io::Error::other)?;
                        changed = true;
                    }
                }
            }
        }
        Ok(Some(if changed {
            value.to_string()
        } else {
            raw.into()
        }))
    }
}

#[cfg(test)]
mod tests;
