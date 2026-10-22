//! Crash recovery for private auto-detect and probe sandboxes (#529).
//!
//! Only a dead owner's durable server records authorize cleanup. Unreadable or
//! mismatching identities retain the entire sandbox. Signals never come from a
//! name match or a process search. A nonblocking lock serializes concurrent sweeps.
#![expect(
    clippy::print_stderr,
    reason = "test harness reports retained crash sandboxes without modifying product logging"
)]
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Once;

use super::{process_lifetime, process_table};

/// Durable process identity expected in exactly one private sandbox.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ServerRecord {
    /// Server PID returned by spawn, never a process-group selector.
    pub pid: u32,
    /// Executable path, including a probe's sandbox-local copy.
    pub binary: PathBuf,
    /// HOME passed to the server, beneath the sandbox.
    pub home: PathBuf,
    /// XDG_STATE_HOME passed to the server, beneath the sandbox.
    pub state: PathBuf,
    /// Native birth identity when available to the launcher.
    pub start_time: Option<u64>,
}

impl ServerRecord {
    /// Persist with temp-file sync, rename and directory sync.
    pub fn write(&self, base: &Path) -> io::Result<()> {
        let pending = base.join(format!(".server-{}.pending", self.pid));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&pending)?;
        file.write_all(&serde_json::to_vec(self)?)?;
        file.sync_all()?;
        fs::rename(pending, base.join(format!(".server-{}.json", self.pid)))?;
        File::open(base)?.sync_all()
    }

    /// Inspect current executable, argv, environment and birth without signalling.
    pub fn matches(&self, base: &Path) -> bool {
        self.inspect(base) == Some(true)
    }

    fn inspect(&self, base: &Path) -> Option<bool> {
        let before = process_lifetime::birth(self.pid)?;
        if self.start_time.is_some_and(|expected| expected != before) {
            return Some(false);
        }
        let base = base.canonicalize().ok()?;
        let inside = |path: &Path| {
            path.canonicalize()
                .ok()
                .filter(|p| p != &base && p.starts_with(&base))
        };
        let (home, state) = (inside(&self.home)?, inside(&self.state)?);
        let binary = self.binary.canonicalize().ok()?;
        if !matches!(
            binary.file_name().and_then(|v| v.to_str()),
            Some("flk" | "flock")
        ) {
            return Some(false);
        }
        let info = process_table::process_info(self.pid).ok()?;
        if info.exe_path.and_then(|p| p.canonicalize().ok()).as_ref() != Some(&binary)
            || !server_argv(&info.argv)
        {
            return Some(false);
        }
        let env = process_table::process_environment(self.pid).ok()?;
        let env_path = |key: &str| {
            env.iter()
                .find_map(|entry| entry.strip_prefix(key))
                .and_then(|value| Path::new(value).canonicalize().ok())
        };
        Some(
            env_path("HOME=") == Some(home)
                && env_path("XDG_STATE_HOME=") == Some(state)
                && process_lifetime::birth(self.pid) == Some(before),
        )
    }
}

/// Recognize the server invocation produced by these fixture launchers.
pub fn server_argv(argv: &[String]) -> bool {
    matches!(argv, [_, mode] if mode == "server")
}

/// Record a direct child only in a sandbox owned by this test process.
pub fn record_spawn(pid: u32, config: &Path, runtime: &Path) {
    let Some(base) = runtime
        .parent()
        .filter(|base| owner(base) == Some(std::process::id()))
    else {
        return;
    };
    let record = ServerRecord {
        pid,
        binary: PathBuf::from(env!("CARGO_BIN_EXE_flk")),
        home: config.join("home"),
        state: config.join("state"),
        start_time: process_lifetime::birth(pid),
    };
    if let Err(error) = record.write(base) {
        eprintln!("sandbox server record {pid} retained: {error}");
    }
}

/// Record detached servers after the auto-detect startup path announces readiness.
/// Discovery records candidates only while this process owns the sandbox; it
/// never authorizes signals, which always recheck the durable record's identity.
pub fn record_runtime(config: &Path, runtime: &Path) {
    let Some(base) = runtime
        .parent()
        .filter(|base| owner(base) == Some(std::process::id()))
    else {
        return;
    };
    let Ok(pids) = super::flock_server_pids_for_runtime_dir(runtime) else {
        return;
    };
    for pid in pids {
        if !base.join(format!(".server-{pid}.json")).exists() {
            record_spawn(pid, config, runtime);
        }
    }
}

/// Locate an owned sandbox from either its runtime or default config socket.
pub fn record_socket(socket: &Path) {
    if let Some(base) = socket
        .ancestors()
        .find(|base| owner(base) == Some(std::process::id()))
    {
        record_runtime(&base.join("config"), &base.join("runtime"));
    }
}

fn owner(base: &Path) -> Option<u32> {
    let name = base.file_name()?.to_str()?;
    let name = name.strip_prefix("flock-autodetect-test-")?;
    let (pid, suffix) = name.split_once('-')?;
    let pid: u32 = pid.parse().ok()?;
    (pid > 0 && pid <= i32::MAX as u32 && !suffix.is_empty()).then_some(pid)
}

/// Run before spawning any auto-detect servers. Each nextest process performs
/// its own sweep; live owner checks and per-sandbox locks make that safe.
pub fn sweep_once() {
    static SWEEP: Once = Once::new();
    // guardrails-ok(hermetic): this prefix is the documented short sandbox namespace.
    SWEEP.call_once(|| sweep(Path::new("/tmp")));
}

/// Recover dead-owner sandboxes under this root; retain every unproven identity.
pub fn sweep(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let base = entry.path();
        let Some(pid) = owner(&base) else { continue };
        if process_lifetime::pid_alive(pid) {
            continue;
        }
        if let Err(error) = sweep_dead(&base, pid) {
            eprintln!("sandbox {} retained: {error}", base.display());
        }
    }
}

fn sweep_dead(base: &Path, owner: u32) -> io::Result<()> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(base.join(".sweep.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if process_lifetime::pid_alive(owner) {
        return Ok(());
    }
    let mut records: Vec<ServerRecord> = Vec::new();
    for entry in fs::read_dir(base)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".server") {
            if !name.ends_with(".json") || !entry.file_type()?.is_file() {
                return Err(io::Error::other("incomplete server record"));
            }
            records.push(serde_json::from_reader(File::open(entry.path())?)?);
        }
    }
    if records.is_empty()
        || records
            .iter()
            .any(|record| record.pid == 0 || record.pid > i32::MAX as u32)
    {
        return Err(io::Error::other("no valid server identity recorded"));
    }
    // Validate every living record before signalling any server in the sandbox.
    if records
        .iter()
        .any(|record| process_lifetime::pid_alive(record.pid) && !record.matches(base))
    {
        return Err(io::Error::other("unproven server identity"));
    }
    for record in records {
        if process_lifetime::pid_alive(record.pid) {
            process_lifetime::stop(record.pid, || record.matches(base))?;
        }
    }
    fs::remove_dir_all(base)
}
