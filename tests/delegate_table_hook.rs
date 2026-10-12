//! #962 (ADR-0027 §4): after a compaction, an architect gets its delegate
//! table back as SessionStart hook output, and nothing is typed into its pane.
//!
//! Each case starts where the value really starts: the installed Claude hook
//! shim, run inside a real agent pane with a real SessionStart payload, so
//! `delegate.list` resolves `self` from genuine process ancestry. It ends at
//! the hook's stdout, which is what Claude Code puts in context.

// The fixture drives the compiled CLI with raw Command, as the delegate tests do.
#![allow(clippy::disallowed_methods)]
mod support;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_flock_pid,
    unregister_spawned_flock_pid, wait_for_socket,
};

const WITHIN: Duration = Duration::from_secs(20);
const TABLE_HEADER: &str = "Your open delegates";

struct Server {
    _master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    base: PathBuf,
    socket: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        let _ = self.child.kill();
        if let Some(pid) = pid {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                let mut status = 0;
                let result =
                    unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
                if result == pid as libc::pid_t || result == -1 {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
        unregister_spawned_flock_pid(pid);
        cleanup_test_base(&self.base);
    }
}

fn start_server() -> Server {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let base = PathBuf::from(format!("/tmp/hdtab-{}-{nanos}", std::process::id()));
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket = runtime_dir.join("flock.sock");
    let app_dir = if cfg!(debug_assertions) {
        "flock-dev"
    } else {
        "flock"
    };
    fs::create_dir_all(config_home.join(app_dir)).unwrap();
    fs::create_dir_all(&runtime_dir).unwrap();
    for dir in ["work", "home", "state", "data"] {
        fs::create_dir_all(base.join(dir)).unwrap();
    }
    register_runtime_dir(&runtime_dir);
    fs::write(
        config_home.join(app_dir).join("config.toml"),
        "onboarding = false\n",
    )
    .unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_flk"));
    cmd.arg("server");
    cmd.cwd(&base);
    for (key, value) in support::environment::isolated_env(&config_home, &runtime_dir) {
        cmd.env(key, value);
    }
    cmd.env("XDG_DATA_HOME", base.join("data"));
    cmd.env("XDG_STATE_HOME", base.join("state"));
    cmd.env("HOME", base.join("home"));
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("FLOCK_SOCKET_PATH", &socket);
    cmd.env_remove("FLOCK_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("FLOCK_ENV");
    cmd.env_remove("FLOCK_HOST_NAME");
    cmd.env_remove("FLOCK_DISABLE_SOUND");
    support::environment::assert_pty_isolated(&cmd);
    let child = support::environment::spawn_pty(pair.slave.as_ref(), cmd).unwrap();
    register_spawned_flock_pid(child.process_id());
    wait_for_socket(&socket, Duration::from_secs(5));
    Server {
        _master: pair.master,
        child,
        base,
        socket,
    }
}

fn request(server: &Server, value: serde_json::Value) -> serde_json::Value {
    let mut stream = UnixStream::connect(&server.socket).unwrap();
    stream.write_all(value.to_string().as_bytes()).unwrap();
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

/// An agent pane that runs whatever shell the test drops into `<base>/<name>.cmd`
/// and logs every line typed into it to `<base>/<name>.typed`.
struct Architect {
    name: &'static str,
    pane: String,
}

fn start_architect(server: &Server, name: &'static str) -> Architect {
    let base = server.base.display();
    // Named `claude`, so flock identifies the pane's process as Claude Code,
    // the one harness a self-compaction can be armed for.
    let script = server.base.join(name).join("claude");
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             printf '%s\\n❯ \\n%s\\n' '{rule}' '{rule}'\n\
             exec 3<&0\n\
             cat <&3 >> '{base}/{name}.typed' &\n\
             while :; do\n\
             if [ -e '{base}/{name}.cmd' ]; then\n\
             sh '{base}/{name}.cmd' > '{base}/{name}.tmp' 2>> '{base}/{name}.err'\n\
             rm '{base}/{name}.cmd'\n\
             mv '{base}/{name}.tmp' '{base}/{name}.out'\n\
             fi\n\
             sleep 0.05\n\
             done\n",
            rule = "\u{2500}".repeat(48)
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let started = request(
        server,
        serde_json::json!({"id": "arch", "method": "agent.start", "params": {
            "name": name, "cwd": server.base.join("work"), "argv": [script]}}),
    );
    let pane = started["result"]["agent"]["pane_id"]
        .as_str()
        .unwrap_or_else(|| panic!("agent.start: {started}"))
        .to_string();
    Architect { name, pane }
}

impl Architect {
    /// Run `body` inside the architect's pane and return its stdout.
    fn run(&self, server: &Server, body: &str) -> String {
        let out = server.base.join(format!("{}.out", self.name));
        let _ = fs::remove_file(&out);
        let prelude = format!(
            "export FLOCK_SOCKET_PATH='{}' FLOCK_BIN='{}'\n",
            server.socket.display(),
            env!("CARGO_BIN_EXE_flk")
        );
        fs::write(
            server.base.join(format!("{}.cmd", self.name)),
            format!("{prelude}{body}\n"),
        )
        .unwrap();
        let deadline = Instant::now() + WITHIN;
        while !out.exists() {
            assert!(
                Instant::now() < deadline,
                "the architect never ran {body:?}: {}",
                fs::read_to_string(server.base.join(format!("{}.err", self.name)))
                    .unwrap_or_default()
            );
            thread::sleep(Duration::from_millis(30));
        }
        fs::read_to_string(out).unwrap()
    }

    /// The installed Claude hook shim, fired as Claude Code fires it.
    fn session_start(&self, server: &Server, source: &str) -> String {
        let shim = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/integration/assets/claude/flock-agent-state.sh");
        let payload = serde_json::json!({
            "hook_event_name": "SessionStart", "session_id": "ses-arch", "source": source,
        });
        self.run(
            server,
            &format!("printf '%s' '{payload}' | sh '{}' session", shim.display()),
        )
    }

    fn delegate(&self, server: &Server, name: &str) {
        let work = server.base.join("work");
        self.run(
            server,
            &format!(
                "\"$FLOCK_BIN\" agent start {name} --cwd '{}' --no-focus -- /bin/sh -c 'exec sleep 600'",
                work.display()
            ),
        );
    }

    /// Arming needs flock to have identified the pane's process as Claude.
    fn wait_identified_as_claude(&self, server: &Server) {
        let deadline = Instant::now() + WITHIN;
        loop {
            let got = request(
                server,
                serde_json::json!({"id": "ag", "method": "agent.get",
                    "params": {"target": self.pane}}),
            );
            if got["result"]["agent"]["agent"] == "claude" {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "never identified as claude: {got}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn typed(&self, server: &Server) -> String {
        fs::read_to_string(server.base.join(format!("{}.typed", self.name))).unwrap_or_default()
    }
}

/// The `additionalContext` of a SessionStart hook's stdout, if it wrote one.
fn context(stdout: &str) -> Option<String> {
    if stdout.trim().is_empty() {
        return None;
    }
    let out: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|err| panic!("hook stdout is not JSON ({err}): {stdout:?}"));
    assert_eq!(out["hookSpecificOutput"]["hookEventName"], "SessionStart");
    Some(
        out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap_or_else(|| panic!("no additionalContext: {out}"))
            .to_string(),
    )
}

/// Each row's delegate and edge state, the part of the table that does not age.
fn rows(table: &str) -> Vec<String> {
    assert!(table.starts_with(TABLE_HEADER), "{table}");
    table
        .lines()
        .skip(1)
        .map(|row| row.split("  ").take(3).collect::<Vec<_>>().join(" "))
        .collect()
}

#[test]
fn a_compacted_architect_gets_its_table_as_hook_output_armed_or_not() {
    let server = start_server();
    let lead = start_architect(&server, "lead");
    let idle = start_architect(&server, "idle");
    // The real startup report, which is what makes the pane a Claude pane.
    assert_eq!(lead.session_start(&server, "startup").trim(), "");
    lead.delegate(&server, "d1");
    lead.delegate(&server, "d2");

    // Unarmed auto-compaction: a two-row table, nothing typed.
    let table = context(&lead.session_start(&server, "compact")).expect("a table");
    assert!(table.starts_with("Your open delegates (2 edges"), "{table}");
    assert_eq!(rows(&table), [" d1 live", " d2 live"], "{table}");
    assert_eq!(lead.typed(&server), "");

    // A resume gets it back too, and a fresh startup does not.
    let resumed = context(&lead.session_start(&server, "resume")).expect("a table");
    assert_eq!(rows(&resumed), rows(&table));
    assert_eq!(lead.session_start(&server, "startup").trim(), "");

    // Armed self-compaction: the same table, and the arming that carries the
    // continuation is still held for the server to deliver.
    lead.wait_identified_as_claude(&server);
    let armed = request(
        &server,
        serde_json::json!({"id": "arm", "method": "pane.arm_self_compact", "params": {
            "pane": lead.pane, "continuation": "carry on with the plan"}}),
    );
    assert_eq!(armed["result"]["state"], "armed", "{armed}");
    let armed_table = context(&lead.session_start(&server, "compact")).expect("a table");
    assert_eq!(rows(&armed_table), rows(&table));
    let again = request(
        &server,
        serde_json::json!({"id": "arm2", "method": "pane.arm_self_compact", "params": {
            "pane": lead.pane, "continuation": "x"}}),
    );
    assert_eq!(again["result"]["state"], "pending", "{again}");
    assert!(!lead.typed(&server).contains(TABLE_HEADER));

    // An agent with no edges gets no extra output at all.
    assert_eq!(idle.session_start(&server, "startup").trim(), "");
    assert_eq!(idle.session_start(&server, "compact").trim(), "");
    assert_eq!(idle.typed(&server), "");
}
