use std::{
    fs::File,
    io::{self, Read},
    path::Path,
};

use sha2::{Digest, Sha256};

use serde::{Deserialize, Deserializer};

/// Stable and preview manifest entry, shared by local and remote installers.
/// Legacy URL strings have no checksum during the transition tracked by #2076.
#[derive(Debug, Clone)]
pub(crate) struct AssetRef {
    pub(crate) url: String,
    pub(crate) sha256: Option<String>,
}

impl<'de> Deserialize<'de> for AssetRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::String(url) if !url.trim().is_empty() => Ok(Self {
                url: url.trim().to_string(),
                sha256: None,
            }),
            serde_json::Value::Object(mut object) => {
                let url = object
                    .remove("url")
                    .and_then(|value| value.as_str().map(str::to_string))
                    .ok_or_else(|| serde::de::Error::custom("asset object is missing url"))?;
                let sha256 = object
                    .remove("sha256")
                    .filter(|value| !value.is_null())
                    .map(|value| {
                        value.as_str().map(str::to_string).ok_or_else(|| {
                            serde::de::Error::custom("asset sha256 must be a string")
                        })
                    })
                    .transpose()?;
                if url.trim().is_empty() {
                    return Err(serde::de::Error::custom("asset url must not be empty"));
                }
                Ok(Self {
                    url: url.trim().to_string(),
                    sha256,
                })
            }
            _ => Err(serde::de::Error::custom(
                "asset must be a URL string or object with url",
            )),
        }
    }
}

impl AssetRef {
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    pub(crate) fn sha256(&self) -> Option<&str> {
        self.sha256.as_deref()
    }
}

/// Verify a declared checksum before an installer can consume the download.
/// Missing checksums remain accepted for legacy manifests until #2076.
/// Malformed or mismatched checksums return `io::ErrorKind::InvalidData`.
pub(crate) fn verify_asset(path: &Path, expected: Option<&str>) -> io::Result<()> {
    match expected {
        Some(expected) => verify_sha256(path, expected),
        None => Ok(()),
    }
}

pub(crate) fn verify_sha256(path: &Path, expected: &str) -> io::Result<()> {
    let expected = expected.trim().to_ascii_lowercase();
    if expected.len() != 64 || !expected.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected sha256 must be 64 hexadecimal characters",
        ));
    }

    let actual = file_sha256(path)?;
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("sha256 mismatch: expected {expected}, got {actual}"),
        ));
    }
    Ok(())
}

/// Hash a local transfer source so the remote can verify the received bytes.
pub(crate) fn file_sha256(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(to_lower_hex(&hasher.finalize()))
}

fn to_lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use std::fs;

    #[test]
    fn asset_shapes_keep_legacy_and_verified_downloads_distinct() {
        let legacy: super::AssetRef =
            serde_json::from_str(r#""https://example.invalid/asset""#).unwrap();
        assert_eq!(legacy.url(), "https://example.invalid/asset");
        assert_eq!(legacy.sha256(), None);
        let fixture = crate::test_support::TempDir::socket();
        let path = fixture.path().join("asset");
        fs::write(&path, b"flock").unwrap();
        assert!(super::verify_asset(&path, legacy.sha256()).is_ok());
        let verified: super::AssetRef = serde_json::from_value(serde_json::json!({
            "url": legacy.url(), "sha256": "a".repeat(64)
        }))
        .unwrap();
        let error = super::verify_asset(&path, verified.sha256()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("sha256 mismatch"));
    }

    #[test]
    fn present_invalid_checksum_never_becomes_a_legacy_download() {
        let fixture = crate::test_support::TempDir::socket();
        let path = fixture.path().join("asset");
        fs::write(&path, b"flock").unwrap();
        for digest in ["", "wrong"] {
            let asset: super::AssetRef = serde_json::from_value(serde_json::json!({
                "url":"https://example.invalid/asset", "sha256":digest
            }))
            .unwrap();
            assert_eq!(
                super::verify_asset(&path, asset.sha256())
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::InvalidData
            );
        }
        assert!(
            serde_json::from_value::<super::AssetRef>(serde_json::json!({
                "url":"https://example.invalid/asset", "sha256":42
            }))
            .is_err()
        );
    }

    #[test]
    fn null_checksum_is_absent_and_remains_unverified() {
        let asset: super::AssetRef = serde_json::from_value(serde_json::json!({
            "url":"https://example.invalid/asset", "sha256":null
        }))
        .unwrap();
        assert_eq!(asset.sha256(), None);
        let root = crate::test_support::TempDir::socket();
        assert!(super::verify_asset(&root.path().join("not-downloaded"), asset.sha256()).is_ok());
    }

    #[test]
    fn verifies_matching_sha256() {
        let path = std::env::temp_dir().join(format!("flock-checksum-test-{}", std::process::id()));
        fs::write(&path, b"flock").unwrap();
        let result = super::verify_sha256(
            &path,
            "8a1eb3d0b3d5c74a5ef77a096f02241389061970fdf83cd7bb737c0230cd9a48",
        );
        let _ = fs::remove_file(&path);
        assert!(result.is_ok());
    }
}
