//! No-follow, owner-checked descriptors for operator profile policy and its lock.

use std::{
    fs::{File, Metadata, OpenOptions},
    io,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

/// Open existing policy data or create its stable advisory lock, without following
/// the final symlink component or blocking on a special file. Validate the opened
/// inode before callers can read policy bytes or acquire authority through it.
pub(crate) fn open_owned_regular(path: &Path, create: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    validate(&file.metadata()?, super::peer::effective_uid())?;
    Ok(file)
}

fn validate(metadata: &Metadata, owner: u32) -> io::Result<()> {
    if !metadata.is_file() || metadata.uid() != owner {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "profile policy requires an owned regular file",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_requires_regular_file_and_matching_owner() {
        let root = crate::test_support::unique_temp_path("profile-owned");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("policy.json");
        std::fs::write(&path, b"7").unwrap();
        let file = open_owned_regular(&path, false).unwrap();
        let metadata = file.metadata().unwrap();
        assert!(validate(&metadata, metadata.uid().wrapping_add(1)).is_err());
        assert!(open_owned_regular(&root, false).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn no_follow_rejects_even_a_dangling_final_symlink() {
        let root = crate::test_support::unique_temp_path("profile-link");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("policy.json");
        std::os::unix::fs::symlink(root.join("absent"), &path).unwrap();
        assert!(open_owned_regular(&path, false).is_err());
        assert!(open_owned_regular(&path, true).is_err());
        assert!(!root.join("absent").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
