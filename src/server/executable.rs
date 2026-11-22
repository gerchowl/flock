//! Canonical executable identity shared by internal server launches.
//!
//! Resolve once before constructing a spawn command. A missing/deleted target
//! refuses startup with `server_executable_unresolved` (ADR-0029 §4), rather
//! than falling back to an alias, PATH lookup or a different server image.

use std::io;
use std::path::{Path, PathBuf};

use crate::api::error_code::ErrorCode;

/// A resolved regular file. Only [`resolve`] constructs this launch identity.
///
/// The pathname is pinned, not the inode or bytes. Removal after resolution
/// remains an ordinary spawn failure, and handoff still verifies version and
/// protocol before committing ownership.
pub(crate) struct Executable(PathBuf);

impl Executable {
    /// The canonical path to use for both the command and its diagnostics.
    pub(crate) fn as_path(&self) -> &Path {
        &self.0
    }

    /// Transfer this identity into the handoff request sent by the CLI.
    pub(crate) fn into_path(self) -> PathBuf {
        self.0
    }
}

/// Resolve an explicit handoff target, or the current executable for a daemon.
/// Unknown paths, dangling links and non-files refuse without a fallback.
pub(crate) fn resolve(candidate: Option<&Path>) -> io::Result<Executable> {
    let resolve = || {
        let path = match candidate {
            Some(path) => path.to_path_buf(),
            None => std::env::current_exe()?,
        };
        let canonical = std::fs::canonicalize(path)?;
        if !std::fs::metadata(&canonical)?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        Ok(Executable(canonical))
    };
    resolve().map_err(|source: io::Error| io::Error::new(source.kind(), Unresolved(source)))
}

/// Typed payload retained through the CLI and the socket handoff error path.
#[derive(Debug)]
struct Unresolved(io::Error);

impl std::fmt::Display for Unresolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: cannot resolve the server executable: {}. Restore the binary or select an existing --import-exe before retrying", ErrorCode::ServerExecutableUnresolved.as_str(), self.0)
    }
}

impl std::error::Error for Unresolved {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// Preserve the registered refusal instead of classifying it by OS error kind.
pub(crate) fn error_code(error: &io::Error) -> Option<ErrorCode> {
    error
        .get_ref()
        .filter(|cause| cause.is::<Unresolved>())
        .map(|_| ErrorCode::ServerExecutableUnresolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_alias_keeps_its_target_after_repointing() {
        let base = crate::test_support::TempDir::socket();
        let first = base.path().join("first");
        let second = base.path().join("second");
        let alias = base.path().join("alias");
        std::fs::write(&first, "first").unwrap();
        std::fs::write(&second, "second").unwrap();
        std::os::unix::fs::symlink(&first, &alias).unwrap();
        let executable = resolve(Some(&alias)).unwrap();
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(second, &alias).unwrap();
        assert_eq!(executable.as_path(), std::fs::canonicalize(first).unwrap());
    }

    #[test]
    fn removed_target_and_non_file_refuse_with_the_registered_class() {
        let base = crate::test_support::TempDir::socket();
        let target = base.path().join("removed");
        let alias = base.path().join("alias");
        std::fs::write(&target, "binary").unwrap();
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        std::fs::remove_file(target).unwrap();
        for path in [&alias, &base.path().to_path_buf()] {
            let error = resolve(Some(path)).err().expect("unresolved target");
            let code = error_code(&error).unwrap();
            assert_eq!(code, ErrorCode::ServerExecutableUnresolved);
            assert_eq!(code.retry(), crate::cli::exit::Retry::AfterInspecting);
            assert_eq!(crate::cli::output::io_error_code(&error), code);
        }
    }
}
