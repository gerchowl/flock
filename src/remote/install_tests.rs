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
    let ssh = format!("#!/bin/sh\nfor command in \"$@\"; do :; done\ncat >/dev/null\nprintf '%s' {} | HOME={} PATH={} /bin/sh -c \"$command\"\n",
        shell_quote(payload), shell_quote(home.to_str().unwrap()), shell_quote(bin.to_str().unwrap()));
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
        Some(ErrorCode::ChecksumMismatch),
    );
    install_case(Some("shasum"), "partial", Some(ErrorCode::ChecksumMismatch));
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
