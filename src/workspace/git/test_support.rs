// The whole module is #[cfg(test)] — TracedCommand polices product code
// (logging redesign PR-3), not test scaffolding.
#![allow(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn temp_test_dir(name: &str) -> PathBuf {
    let unique = format!(
        "flock-workspace-tests-{}-{}-{}",
        name,
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let path = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&path).unwrap();
    path
}

pub(crate) fn write_fake_tracked_repo(root: &Path) {
    let head_oid = "1111111111111111111111111111111111111111";
    let upstream_oid = "2222222222222222222222222222222222222222";
    std::fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
    std::fs::create_dir_all(root.join(".git/refs/remotes/origin")).unwrap();
    std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(root.join(".git/refs/heads/main"), format!("{head_oid}\n")).unwrap();
    std::fs::write(
        root.join(".git/refs/remotes/origin/main"),
        format!("{upstream_oid}\n"),
    )
    .unwrap();
    std::fs::write(
        root.join(".git/config"),
        "[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
    )
    .unwrap();
}

pub(super) fn run_git(cwd: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A git fixture whose child announces that it is alive, then waits for its
/// controller socket to close. No sleeping producer or ambient git process.
/// Dropping the controller releases even the pre-fix unbounded command.
pub(crate) struct HungGit {
    dir: crate::test_support::TempDir,
    old_path: Option<std::ffi::OsString>,
    ready: std::sync::mpsc::Receiver<std::os::unix::net::UnixStream>,
}
impl HungGit {
    pub(crate) fn new() -> Self {
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};
        let python = crate::test_support::program_path("python3");
        let dir = crate::test_support::TempDir::socket();
        let socket = dir.path().join("gate");
        let listener = UnixListener::bind(&socket).unwrap();
        let (tx, ready) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let stream = listener.accept().unwrap().0;
            let _ = tx.send(stream);
        });
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let script = format!("#!/bin/sh\nexec '{}' - <<'PYTHON'\nimport socket\ns = socket.socket(socket.AF_UNIX)\ns.connect({})\ns.recv(1)\nprint('1 2')\nPYTHON\n",
            python.display().to_string().replace('\'', "'\\''"), serde_json::to_string(&socket).unwrap());
        std::fs::write(bin.join("git"), script).unwrap();
        std::fs::set_permissions(bin.join("git"), std::fs::Permissions::from_mode(0o700)).unwrap();
        let old_path = std::env::var_os("PATH");
        // nextest runs each test in a separate process. Restore only after the
        // owned probe has returned and cannot launch another subprocess.
        std::env::set_var("PATH", bin);
        Self {
            dir,
            old_path,
            ready,
        }
    }
    pub(crate) fn cwd(&self) -> &Path {
        self.dir.path()
    }
    pub(crate) fn started(&self) -> std::os::unix::net::UnixStream {
        self.ready
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("git child died or never announced itself")
    }
}
impl Drop for HungGit {
    fn drop(&mut self) {
        // Unblock an accept if spawning the fixture failed before connection.
        let _ = std::os::unix::net::UnixStream::connect(self.dir.path().join("gate"));
        if let Some(path) = &self.old_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
    }
}
