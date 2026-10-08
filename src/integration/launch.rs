//! Stable executable selection shared by hooks, MCP registration and status.
use std::io;
use std::path::{Path, PathBuf};

pub(crate) fn is_store_path(path: &Path) -> bool {
    path.starts_with("/nix/store")
}

pub(crate) fn stable_launch_path() -> io::Result<PathBuf> {
    let current = std::env::current_exe()?;
    let invocation = std::env::args_os().next().map(PathBuf::from).map(|path| {
        if path.is_relative() && path.components().count() > 1 {
            std::env::current_dir()
                .map(|dir| dir.join(&path))
                .unwrap_or(path)
        } else {
            path
        }
    });
    let paths = std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_default();
    select_launch_path(invocation.as_deref(), &current, &paths)
}

pub(super) fn select_launch_path(
    invocation: Option<&Path>,
    current: &Path,
    paths: &[PathBuf],
) -> io::Result<PathBuf> {
    // Keep the profile symlink itself: resolving it would pin the old image.
    if let Some(path) = invocation.filter(|path| path.is_absolute()) {
        if !is_store_path(path) && super::executable_file_exists(path) {
            return Ok(path.to_owned());
        }
    }
    if !is_store_path(current)
        && invocation.is_none_or(|path| path.is_absolute() || path.components().count() > 1)
    {
        return Ok(current.to_owned());
    }
    for dir in paths.iter().filter(|dir| dir.is_absolute()) {
        let path = dir.join("flk");
        if !is_store_path(&path) && super::executable_file_exists(&path) {
            return Ok(path);
        }
    }
    if !is_store_path(current) {
        return Ok(current.to_owned());
    }
    Err(io::Error::other(
        "no stable flk launch path found: expose flk through an absolute profile or user-local symlink",
    ))
}

/// Hook calls must keep working even when a Nix-only installation has no
/// stable profile entry. A non-store server executable also supersedes any
/// FLOCK_BIN inherited from an outer pane, including development builds.
pub(super) fn pane_launch_path(current: &Path, stable: io::Result<PathBuf>) -> PathBuf {
    if !is_store_path(current) {
        current.to_owned()
    } else {
        stable.unwrap_or_else(|_| current.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_launch_path_keeps_store_executable_without_a_profile() {
        let current = Path::new("/nix/store/fixture-flock/bin/flk");
        let stable = select_launch_path(Some(current), current, &[]);
        assert_eq!(pane_launch_path(current, stable), current);
    }

    #[test]
    fn stable_launch_path_keeps_profile_symlink_after_retarget() {
        let dir = crate::test_support::unique_temp_path("stable-launch");
        std::fs::create_dir_all(&dir).unwrap();
        let first = dir.as_path().join("first");
        let second = dir.as_path().join("second");
        for path in [&first, &second] {
            std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
            super::super::make_executable(path).unwrap();
        }
        let profile = dir.as_path().join("flk");
        std::os::unix::fs::symlink(&first, &profile).unwrap();
        assert_eq!(
            select_launch_path(Some(&profile), &first, &[]).unwrap(),
            profile
        );
        std::fs::remove_file(&profile).unwrap();
        std::os::unix::fs::symlink(&second, &profile).unwrap();
        assert_eq!(
            select_launch_path(Some(&profile), &first, &[]).unwrap(),
            profile
        );
        assert_eq!(
            std::fs::canonicalize(&profile).unwrap(),
            std::fs::canonicalize(second).unwrap()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stable_launch_path_skips_store_directories_and_rejects_missing_target() {
        let dir = crate::test_support::unique_temp_path("stable-launch");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.as_path().join("flk");
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        super::super::make_executable(&path).unwrap();
        let pinned = Path::new("/nix/store/fixture-flock/bin/flk");
        let paths = vec![
            pinned.parent().unwrap().to_owned(),
            dir.as_path().to_owned(),
        ];
        assert_eq!(
            select_launch_path(Some(pinned), pinned, &paths).unwrap(),
            path
        );
        assert!(select_launch_path(Some(pinned), pinned, &[]).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
