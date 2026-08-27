//! Shared helpers for tests that need to run real programs.
//!
//! Tests reached for absolute FHS paths — `/usr/bin/true`, `/bin/cat` — as a
//! shorthand for "a trivial program that definitely exists". On NixOS neither
//! does: `/bin` holds only `sh`, and there is no `/usr/bin` to speak of, so
//! nine tests failed there on `main` while passing on macOS and on GitHub's
//! ubuntu runners. Same class as the `/bin/bash` fix in #268 — a test asserting
//! about the filesystem layout of the machine it happens to run on.
//!
//! Resolve through `PATH` instead, which is the one mechanism every platform
//! agrees on.

use std::path::PathBuf;

/// Absolute path to `name`, found on `PATH`.
///
/// Panics rather than returning an `Option`: every caller wants a program it
/// can spawn, and a missing `true`/`cat` means the test environment is broken
/// in a way that a confusing spawn error downstream would only obscure.
pub(crate) fn program_path(name: &str) -> PathBuf {
    let Some(path) = std::env::var_os("PATH") else {
        panic!("PATH is unset, cannot resolve `{name}` for a test");
    };
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| {
            panic!("`{name}` not found on PATH — required by this test");
        })
}

/// Same, as a `String`, for the many call sites that hold shell paths as text.
pub(crate) fn program_path_string(name: &str) -> String {
    program_path(name).to_string_lossy().into_owned()
}

/// A program that exits 0 immediately, for tests that need a pane's process to
/// spawn and finish without doing anything. `/usr/bin/true` by another name.
pub(crate) fn no_op_program() -> String {
    program_path_string("true")
}

/// A program that stays up instead of exiting the moment it execs, for tests
/// that need a pane whose process is still there afterwards (#178).
///
/// `sh` with no arguments sits reading the pane's pty, which is the shape a
/// real agent has. Tests that used [`no_op_program`] for this got away with it
/// until flock started refusing a start whose agent was already gone.
pub(crate) fn live_program() -> String {
    program_path_string("sh")
}

/// `PATH` as a `(key, value)` pair, for check scripts.
///
/// `checks::script::run_script` deliberately calls `env_clear()`, so a script
/// it runs has no `PATH` at all. On an FHS distro `/bin/sh` still finds
/// `sleep` or `dd` via its compiled-in fallback (`/bin:/usr/bin`); on NixOS
/// that fallback resolves to nothing. Tests whose scripts call out to real
/// binaries must therefore pass `PATH` explicitly, which is what a real check
/// with an `env` block would do.
pub(crate) fn path_env_pair() -> (String, String) {
    (
        "PATH".to_string(),
        std::env::var("PATH").unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_a_program_that_exists_on_path() {
        let resolved = program_path("sh");
        assert!(resolved.is_absolute(), "{resolved:?} should be absolute");
        assert!(resolved.is_file(), "{resolved:?} should exist");
        assert!(resolved.ends_with("sh"));
    }

    #[test]
    #[allow(clippy::disallowed_methods)] // Test spawns a real binary to prove it is spawnable — TracedCommand polices product code.
    fn no_op_program_is_spawnable_and_exits_zero() {
        // The property every `/usr/bin/true` call site actually depended on.
        let status = std::process::Command::new(no_op_program())
            .status()
            .expect("the no-op program should spawn");
        assert!(status.success());
    }

    #[test]
    #[should_panic(expected = "not found on PATH")]
    fn missing_program_panics_with_a_useful_message() {
        program_path("flock-definitely-not-a-real-program");
    }
}
