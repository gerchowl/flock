//! Linux machine-id validation, with injected paths and boot time for portable tests.
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::UNIX_EPOCH;

pub(crate) fn read_machine_id(path: &Path, boot_time: u64) -> io::Result<String> {
    let id = std::fs::read_to_string(path)
        .map_err(|err| unavailable(path, &format!("cannot read machine-id: {err}")))?;
    let id = id.trim();
    if id.is_empty() || id == "uninitialized" {
        return Err(unavailable(path, "machine-id is not initialized"));
    }
    if id.len() != 32
        || !id.bytes().all(|byte| byte.is_ascii_hexdigit())
        || id.bytes().all(|byte| byte == b'0')
    {
        return Err(unavailable(path, "machine-id is invalid"));
    }
    let metadata = std::fs::metadata(path)?;
    let modified = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs();
    // A value generated during this boot has not demonstrated persistence.
    // In particular, impermanent roots can regenerate it on every boot.
    let changed = u64::try_from(metadata.ctime()).map_err(io::Error::other)?;
    if boot_time == 0 || modified.max(changed) >= boot_time {
        return Err(unavailable(path, "machine-id persistence is unverified (created or modified during this boot, or boot time unavailable)"));
    }
    Ok(id.to_ascii_lowercase())
}

fn unavailable(path: &Path, reason: &str) -> io::Error {
    io::Error::other(format!("{}: {reason}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_machine_id_is_unavailable() {
        let path = crate::test_support::unique_temp_path("missing-machine-id");
        assert!(read_machine_id(&path, u64::MAX)
            .unwrap_err()
            .to_string()
            .contains("cannot read machine-id"));
    }

    #[test]
    fn empty_and_uninitialized_machine_ids_are_unavailable() {
        let path = crate::test_support::unique_temp_path("uninitialized-machine-id");
        for contents in [
            "",
            "\n",
            "uninitialized\n",
            "00000000000000000000000000000000",
            "invalid",
        ] {
            std::fs::write(&path, contents).unwrap();
            assert!(read_machine_id(&path, u64::MAX).is_err());
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn boot_regenerated_machine_id_is_unavailable_but_persistent_id_is_usable() {
        let path = crate::test_support::unique_temp_path("boot-machine-id");
        let id = "1234567890abcdef1234567890abcdef";
        std::fs::write(&path, id).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let modified = metadata
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .max(metadata.ctime() as u64);
        assert!(read_machine_id(&path, modified)
            .unwrap_err()
            .to_string()
            .contains("persistence is unverified"));
        assert!(read_machine_id(&path, 0).is_err());
        assert_eq!(read_machine_id(&path, modified + 1).unwrap(), id);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn regenerated_machine_id_with_preserved_mtime_is_still_unavailable() {
        let path = crate::test_support::unique_temp_path("copied-machine-id");
        std::fs::write(&path, "1234567890abcdef1234567890abcdef").unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_times(
            std::fs::FileTimes::new().set_modified(UNIX_EPOCH + std::time::Duration::from_secs(1)),
        )
        .unwrap();
        let changed = std::fs::metadata(&path).unwrap().ctime() as u64;
        assert!(read_machine_id(&path, changed).is_err());
        std::fs::remove_file(path).unwrap();
    }
}
