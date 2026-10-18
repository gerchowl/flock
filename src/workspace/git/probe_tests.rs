#![allow(clippy::disallowed_methods)] // Fixtures execute git, not product subprocesses.
use super::*;
use crate::workspace::git::test_support::run_git;
fn deadline() -> Instant {
    Instant::now() + GIT_EXEC_TIMEOUT
}
fn repo(reftable: bool) -> crate::test_support::TempDir {
    let dir = crate::test_support::TempDir::socket();
    let mut args = vec!["init", "-q", "-b", "main"];
    if reftable {
        args.push("--ref-format=reftable");
    }
    run_git(dir.path(), &args);
    dir
}
#[test]
fn git2034_unborn_and_missing_refs_are_absent_in_both_stores() {
    for reftable in [false, true] {
        let dir = repo(reftable);
        for name in ["HEAD", "refs/heads/main", "refs/remotes/origin/main"] {
            assert_eq!(revision(dir.path(), name, deadline()), Ok(None));
        }
    }
}
#[test]
fn git2034_not_repository_is_fatal() {
    let dir = crate::test_support::TempDir::socket();
    assert_eq!(
        revision(dir.path(), "HEAD", deadline()),
        Err(Error::GitExit)
    );
}
#[test]
fn git2034_corrupt_ref_database_is_fatal_in_both_stores() {
    for reftable in [false, true] {
        let dir = repo(reftable);
        if reftable {
            std::fs::write(
                dir.path().join(".git/reftable/tables.list"),
                "missing.ref\n",
            )
            .unwrap();
        } else {
            std::fs::write(
                dir.path().join(".git/packed-refs"),
                "invalid ref database\n",
            )
            .unwrap();
        }
        assert_eq!(
            revision(dir.path(), "HEAD", deadline()),
            Err(Error::GitExit),
            "reftable={reftable}"
        );
        if reftable {
            assert!(crate::workspace::git::status::git_status_snapshot_for_cwd(
                dir.path(),
                None,
                deadline()
            )
            .is_err());
        }
    }
}
#[test]
fn git2034_unsafe_repository_is_fatal_in_both_stores() {
    for reftable in [false, true] {
        let dir = repo(reftable);
        // nextest isolates processes. Command-scope config overrides ambient safe.directory entries.
        let vars = [
            ("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1"),
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "safe.directory"),
            ("GIT_CONFIG_VALUE_0", ""),
        ];
        let saved: Vec<_> = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var_os(k)))
            .collect();
        for (key, value) in vars {
            std::env::set_var(key, value);
        }
        let result = revision(dir.path(), "HEAD", deadline());
        for (key, value) in saved {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
        assert_eq!(result, Err(Error::GitExit), "reftable={reftable}");
    }
}

#[test]
fn git2034_corrupt_loose_ref_is_not_optional_absence() {
    let dir = repo(false);
    std::fs::write(dir.path().join(".git/refs/heads/main"), "invalid-oid\n").unwrap();
    assert_eq!(
        revision(dir.path(), "HEAD", deadline()),
        Err(Error::GitExit)
    );
}
