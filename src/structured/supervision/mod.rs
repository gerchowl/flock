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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Quarantine {
    at: u64,
    reason: String,
}
impl Quarantine {
    fn new(reason: String) -> io::Result<Self> {
        Ok(Self {
            at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_secs(),
            reason,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct Owner {
    owner: ProcessIdentity,
    supervisor: ProcessIdentity,
    uncertain: bool,
    #[serde(default)]
    quarantine: Option<Quarantine>,
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
    fn refresh(&mut self, dir: &Path) -> io::Result<()> {
        let before = self.tree.clone();
        self.snapshot();
        if self.tree != before {
            self.write(dir)?;
        }
        Ok(())
    }
    fn snapshot_and_signal(&mut self, dir: &Path, signal: Signal) -> io::Result<()> {
        let result = self.refresh(dir);
        self.signal(signal);
        result
    }
    fn has_living(&self) -> bool {
        self.tree.iter().any(|identity| identity.alive())
    }
    fn signal(&self, signal: Signal) {
        // Recheck birth immediately before each signal; never signal an unknown pid.
        for identity in self.tree.iter().rev().filter(|identity| identity.alive()) {
            platform::signal_processes(&[identity.pid], signal);
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
        supervisor: owner,
        uncertain: false,
        quarantine: None,
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
/// After tree termination, `finish_io` must cancel/drain and observe both I/O
/// completions within a bound. Its failure retains files; it runs on this worker.
/// Snapshots persist for crash recovery. EOF grace precedes TERM, a live-tree
/// snapshot precedes KILL, and cleanup waits for confirmed identity disappearance.
pub fn supervise(
    mut child: Child,
    mut files: SessionConfigDir,
    profile: PathBuf,
    stop: StopRequest,
    close_stdin: impl FnOnce(),
    finish_io: impl FnOnce() -> io::Result<()>,
) -> ExitReport {
    files.retain();
    // Bind and persist immediately, before the first process-tree enumeration.
    let (mut owner, mut failure) =
        match bind_child(&mut child, &files, profile, &stop, ProcessIdentity::capture) {
            Ok(bound) => bound,
            Err(error) => {
                return ExitReport {
                    status: reap_owned(&mut child, &stop).ok(),
                    stop_requested: stop.requested(),
                    cleanup: Err(error),
                }
            }
        };
    if let Err(error) = owner.refresh(files.path()) {
        failure.get_or_insert(error);
    }
    let mut status = None;
    let mut close = Some(close_stdin);
    let mut phase = 0;
    let mut deadline = Instant::now();
    let mut snapshot = Instant::now();
    loop {
        if snapshot.elapsed() >= SNAPSHOT {
            if let Err(error) = owner.refresh(files.path()) {
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
        if status.is_some() && !owner.has_living() {
            let cleanup = finish_io()
                .and_then(|()| failure.map_or_else(|| cleanup_transient(&owner, files.path()), Err))
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
            if let Err(error) = owner.snapshot_and_signal(files.path(), Signal::Terminate) {
                failure.get_or_insert(error);
            }
            phase = 2;
            deadline = Instant::now() + TERM_GRACE;
        }
        if phase == 2 && Instant::now() >= deadline {
            if let Err(error) = owner.snapshot_and_signal(files.path(), Signal::Kill) {
                failure.get_or_insert(error);
            }
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

fn bind_child(
    child: &mut Child,
    files: &SessionConfigDir,
    profile: PathBuf,
    stop: &StopRequest,
    capture: impl FnOnce(u32) -> io::Result<ProcessIdentity>,
) -> io::Result<(Owner, Option<io::Error>)> {
    let mut owner = Owner::read(files.path())?;
    match capture(child.id()) {
        Ok(root) => {
            owner.owner = root;
            owner.supervisor = ProcessIdentity::capture(std::process::id())?;
            owner.uncertain = false;
            owner.profile = Some(profile);
            owner.tree = vec![root];
        }
        Err(error) => {
            // Unknown descendants prevent cleanup even after authoritative reaping.
            let status = reap_owned(child, stop)?;
            let reason = format!(
                "structured_child_identity_unavailable: {error}; owned child reaped: {status}"
            );
            owner.quarantine = Some(Quarantine::new(reason.clone())?);
            owner.write(files.path())?;
            return Err(io::Error::other(reason));
        }
    }
    let failure = owner.write(files.path()).err();
    Ok((owner, failure))
}

fn reap_owned(child: &mut Child, stop: &StopRequest) -> io::Result<ExitStatus> {
    stop.request();
    // The unreaped Child owns its pid; kill/wait cannot target a reused pid.
    match child.try_wait()? {
        Some(status) => Ok(status),
        None => {
            child.kill()?;
            child.wait()
        }
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
    let sessions = path
        .parent()
        .ok_or_else(|| io::Error::other("registry parent missing"))?;
    match sessions.canonicalize() {
        Ok(canonical) if canonical == sessions => (),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        _ => return Err(io::Error::other("redirected Claude registry directory")),
    }
    let registry: Option<serde_json::Value> = match private_fs::open_regular_file(&path) {
        Ok(file) => {
            if path.canonicalize()? != path {
                return Err(io::Error::other("redirected Claude registry"));
            }
            let mut bytes = Vec::new();
            file.take(65_537).read_to_end(&mut bytes)?;
            if bytes.len() > 65_536 {
                return Err(io::Error::other("oversize Claude registry"));
            }
            Some(serde_json::from_slice(&bytes)?)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if let Some(socket) = registry
        .as_ref()
        .and_then(|registry| registry.get("messagingSocketPath"))
        .filter(|v| !v.is_null())
    {
        let socket = socket
            .as_str()
            .ok_or_else(|| io::Error::other("invalid socket path"))?;
        if !socket.is_empty() {
            private_fs::remove_transient(Path::new(socket), Some(owner.owner.pid))?;
        }
    }
    for entry in fs::read_dir(sessions)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(hash) = name
            .to_str()
            .and_then(|name| name.strip_prefix(&format!("{}.", owner.owner.pid)))
            .and_then(|name| name.strip_suffix(".key"))
        else {
            continue;
        };
        if hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            private_fs::remove_transient(&entry.path(), None)?;
        }
    }
    private_fs::remove_transient(&path, None)
}

/// Outcome for one contained session directory; failures never abort other entries.
#[derive(Debug)]
pub struct SweepEntry {
    /// Absolute session directory inspected.
    pub directory: PathBuf,
    /// Why it was preserved, quarantined or removed.
    pub outcome: SweepOutcome,
}

/// Recovery facts; a quarantine never grants permission to signal or delete.
#[derive(Debug)]
pub enum SweepOutcome {
    /// The owning supervisor still holds the session.
    Live,
    /// Known descendants and transients were confirmed gone and files removed.
    Removed,
    /// Stable quarantine across restarts; inspect before removing the dir by hand.
    Quarantined {
        /// First quarantine time, in Unix seconds.
        at: u64,
        /// Original reason, preserved without retrying recovery.
        reason: String,
    },
    /// Unsafe/malformed entry or cleanup failure; files retained for inspection.
    Retained(String),
}

/// Sweep on a worker, returning an outcome for every session. Only namespace-level
/// failures abort the scan. Uncertain spawns are durably quarantined, never reaped
/// or retried; inspect their files/processes, then remove the directory by hand.
/// Live supervisors are preserved. Confirmed identities precede all other reaping.
/// Root aliases resolve; linked structured namespace ancestors remain refused.
pub fn sweep_stale_sessions(state: &Path) -> io::Result<Vec<SweepEntry>> {
    let parent = state.canonicalize()?.join("structured/sessions");
    let entries = match fs::read_dir(&parent) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    if parent.canonicalize()? != parent {
        return Err(io::Error::other("redirected structured namespace"));
    }
    private_fs::ensure_private_dir(&parent)?;
    let mut outcomes = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                outcomes.push(SweepEntry {
                    directory: parent.clone(),
                    outcome: SweepOutcome::Retained(format!("directory entry unreadable: {error}")),
                });
                continue;
            }
        };
        let name = entry.file_name();
        let Some(suffix) = name.to_str().and_then(|s| s.strip_prefix("flk-claude-")) else {
            continue;
        };
        if suffix.len() != 32 || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let directory = entry.path();
        let outcome = sweep_entry(&directory)
            .unwrap_or_else(|error| SweepOutcome::Retained(error.to_string()));
        outcomes.push(SweepEntry { directory, outcome });
    }
    Ok(outcomes)
}

fn sweep_entry(directory: &Path) -> io::Result<SweepOutcome> {
    private_fs::ensure_private_dir(directory)?;
    let mut owner = Owner::read(directory)?;
    if let Some(quarantine) = &owner.quarantine {
        return Ok(SweepOutcome::Quarantined {
            at: quarantine.at,
            reason: quarantine.reason.clone(),
        });
    }
    if owner.supervisor.alive() {
        return Ok(SweepOutcome::Live);
    }
    if owner.uncertain {
        let quarantine =
            Quarantine::new("uncertain spawn: child birth identity was not persisted".into())?;
        let outcome = SweepOutcome::Quarantined {
            at: quarantine.at,
            reason: quarantine.reason.clone(),
        };
        owner.quarantine = Some(quarantine);
        owner.write(directory)?;
        return Ok(outcome);
    }
    owner.snapshot_and_signal(directory, Signal::Kill)?;
    let deadline = Instant::now() + KILL_GRACE;
    while owner.has_living() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "stale descendants survived",
            ));
        }
        std::thread::sleep(TICK);
    }
    cleanup_transient(&owner, directory)?;
    fs::remove_dir_all(directory)?;
    Ok(SweepOutcome::Removed)
}

#[cfg(test)]
mod tests;
