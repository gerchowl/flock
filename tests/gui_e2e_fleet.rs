//! Real embedded GUI, two real mesh nodes, and one bounded browser process group.
#![allow(clippy::disallowed_methods)] // Test driver owns its subprocesses.
mod support;

use serde_json::json;
use std::{
    fs, io,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::Duration,
};
use support::{
    environment::Command,
    fleet::{self, NodeSpec},
};

struct ProcessGroup {
    pid: i32,
    marker: Option<String>,
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // Playwright gives Chromium its own group. Its unique inherited marker
        // proves ownership even when the browser has detached from Bun's group.
        if let Some(marker) = &self.marker {
            for pid in support::process_table::list_process_ids().unwrap_or_default() {
                if support::process_table::process_environment(pid)
                    .is_ok_and(|vars| vars.iter().any(|var| var == marker))
                    && unsafe { libc::getpgid(pid as i32) } == pid as i32
                {
                    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                }
            }
        }
        // The leader remains unreaped until cleanup, so this group is still ours.
        unsafe { libc::kill(-self.pid, libc::SIGKILL) };
    }
}

fn run(mut command: Command, limit: Duration) -> io::Result<ExitStatus> {
    let marker = command.get_envs().find_map(|(key, value)| {
        (key == "FLOCK_GUI_FLEET_MANIFEST")
            .then(|| {
                value.map(|value| format!("FLOCK_GUI_FLEET_MANIFEST={}", value.to_string_lossy()))
            })
            .flatten()
    });
    command.stdin(Stdio::null());
    let mut command = command.into_std();
    command.process_group(0);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        let mut child = command.spawn()?;
        let group = ProcessGroup {
            pid: child.id().expect("running browser driver") as i32,
            marker,
        };
        // Keep the leader unreaped until the owned descendants have been killed.
        let pid = group.pid;
        let exited = tokio::time::timeout(
            limit,
            tokio::task::spawn_blocking(move || {
                let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
                let result = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        pid as libc::id_t,
                        info.as_mut_ptr(),
                        libc::WEXITED | libc::WNOWAIT,
                    )
                };
                if result == 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            }),
        )
        .await;
        drop(group);
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait()).await??;
        match exited {
            Ok(Ok(Ok(()))) => Ok(status),
            Ok(Ok(Err(error))) => Err(error),
            Ok(Err(error)) => Err(io::Error::other(error)),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "browser process group exceeded deadline",
            )),
        }
    })
}

struct Tools(PathBuf);
impl Drop for Tools {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn artifacts() -> PathBuf {
    std::env::var_os("GUI_E2E_FLEET_ARTIFACTS")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("web/client/artifacts/gui-e2e-fleet")
        })
}

fn copy_logs(directory: &Path, destination: &Path) {
    if let Ok(entries) = fs::read_dir(directory) {
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|ext| ext == "log") {
                fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
            }
        }
    }
}

#[test]
fn real_gui_two_nodes_online() {
    cfg!(feature = "web")
        .then_some(())
        .expect("gui-e2e-fleet requires --features web,ts and the real GUI build");
    let artifacts = artifacts();
    fs::create_dir_all(&artifacts).unwrap();
    for name in ["trace.zip", "fleet.png"] {
        let path = artifacts.join(name);
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => panic!("clear previous browser evidence: {error}"),
        }
    }
    let tools = Tools(PathBuf::from(format!(
        "/tmp/gui-tools-{}",
        getrandom::u64().unwrap()
    )));
    fs::create_dir(&tools.0).unwrap();
    let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|path| path.join("python3"))
        .find(|path| path.is_file())
        .expect("python3 fixture interpreter");
    let fixture = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/gui-fleet/claude.py"
    ))
    .expect("GUI fleet model fixture from #3093");
    fs::write(
        tools.0.as_path().join("claude"),
        format!("#!{}\n{fixture}", python.display()),
    )
    .unwrap();
    fs::set_permissions(
        tools.0.as_path().join("claude"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let config = "\n[client]\nbind = '127.0.0.1:0'\n[security]\nagent_account = 'none'\n[structured]\nrun_as = 'operator'\nprofile = 'fixture'\n";
    let specs = [
        NodeSpec::new("nodea", "alpha", &["nodeb"])
            .with_config(config)
            .with_tool_path(tools.0.as_path().into()),
        NodeSpec::new("nodeb", "beta", &[])
            .with_config(config)
            .with_tool_path(tools.0.as_path().into()),
    ];
    let fleet = fleet::spawn_with_startup_probe("gui", &specs, |fleet, name| {
        let node = fleet.node(name);
        fs::write(node.home.join("gui-fleet.json"), "[]").unwrap();
        if std::env::var_os("GUI_E2E_FLEET_REFUSE_EDGE").is_some() {
            fleet.refuse_edge("nodea", "nodeb");
        }
    });
    let nodes: Vec<_> = fleet
        .nodes
        .iter()
        .map(|node| {
            let mut env = serde_json::Map::new();
            for (key, value) in
                support::environment::isolated_env(&node.config_home, &node.runtime_dir)
            {
                env.insert(key.into(), json!(value));
            }
            env.insert("HOME".into(), json!(node.home));
            env.insert("XDG_STATE_HOME".into(), json!(node.home.join("state")));
            env.insert("XDG_DATA_HOME".into(), json!(node.home.join("data")));
            env.insert("XDG_CACHE_HOME".into(), json!(node.home.join("cache")));
            env.insert(
                "PATH".into(),
                json!(format!(
                    "{}:{}",
                    tools.0.as_path().display(),
                    std::env::var("PATH").unwrap()
                )),
            );
            json!({"name":node.name, "id":fleet.node_id(&node.name), "socket":node.api_socket, "env":env})
        })
        .collect();
    let manifest = fleet.base.join("gui.json");
    fs::write(&manifest, serde_json::to_vec(&json!({
        "version":1, "binary":env!("CARGO_BIN_EXE_flk"), "home":"nodea", "nodes":nodes, "artifacts":artifacts,
    })).unwrap()).unwrap();
    let mut command = Command::new("bun");
    command
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/web/client"))
        .args(["test", "./e2e-fleet/fleet.test.ts"]);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("FLOCK_") {
            command.env_remove(key);
        }
    }
    let home = fleet.node("nodea");
    command.envs(support::environment::isolated_env(
        &home.config_home,
        &home.runtime_dir,
    ));
    command.env("HOME", &home.home);
    let browser_cache = std::env::var_os("PLAYWRIGHT_BROWSERS_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap()).join(if cfg!(target_os = "macos") {
                "Library/Caches/ms-playwright"
            } else {
                ".cache/ms-playwright"
            })
        });
    command.env("PLAYWRIGHT_BROWSERS_PATH", browser_cache);
    command.env("FLOCK_GUI_FLEET_MANIFEST", manifest);
    command.stdout(fs::File::create(artifacts.join("bun.log")).unwrap());
    command.stderr(Stdio::from(
        fs::File::create(artifacts.join("bun-stderr.log")).unwrap(),
    ));
    let result = run(command, Duration::from_secs(120));
    if !result.as_ref().is_ok_and(|status| status.success()) {
        for node in &fleet.nodes {
            let destination = artifacts.join(&node.name);
            fs::create_dir_all(&destination).unwrap();
            copy_logs(
                &node.config_home.join(support::paths::app_dir_name()),
                &destination,
            );
        }
    }
    assert!(
        result.as_ref().is_ok_and(|status| status.success()),
        "browser driver failed: {result:?}; artifacts: {}",
        artifacts.display()
    );
}

#[test]
fn browser_group_deadline_rejects_a_live_silent_producer() {
    let mut command = Command::new("python3");
    command.args(["-c", "import signal; signal.pause()"]);
    let error = run(command, Duration::from_secs(1)).expect_err("silent child must fail");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
}

#[test]
fn browser_group_preserves_successful_exit() {
    let mut command = Command::new("python3");
    command.args(["-c", "pass"]);
    assert!(run(command, Duration::from_secs(10)).unwrap().success());
}
