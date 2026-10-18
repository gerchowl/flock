use super::*;
use std::os::unix::fs::{symlink, PermissionsExt};

fn install_case(tool: Option<&str>, payload: &str, refusal: Option<ErrorCode>) {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap();
    let root = crate::test_support::TempDir::socket();
    let bin = root.path().join("bin");
    let home = root.path().join("remote home 'quoted'");
    let dest = home.join(".local/bin/flk");
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    fs::create_dir(&bin).unwrap();
    fs::write(&dest, b"old binary").unwrap();
    let source = root.path().join("source");
    fs::write(&source, b"complete binary").unwrap();
    for program in ["cat", "mkdir", "rm", "chmod", "mv"] {
        symlink(
            crate::test_support::program_path(program),
            bin.join(program),
        )
        .unwrap();
    }
    if let Some(tool) = tool {
        let python = crate::test_support::program_path_string("python3");
        let check = if tool == "shasum" {
            "[ \"$1\" = -a ] && [ \"$2\" = 256 ] || exit 1\n"
        } else {
            ""
        };
        let hash = format!("#!/bin/sh\n{check}exec {} -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[-1],\"rb\").read()).hexdigest()+\"  \"+sys.argv[-1])' \"$@\"\n", shell_quote(&python));
        fs::write(bin.join(tool), hash).unwrap();
        fs::set_permissions(bin.join(tool), fs::Permissions::from_mode(0o755)).unwrap();
    }
    // Drain the real upload, then deliver a controlled stream to the real install script.
    let upload = shell_quote(root.path().join("upload").to_str().unwrap());
    let sender = if payload == "complete binary" {
        format!("cat {upload}")
    } else {
        format!("printf '%s' {}", shell_quote(payload))
    };
    let ssh = format!("#!/bin/sh\nfor command in \"$@\"; do :; done\ncat > {upload}\n{sender} | HOME={} PATH={} /bin/sh -c \"$command\"\n",
        shell_quote(home.to_str().unwrap()), shell_quote(bin.to_str().unwrap()));
    fs::write(bin.join("ssh"), ssh).unwrap();
    fs::set_permissions(bin.join("ssh"), fs::Permissions::from_mode(0o755)).unwrap();
    let prior = std::env::var_os("PATH");
    // Nextest isolates the test process and the lock serializes these fixtures.
    unsafe {
        std::env::set_var("PATH", &bin);
    }
    let result = install_remote_flock(
        "fixture.example.invalid",
        &RemoteFlock::for_platform(RemotePlatform::local()),
        &source,
    );
    unsafe {
        match prior {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
    }
    if let Some(code) = refusal {
        assert_eq!(
            fs::read(&dest).unwrap(),
            b"old binary",
            "refused input replaced the installed binary"
        );
        let error = result.unwrap_err();
        assert_eq!(crate::cli::io_error_exit(&error), code.exit());
        assert_eq!(
            error.get_ref().unwrap().downcast_ref::<ErrorCode>(),
            Some(&code)
        );
    } else {
        result.unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"complete binary");
    }
    assert_eq!(
        fs::read_dir(dest.parent().unwrap()).unwrap().count(),
        1,
        "temporary upload was not removed"
    );
}

#[test]
fn remote_install_rejects_truncated_stream_and_keeps_existing_binary() {
    install_case(
        Some("sha256sum"),
        "partial",
        Some(ErrorCode::RemoteChecksumMismatch),
    );
    install_case(
        Some("shasum"),
        "partial",
        Some(ErrorCode::RemoteChecksumMismatch),
    );
}

#[test]
fn remote_install_requires_a_checksum_tool_and_keeps_existing_binary() {
    install_case(
        None,
        "complete binary",
        Some(ErrorCode::ChecksumToolMissing),
    );
}

#[test]
fn remote_install_accepts_both_portable_checksum_commands() {
    install_case(Some("sha256sum"), "complete binary", None);
    install_case(Some("shasum"), "complete binary", None);
}

#[test]
fn generic_shell_failures_keep_stderr_and_never_become_checksum_refusals() {
    use std::os::unix::process::ExitStatusExt;
    for status in [2, 126, 127] {
        let error = remote_install_result(
            std::process::ExitStatus::from_raw(status << 8),
            b"Permission denied: fixture install path",
        )
        .unwrap_err();
        assert!(error.to_string().contains("Permission denied"));
        assert!(error
            .get_ref()
            .unwrap()
            .downcast_ref::<ErrorCode>()
            .is_none());
        assert_eq!(crate::cli::io_error_exit(&error), crate::cli::exit::FAILURE);
    }
}

#[test]
fn checksum_refusals_have_distinct_retry_classes_and_registered_exits() {
    use crate::cli::exit::{self, Retry};
    for (code, retry) in [
        (ErrorCode::RemoteChecksumMismatch, Retry::Yes),
        (ErrorCode::ChecksumMismatch, Retry::Never),
        (ErrorCode::ChecksumToolMissing, Retry::AfterHuman),
    ] {
        assert_eq!(code.retry(), retry);
        assert_eq!(code.exit(), exit::REFUSED);
        let error = io::Error::other(code);
        assert_eq!(crate::cli::io_error_exit(&error), exit::REFUSED);
    }
}

#[test]
#[allow(clippy::disallowed_methods)] // The fixture runs the generated script under dash when that shell is installed.
fn dash_shell_errors_preserve_the_real_failure_instead_of_requesting_hash_tools() {
    let dash = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|path| path.join("dash"))
            .find(|path| path.is_file())
    });
    let Some(dash) = dash else {
        let notice =
            "SKIPPED dash script coverage: dash not found on PATH; generic status checks still run";
        eprintln!("{notice}"); // guardrails-ok(debug-leftovers): explicit coverage notice required by #2106
        generic_shell_failures_keep_stderr_and_never_become_checksum_refusals();
        return;
    };
    let root = crate::test_support::TempDir::socket();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    symlink(crate::test_support::program_path("rm"), bin.join("rm")).unwrap();
    fs::write(bin.join("mkdir"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(bin.join("mkdir"), fs::Permissions::from_mode(0o755)).unwrap();
    symlink(crate::test_support::program_path("cat"), bin.join("cat")).unwrap();
    fs::write(
        bin.join("sha256sum"),
        "#!/bin/sh\nprintf 'Permission denied: checksum fixture\\n' >&2\nexit 2\n",
    )
    .unwrap();
    fs::set_permissions(bin.join("sha256sum"), fs::Permissions::from_mode(0o755)).unwrap();
    let existing = root.path().join("existing");
    fs::create_dir_all(existing.join(".local/bin")).unwrap();
    fs::write(existing.join(".local/bin/flk"), b"old binary").unwrap();
    let script = remote_install_script(
        &RemoteFlock::for_platform(RemotePlatform::local()),
        &"a".repeat(64),
    );
    for (program, home) in [
        (script.as_str(), None),
        (script.as_str(), Some(root.path())),
        ("(", Some(root.path())),
        (script.as_str(), Some(existing.as_path())),
    ] {
        let mut command = std::process::Command::new(&dash);
        command
            .args(["-eu", "-c", program])
            .env_clear()
            .env("PATH", &bin)
            .stdin(Stdio::null());
        if let Some(home) = home {
            command.env("HOME", home);
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(!output.stderr.is_empty());
        let error = remote_install_result(output.status, &output.stderr).unwrap_err();
        assert!(error
            .to_string()
            .contains(std::str::from_utf8(&output.stderr).unwrap().trim()));
        assert_eq!(crate::cli::io_error_exit(&error), crate::cli::exit::FAILURE);
        assert_eq!(
            fs::read(existing.join(".local/bin/flk")).unwrap(),
            b"old binary"
        );
    }
}

#[test]
#[allow(clippy::disallowed_methods)] // An owned shell drains the fixture upload and reports the resulting mismatch.
fn local_read_failure_keeps_its_type_before_remote_transfer_mismatch() {
    #[derive(Debug)]
    struct ReadFailure;
    impl std::fmt::Display for ReadFailure {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("fixture source became unreadable")
        }
    }
    impl std::error::Error for ReadFailure {}
    struct FailingReader(bool, io::ErrorKind);
    impl io::Read for FailingReader {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            if self.0 {
                return Err(io::Error::new(self.1, ReadFailure));
            }
            self.0 = true;
            bytes[..7].copy_from_slice(b"partial");
            Ok(7)
        }
    }
    for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::BrokenPipe] {
        let root = crate::test_support::TempDir::socket();
        let upload = root.path().join("upload");
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "cat > \"$1\"; exit 30", "upload-fixture"])
            .arg(&upload)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let error = finish_remote_upload(child, &mut FailingReader(false, kind)).unwrap_err();
        assert_eq!(fs::read(&upload).unwrap(), b"partial");
        assert_eq!(error.kind(), kind);
        assert!(error
            .get_ref()
            .unwrap()
            .downcast_ref::<ReadFailure>()
            .is_some());
    }
}

#[test]
#[allow(clippy::disallowed_methods)] // Run the dash fixture with an empty PATH to verify its explicit coverage notice.
fn missing_dash_reports_the_skipped_script_coverage() {
    let root = crate::test_support::TempDir::socket();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "remote::install_tests::dash_shell_errors_preserve_the_real_failure_instead_of_requesting_hash_tools",
            "--nocapture",
        ])
        .env_clear()
        .env("PATH", root.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("SKIPPED dash script coverage: dash not found on PATH"),
        "{stderr}"
    );
}

#[test]
#[allow(clippy::disallowed_methods)] // Waiting for the owned child fixes the closed-reader ordering before upload.
fn early_remote_exit_preserves_stderr_instead_of_write_side_broken_pipe() {
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "printf '%s' 'Read-only file system' >&2; exit 2"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Child::wait closes its stdin, so retain the writer until the reader has exited.
    let stdin = child.stdin.take().unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(2));
    child.stdin = Some(stdin);
    let error = finish_remote_upload(child, &mut &b"complete binary"[..]).unwrap_err();
    assert!(
        error.to_string().contains("Read-only file system"),
        "{error}"
    );
    assert_ne!(error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(crate::cli::io_error_exit(&error), crate::cli::exit::FAILURE);
}

#[test]
#[allow(clippy::disallowed_methods)] // The owned child drains a complete upload before reporting checksum mismatch.
fn complete_copy_preserves_the_remote_checksum_mismatch() {
    let child = std::process::Command::new("/bin/sh")
        .args(["-c", "cat > /dev/null; exit 30"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut source = io::Cursor::new(b"complete binary");
    let error = finish_remote_upload(child, &mut source).unwrap_err();
    assert_eq!(source.position(), b"complete binary".len() as u64);
    assert_eq!(
        error.get_ref().unwrap().downcast_ref::<ErrorCode>(),
        Some(&ErrorCode::RemoteChecksumMismatch)
    );
}
