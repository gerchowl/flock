use super::*;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

#[test]
#[allow(clippy::disallowed_methods)] // This fixture launches the freshly built CLI, never a product subprocess.
fn local_checksum_refusal_reaches_the_real_cli_with_its_registered_exit() {
    let root = crate::test_support::TempDir::socket();
    let bin = root.path().join("bin");
    let home = root.path().join("home");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&home).unwrap();
    let (os, arch) = platform_target();
    let manifest = serde_json::json!({"version":"99.0.0", "notes":"fixture",
        "assets":{format!("{os}-{arch}"):{"url":format!("{RELEASE_ASSET_URL_PREFIX}v99.0.0/flock-fixture"),"sha256":"a".repeat(64)}}});
    let quoted_manifest = format!("'{}'", manifest.to_string().replace('\'', "'\\''"));
    let curl = format!("#!/bin/sh\nwhile [ \"$#\" -gt 0 ]; do\nif [ \"$1\" = -o ]; then shift; printf partial > \"$1\"; exit 0; fi\nshift\ndone\nprintf '%s' {}\n", quoted_manifest);
    fs::write(bin.join("curl"), curl).unwrap();
    fs::set_permissions(bin.join("curl"), fs::Permissions::from_mode(0o755)).unwrap();
    let helper = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("flk");
    let mut child = Command::new(helper)
        .arg("update")
        .env_clear()
        .env("PATH", &bin)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("c"))
        .env("XDG_STATE_HOME", home.join("s"))
        .env("XDG_DATA_HOME", home.join("d"))
        .env("TMPDIR", root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "updater did not refuse within its fail ceiling: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::yield_now();
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(crate::cli::exit::REFUSED),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("checksum_mismatch"));
}
