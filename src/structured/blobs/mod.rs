//! Private shared content store and structured persistence projection (ADR-0036 §4).
//! No deletion API: shared-root retention and concurrent GC are undecided (#1712).
#![deny(missing_docs)]
use crate::platform::blobs::Directory;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};
use std::path::Path;

/// Session persistence settings; the threshold is independent of each put's cap.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// Replace eligible JSON blocks strictly larger than this many encoded bytes.
    pub blob_min_bytes: usize,
}
impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            blob_min_bytes: 2048,
        }
    }
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
            io::ErrorKind::NotFound => Self::Missing,
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
    /// Return the size without reading content. Missing and unsafe entries fail.
    pub fn stat(&self, hash: &str) -> Result<u64, BlobError> {
        Self::check(hash)?;
        Ok(self.0.file(hash, false)?.metadata()?.len())
    }
    /// Read and verify content, distinguishing absence from hash corruption.
    pub fn get(&self, hash: &str) -> Result<Vec<u8>, BlobError> {
        Self::check(hash)?;
        let mut bytes = Vec::new();
        self.0.file(hash, false)?.read_to_end(&mut bytes)?;
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
    /// Concurrent identical puts may replace an identical inode, never partial data.
    pub fn put(&self, mut reader: impl Read, max_bytes: u64) -> Result<BlobRef, BlobError> {
        let mut entropy = [0; 16];
        getrandom::fill(&mut entropy).map_err(io::Error::other)?;
        let name = format!(".tmp-{:x}", u128::from_ne_bytes(entropy));
        let result = (|| {
            let mut file = self.0.file(&name, true)?;
            let mut hash = Sha256::new();
            let mut size = 0_u64;
            let mut preview = Vec::new();
            let mut buffer = [0; 8192];
            loop {
                let n = reader.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                if n as u64 > max_bytes.saturating_sub(size) {
                    return Err(BlobError::TooLarge);
                }
                file.write_all(&buffer[..n])?;
                hash.update(&buffer[..n]);
                size += n as u64;
                preview
                    .extend_from_slice(&buffer[..n.min(320_usize.saturating_sub(preview.len()))]);
            }
            file.sync_all()?;
            let blob = hash
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            match self.get(&blob) {
                Ok(_) => (),
                Err(BlobError::Missing) => self.0.rename(&name, &blob)?,
                Err(error) => return Err(error),
            }
            Ok(BlobRef {
                blob,
                size,
                preview: String::from_utf8_lossy(&preview).chars().take(80).collect(),
            })
        })();
        // A successful rename consumed the name. Every other path removes it.
        match self.0.remove(&name) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(BlobError::Io(error)),
        }
        result
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
        fn visit(
            store: &BlobStore,
            value: &mut Value,
            threshold: usize,
        ) -> Result<bool, BlobError> {
            if matches!(
                value.get("type").and_then(Value::as_str),
                Some("tool_result" | "attachment" | "image" | "document" | "thinking")
            ) {
                let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
                if bytes.len() > threshold {
                    *value = serde_json::to_value(store.put(bytes.as_slice(), bytes.len() as u64)?)
                        .map_err(io::Error::other)?;
                    return Ok(true);
                }
            }
            let mut changed = false;
            match value {
                Value::Object(map) => {
                    for child in map.values_mut() {
                        changed |= visit(store, child, threshold)?;
                    }
                }
                Value::Array(items) => {
                    for child in items {
                        changed |= visit(store, child, threshold)?;
                    }
                }
                _ => (),
            }
            Ok(changed)
        }
        if visit(self, &mut value, threshold)? {
            Ok(Some(value.to_string()))
        } else {
            Ok(Some(raw.into()))
        }
    }
}
