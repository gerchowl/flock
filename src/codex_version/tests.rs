use super::*;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new() -> std::io::Result<Self> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "flock-version-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn executable(body: &str) -> (TempDir, std::path::PathBuf) {
    let directory = TempDir::new().expect("fixture directory");
    let path = directory.path().join("writer");
    std::fs::write(
        &path,
        format!(
            "#!{}\nimport sys\nassert sys.argv[1:] == ['--version']\n{body}\n",
            crate::test_support::program_path_string("python3")
        ),
    )
    .expect("scripted executable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
        .expect("fixture permissions");
    (directory, path)
}

#[test]
fn tested_metadata_and_actual_writer_allow_lifecycle() {
    let (directory, path) = executable("from pathlib import Path\np = Path(__file__).with_name('calls')\np.open('a').write('call\\n')\nprint('codex-cli 0.160.1')");
    let result = probe(&path, Some(TESTED_VERSION));
    assert!(result.supports_lifecycle());
    assert_eq!(result.screen_fallback_reason(), None);
    assert_eq!(result.writer_version.as_deref(), Some(TESTED_VERSION));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("calls")).unwrap(),
        "call\n"
    );
}

#[test]
fn either_missing_or_unsupported_version_selects_diagnosed_screen_fallback() {
    for (metadata, writer) in [
        (None, "0.160.1"),
        (Some("0.159.0"), "0.160.1"),
        (Some("0.160.1"), "0.161.0"),
    ] {
        let (_directory, path) = executable(&format!("print('codex-cli {writer}')"));
        let result = probe(&path, metadata);
        assert!(!result.supports_lifecycle());
        assert_eq!(
            result.screen_fallback_reason(),
            Some("unsupported_codex_version")
        );
        assert_eq!(
            result.probe_failure, None,
            "valid syntax can still be unsupported"
        );
    }
}

#[test]
fn failures_remove_writer_evidence_instead_of_authorizing_metadata() {
    for (body, failure) in [
        (
            "print('codex-cli 0.160.1'); sys.exit(1)",
            ProbeFailure::NonzeroExit,
        ),
        ("print('unexpected banner')", ProbeFailure::InvalidOutput),
        (
            "sys.stdout.buffer.write(b'\\xff')",
            ProbeFailure::InvalidOutput,
        ),
    ] {
        let (_directory, path) = executable(body);
        let result = probe(&path, Some(TESTED_VERSION));
        assert_eq!(result.probe_failure, Some(failure));
        assert_eq!(result.writer_version, None);
        assert_eq!(
            result.screen_fallback_reason(),
            Some("unsupported_codex_version")
        );
    }
    let directory = TempDir::new().unwrap();
    assert_eq!(
        probe(&directory.path().join("absent"), Some(TESTED_VERSION)).probe_failure,
        Some(ProbeFailure::SpawnOrRead)
    );
    assert_eq!(
        probe(Path::new("unresolved-writer"), Some(TESTED_VERSION)).probe_failure,
        Some(ProbeFailure::UnresolvedExecutable)
    );
}

#[test]
fn parse_contract_is_exact_and_bounded() {
    for ending in ["", "\n", "\r\n"] {
        assert_eq!(
            parse_version(format!("codex-cli 0.160.1{ending}").as_bytes()).as_deref(),
            Some(TESTED_VERSION)
        );
    }
    for invalid in [
        "0.160.1",
        "codex-cli 00.160.1",
        "codex-cli 0.160",
        "codex-cli 0.160.1-beta",
        "codex-cli 0.160.1+build",
        "codex-cli 0.160.1 ",
        "codex-cli 0.160.1\nextra",
        "codex-cli 0.160.1\r",
    ] {
        assert_eq!(parse_version(invalid.as_bytes()), None, "{invalid:?}");
    }
    assert_eq!(parse_version(&vec![b'0'; MAX_VERSION_BYTES + 1]), None);
}

fn timeout_fixture(inherit_stdout: bool) {
    let directory = TempDir::new().unwrap();
    let socket = directory.path().join("probe.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let body = format!(
        "import os, signal, socket\nc = socket.socket(socket.AF_UNIX)\nc.connect({})\n{}c.sendall((str(os.getpgrp()) + '\\n').encode())\nsignal.pause()",
        serde_json::to_string(socket.to_str().unwrap()).unwrap(),
        if inherit_stdout { "if os.fork() != 0: os._exit(0)\n" } else { "" }
    );
    let (_executable_directory, path) = executable(&body);
    let started = std::time::Instant::now();
    let result = probe(&path, Some(TESTED_VERSION));
    assert_eq!(result.probe_failure, Some(ProbeFailure::Timeout));
    assert_eq!(
        result.screen_fallback_reason(),
        Some("unsupported_codex_version")
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "probe failed its hang ceiling"
    );
    // The queued handshake proves the producer reached its silent stage. EOF
    // on the same connection proves group cleanup closed the descendant too.
    let (stream, _) = listener.accept().unwrap();
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut group = String::new();
    reader.read_line(&mut group).unwrap();
    struct Cleanup(u32);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            crate::platform::kill_process_group(self.0);
        }
    }
    let _cleanup = Cleanup(group.trim().parse().unwrap());
    let mut remainder = Vec::new();
    reader
        .read_to_end(&mut remainder)
        .expect("probe process group must release its inherited socket");
    assert!(remainder.is_empty());
}

#[test]
fn live_silent_writer_fails_without_hanging() {
    timeout_fixture(false);
}

#[test]
fn exited_launcher_with_live_descendant_stdout_fails_and_kills_group() {
    timeout_fixture(true);
}
