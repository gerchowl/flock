//! Process-tree supervision and conservative cleanup for structured sessions.
//!
//! Run supervision/sweeping on a worker, never on the server loop. Sampling
//! retains detached descendants already observed; it cannot discover a child
//! whose ancestry disappeared entirely between samples. Uncertain ownership,
//! unreadable metadata and unconfirmed termination retain generated files.
#![deny(missing_docs)]

use super::launch::SessionConfigDir;
use crate::platform::{self, structured as private_fs, Signal};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(20);
const SNAPSHOT: Duration = Duration::from_millis(100);
const EOF_GRACE: Duration = Duration::from_millis(150);
const TERM_GRACE: Duration = Duration::from_millis(250);
const KILL_GRACE: Duration = Duration::from_secs(2);

/// A process identity; no unknown-birth or PID-only identities are accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    /// Operating-system process id.
    pub pid: u32,
    /// Platform start-time discriminator (opaque, not a wall-clock timestamp).
    pub birth: u64,
}
impl ProcessIdentity {
    /// Capture an existing process's identity, refusing unavailable birth times.
    pub fn capture(pid: u32) -> io::Result<Self> {
        Ok(Self {
            pid,
            birth: platform::process_start_time(pid)
                .ok_or_else(|| io::Error::other("process birth unavailable"))?,
        })
    }
    /// Whether this pid still names the recorded process.
    pub fn alive(self) -> bool {
        platform::process_start_time(self.pid) == Some(self.birth)
    }
}

/// Shared stop intent. Requesting it is nonblocking and precedes all signals.
#[derive(Clone, Default)]
pub struct StopRequest(Arc<AtomicBool>);
impl StopRequest {
    /// Record stop intent before the supervisor closes stdin or sends a signal.
    pub fn request(&self) {
        self.0.store(true, Ordering::Release);
    }
    /// Read the recorded intent; exit codes never synthesize a stop request.
    pub fn requested(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Process exit and cleanup facts; turn outcome interpretation belongs to #1462.
#[derive(Debug)]
pub struct ExitReport {
    /// Reaped direct-child status, or None when termination could not be confirmed.
    pub status: Option<ExitStatus>,
    /// Intent recorded before shutdown; inspect cleanup errors before calling it stopped.
    pub stop_requested: bool,
    /// Explicit failure retains generated files for recovery.
    pub cleanup: io::Result<()>,
}

#[derive(Serialize, Deserialize)]
struct Owner {
    owner: ProcessIdentity,
    uncertain: bool,
    profile: Option<PathBuf>,
    tree: Vec<ProcessIdentity>,
}
impl Owner {
    fn read(dir: &Path) -> io::Result<Self> {
        let mut data = Vec::new();
        private_fs::open_token(&dir.join("owner.json"))?
            .take(1_048_577)
            .read_to_end(&mut data)?;
        if data.len() > 1_048_576 {
            return Err(io::Error::other("oversize session owner record"));
        }
        Ok(serde_json::from_slice(&data)?)
    }
    fn write(&self, dir: &Path) -> io::Result<()> {
        let path = dir.join("owner.next");
        let mut file = private_fs::create_private_file(&path)?;
        file.write_all(&serde_json::to_vec(self)?)?;
        file.sync_all()?;
        fs::rename(path, dir.join("owner.json"))?;
        File::open(dir)?.sync_all()
    }
    fn snapshot(&mut self) {
        self.tree.retain(|identity| identity.alive());
        let processes: Vec<_> = platform::all_process_ids()
            .into_iter()
            .filter_map(|pid| {
                Some((
                    ProcessIdentity::capture(pid).ok()?,
                    platform::process_parent_id(pid)?,
                ))
            })
            .collect();
        loop {
            let before = self.tree.len();
            for (child, parent) in &processes {
                if !self.tree.contains(child)
                    && self.tree.iter().any(|p| p.pid == *parent && p.alive())
                    && child.alive()
                {
                    self.tree.push(*child);
                }
            }
            if self.tree.len() == before {
                break;
            }
        }
    }
    fn living(&self) -> Vec<u32> {
        self.tree
            .iter()
            .filter(|p| p.alive())
            .map(|p| p.pid)
            .collect()
    }
    fn signal(&self, signal: Signal) {
        // Recheck birth immediately before each signal; never signal an unknown pid.
        for identity in self.tree.iter().rev() {
            if identity.alive() {
                platform::signal_processes(&[identity.pid], signal);
            }
        }
    }
}

/// Create a contained generated-files directory with the preparer's birth identity.
/// Unarmed guards still clean up normally on preparation failure or unwind.
pub fn prepare_session_files(state: &Path) -> io::Result<SessionConfigDir> {
    let parent = state.canonicalize()?.join("structured");
    private_fs::ensure_private_dir(&parent)?;
    let parent = parent.join("sessions");
    private_fs::ensure_private_dir(&parent)?;
    let files = SessionConfigDir::create(&parent)?;
    let owner = ProcessIdentity::capture(std::process::id())?;
    Owner {
        owner,
        uncertain: false,
        profile: None,
        tree: vec![owner],
    }
    .write(files.path())?;
    Ok(files)
}

/// Arm BEFORE attempting spawn. A failed/uncertain spawn leaves recoverable files
/// instead of deleting configuration possibly still used by an unknown child.
pub fn arm_session_files(files: &mut SessionConfigDir) -> io::Result<()> {
    files.retain();
    let mut owner = Owner::read(files.path())?;
    owner.uncertain = true;
    owner.write(files.path())
}

/// Supervise an owned child on a dedicated runtime worker. The caller must arm
/// files before spawn. `close_stdin` must be a nonblocking transport request.
/// Snapshots persist for crash recovery. EOF grace precedes TERM, a live-tree
/// snapshot precedes KILL, and cleanup waits for confirmed identity disappearance.
pub fn supervise(
    mut child: Child,
    mut files: SessionConfigDir,
    profile: PathBuf,
    stop: StopRequest,
    close_stdin: impl FnOnce(),
) -> ExitReport {
    files.retain();
    let root = match ProcessIdentity::capture(child.id()) {
        Ok(root) => root,
        Err(error) => {
            return ExitReport {
                status: child.try_wait().ok().flatten(),
                stop_requested: stop.requested(),
                cleanup: Err(error),
            }
        }
    };
    let mut owner = Owner {
        owner: root,
        uncertain: false,
        profile: Some(profile),
        tree: vec![root],
    };
    owner.snapshot();
    let mut failure = owner.write(files.path()).err();
    let mut status = None;
    let mut close = Some(close_stdin);
    let mut phase = 0;
    let mut deadline = Instant::now();
    let mut snapshot = Instant::now();
    loop {
        if snapshot.elapsed() >= SNAPSHOT {
            owner.snapshot();
            if let Err(error) = owner.write(files.path()) {
                failure.get_or_insert(error);
            }
            snapshot = Instant::now();
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(exit) => status = exit,
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        if status.is_some() && owner.living().is_empty() {
            let cleanup = failure
                .map_or_else(|| cleanup_transient(&owner, files.path()), Err)
                .and_then(|()| files.remove_after_reap());
            return ExitReport {
                status,
                stop_requested: stop.requested(),
                cleanup,
            };
        }
        if phase == 0 && (stop.requested() || failure.is_some() || status.is_some()) {
            // Internal errors also record intent before any shutdown side effect.
            if failure.is_some() {
                stop.request();
            }
            if let Some(close) = close.take() {
                close();
            }
            phase = 1;
            deadline = Instant::now() + EOF_GRACE;
        }
        if phase == 1 && Instant::now() >= deadline {
            owner.snapshot();
            if let Err(error) = owner.write(files.path()) {
                failure.get_or_insert(error);
            }
            owner.signal(Signal::Terminate);
            phase = 2;
            deadline = Instant::now() + TERM_GRACE;
        }
        if phase == 2 && Instant::now() >= deadline {
            owner.snapshot();
            if let Err(error) = owner.write(files.path()) {
                failure.get_or_insert(error);
            }
            owner.signal(Signal::Kill);
            phase = 3;
            deadline = Instant::now() + KILL_GRACE;
        }
        if phase == 3 {
            owner.signal(Signal::Kill);
            if Instant::now() >= deadline {
                return ExitReport {
                    status,
                    stop_requested: stop.requested(),
                    cleanup: Err(failure.unwrap_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::TimedOut,
                            "structured descendants survived shutdown",
                        )
                    })),
                };
            }
        }
        std::thread::sleep(TICK);
    }
}

fn cleanup_transient(owner: &Owner, session_dir: &Path) -> io::Result<()> {
    let Some(profile) = &owner.profile else {
        return Ok(());
    };
    let structured = session_dir
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::other("invalid session directory"))?;
    if profile.parent() != Some(structured.join("claude").as_path())
        || profile.canonicalize()? != *profile
    {
        return Err(io::Error::other("profile outside structured namespace"));
    }
    private_fs::ensure_private_dir(profile)?;
    if platform::process_start_time(owner.owner.pid).is_some() {
        return Err(io::Error::other(
            "refusing transient cleanup for a live/reused pid",
        ));
    }
    let path = profile
        .join("sessions")
        .join(format!("{}.json", owner.owner.pid));
    let mut bytes = Vec::new();
    match private_fs::open_regular_file(&path) {
        Ok(file) => {
            file.take(65_537).read_to_end(&mut bytes)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }
    if bytes.len() > 65_536 {
        return Err(io::Error::other("oversize Claude registry"));
    }
    let registry: serde_json::Value = serde_json::from_slice(&bytes)?;
    if let Some(socket) = registry.get("messagingSocketPath").filter(|v| !v.is_null()) {
        let socket = socket
            .as_str()
            .ok_or_else(|| io::Error::other("invalid socket path"))?;
        if !socket.is_empty() {
            private_fs::remove_transient(Path::new(socket), Some(owner.owner.pid))?;
        }
    }
    private_fs::remove_transient(&path, None)
}

/// Startup sweep on a worker: only dead, known owners are eligible. Live owners
/// are untouched. Uncertain/malformed records fail explicitly and retain files.
/// Recorded descendants are killed by matching birth identity before deletion.
pub fn sweep_stale_sessions(state: &Path) -> io::Result<usize> {
    let parent = state.join("structured/sessions");
    let entries = match fs::read_dir(&parent) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    private_fs::ensure_private_dir(&parent)?;
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(suffix) = name.to_str().and_then(|s| s.strip_prefix("flk-claude-")) else {
            continue;
        };
        if suffix.len() != 32 || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        private_fs::ensure_private_dir(&entry.path())?;
        let owner = Owner::read(&entry.path())?;
        if owner.owner.alive() {
            continue;
        }
        if owner.uncertain {
            return Err(io::Error::other("uncertain session spawn retained"));
        }
        owner.signal(Signal::Kill);
        let deadline = Instant::now() + KILL_GRACE;
        while !owner.living().is_empty() {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "stale descendants survived",
                ));
            }
            std::thread::sleep(TICK);
        }
        cleanup_transient(&owner, &entry.path())?;
        fs::remove_dir_all(entry.path())?;
        removed += 1;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests;
