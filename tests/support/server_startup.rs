//! Owned normal-server startup for fleet fixtures (ADR-0037 §1).
//!
//! The child announces readiness after initializing its app and both listeners.
//! A single failure-only deadline bounds the announcement and optional API
//! responsiveness proof, including partial
//! output. Child exit is observed without reaping, even if a descendant holds
//! the PTY open. At most 64 KiB of startup output is retained for diagnostics.
//! After readiness, a cancellable worker keeps draining the PTY. Stop cancels
//! and joins the output worker before releasing its descriptors, then joins
//! the exit observer before reaping the child.

use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::readiness::{read_announcement, read_socket_line, Stages};
use portable_pty::{Child, MasterPty};

const OUTPUT_LIMIT: usize = 64 * 1024;
const STOP_GRACE: Duration = Duration::from_millis(400);

static OWNED_GROUPS: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();

fn owned_groups() -> std::sync::MutexGuard<'static, HashSet<u32>> {
    OWNED_GROUPS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

/// Shared panic/interrupt cleanup may signal this group, but must leave its
/// wait status and PID identity reserved for the owning guard's observers.
pub(super) fn signal_owned_group(pid: u32) -> bool {
    let groups = owned_groups();
    if !groups.contains(&pid) {
        return false;
    }
    // SAFETY: the registry lock prevents reaping while this signal is sent.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
    true
}

enum Event {
    Announcement(io::Result<()>),
    Exited(io::Result<()>),
}

/// Exclusively own one PTY session leader, its master and startup observers.
/// No method reaps the child until all group signals and observations finish.
pub struct ServerStartup {
    child: Option<Box<dyn Child + Send + Sync>>,
    master: Option<Box<dyn MasterPty + Send>>,
    api: PathBuf,
    exit_notice: Option<UnixStream>,
    source: Option<UnixStream>,
    pid: Option<u32>,
    own_group: bool,
    cancel: Option<UnixStream>,
    reader: Option<JoinHandle<()>>,
    watcher: Option<JoinHandle<()>>,
    events: Option<mpsc::Receiver<Event>>,
    output: Arc<Mutex<Vec<u8>>>,
    stages: Arc<Stages>,
}

impl ServerStartup {
    /// Take ownership before fallible observer setup. The caller must release
    /// its parent PTY slave before this call, and pass a fresh owned session.
    pub fn new(
        master: Box<dyn MasterPty + Send>,
        child: Box<dyn Child + Send + Sync>,
        api: &Path,
        ceiling: Duration,
    ) -> io::Result<Self> {
        Self::observe(master, child, api, ceiling, None)
    }

    /// Observe an owned output stream instead of the PTY for failure injection.
    /// Retaining its writer models a descendant holding output open after exit.
    pub fn with_output(
        master: Box<dyn MasterPty + Send>,
        child: Box<dyn Child + Send + Sync>,
        api: &Path,
        ceiling: Duration,
        output: UnixStream,
    ) -> io::Result<Self> {
        Self::observe(master, child, api, ceiling, Some(output))
    }

    fn observe(
        master: Box<dyn MasterPty + Send>,
        child: Box<dyn Child + Send + Sync>,
        api: &Path,
        ceiling: Duration,
        source: Option<UnixStream>,
    ) -> io::Result<Self> {
        let pid = child.process_id();
        let mut owner = Self {
            child: Some(child),
            master: Some(master),
            api: api.to_path_buf(),
            exit_notice: None,
            source,
            pid,
            own_group: false,
            cancel: None,
            reader: None,
            watcher: None,
            events: None,
            output: Arc::new(Mutex::new(Vec::new())),
            stages: Arc::new(Stages::new(ceiling)),
        };
        let pid = pid.ok_or_else(|| io::Error::other("startup child has no pid"))?;
        // SAFETY: the unreaped child remains owned by this guard.
        if unsafe { libc::getpgid(pid as libc::pid_t) } != pid as libc::pid_t {
            return Err(io::Error::other("startup child does not own its PTY group"));
        }
        owner.own_group = true;
        owned_groups().insert(pid);
        super::register_spawned_flock_pid(Some(pid));
        let master = owner.master.as_ref().expect("owned master");
        let (fd, reader): (_, Box<dyn Read + Send>) = if let Some(source) = &owner.source {
            (source.as_raw_fd(), Box::new(source.try_clone()?))
        } else {
            (
                master
                    .as_raw_fd()
                    .ok_or_else(|| io::Error::other("startup PTY has no descriptor"))?,
                master.try_clone_reader().map_err(io::Error::other)?,
            )
        };
        let (cancel, stop) = UnixStream::pair()?;
        owner.cancel = Some(cancel);
        let (exited, mut exit_notice) = UnixStream::pair()?;
        owner.exit_notice = Some(exited);
        let (notice, events) = mpsc::channel();
        owner.events = Some(events);
        let output = owner.output.clone();
        let stages = owner.stages.clone();
        let api = api.to_path_buf();
        let announced = notice.clone();
        owner.reader = Some(thread::Builder::new().name("fixture output".into()).spawn(
            move || {
                let mut reader = Output {
                    reader,
                    fd,
                    stop,
                    stages: Some(stages),
                    output: Some(output),
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    read_announcement(&mut reader, &api)
                }))
                .unwrap_or_else(|_| Err(io::Error::other("startup output reader panicked")));
                let ready = result.is_ok();
                let _ = announced.send(Event::Announcement(result));
                if ready {
                    reader.stages = None;
                    reader.output = None;
                    let _ = io::copy(&mut reader, &mut io::sink());
                }
            },
        )?);
        owner.watcher = Some(
            thread::Builder::new()
                .name("fixture child exit".into())
                .spawn(move || {
                    let result = observe_exit(pid, false).map(|_| ());
                    let _ = notice.send(Event::Exited(result));
                    // One byte fits the fresh notification stream. Closing it
                    // also wakes a response wait if this observer fails.
                    let _ = exit_notice.write_all(&[1]);
                })?,
        );
        Ok(owner)
    }

    /// PID of the still-owned, unreaped child, never a cached reaped identity.
    pub fn process_id(&self) -> u32 {
        assert!(self.child.is_some(), "server is stopped");
        self.pid.expect("owned child pid")
    }

    /// Observe liveness without consuming the child's wait status.
    pub fn has_exited(&self) -> io::Result<bool> {
        observe_exit(self.process_id(), true)
    }

    /// Wait on the owned exit observer, without polling or consuming wait status.
    pub fn wait_exited(&self, ceiling: Duration) -> io::Result<()> {
        let notice = self
            .exit_notice
            .as_ref()
            .expect("owned child exit observer");
        Stages::new(ceiling).readable(&[notice.as_raw_fd()])?;
        if self.has_exited()? {
            Ok(())
        } else {
            Err(io::Error::other(
                "child exit observer ended before child exit",
            ))
        }
    }

    /// Require this child's complete announcement once. Exit, malformed or
    /// excessive output, observer failure, EOF and silence are errors, not readiness.
    pub fn wait_ready(&mut self) -> io::Result<()> {
        let events = self
            .events
            .take()
            .ok_or_else(|| io::Error::other("startup already observed"))?;
        let mut exited = false;
        loop {
            let event = events
                .recv_timeout(self.stages.remaining())
                .map_err(|error| {
                    self.failure(match error {
                        mpsc::RecvTimeoutError::Timeout => io::Error::new(
                            io::ErrorKind::TimedOut,
                            "startup producer stayed silent at failure ceiling",
                        ),
                        mpsc::RecvTimeoutError::Disconnected => {
                            io::Error::new(io::ErrorKind::UnexpectedEof, "startup observers closed")
                        }
                    })
                })?;
            match event {
                Event::Announcement(result) => {
                    result.map_err(|error| self.failure(error))?;
                    if exited || self.has_exited().map_err(|e| self.failure(e))? {
                        return Err(self.failure(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "child exited before readiness",
                        )));
                    }
                    return Ok(());
                }
                Event::Exited(result) => {
                    result.map_err(|e| self.failure(e))?;
                    exited = true;
                    // Read available failure output before cancellation wins.
                    self.cancel_reader();
                }
            }
        }
    }

    /// Require initialization and one matching pong from the API listener, sharing
    /// the existing startup ceiling. There are no short probes or retries.
    /// Connect and response waits observe child exit independently of output
    /// closure, and share the same failure-only deadline. A short-lived current
    /// thread runtime drives nonblocking connect; its worker is always joined.
    pub fn wait_responsive(&mut self) -> io::Result<()> {
        self.wait_ready()?;
        self.ping_once().map_err(|error| self.failure(error))
    }

    fn ping_once(&self) -> io::Result<()> {
        let remaining = self.stages.remaining();
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "startup ceiling expired before pong",
            ));
        }
        let mut api = self.connect_api()?;
        api.send(r#"{"id":"startup-ready","method":"ping","params":{}}"#)?;
        let exited = self.exit_notice.as_ref().expect("owned exit notification");
        let response = read_socket_line(
            &mut api.stream,
            |fd| {
                if self.stages.readable(&[exited.as_raw_fd(), fd])? == 0 {
                    if self.has_exited()? {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "child exited before pong",
                        ));
                    }
                    return Err(io::Error::other("child exit observer ended before pong"));
                }
                Ok(())
            },
            "API closed before startup pong",
        )?;
        let response: serde_json::Value = serde_json::from_str(&response)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if response["id"] != "startup-ready" || response["result"]["type"] != "pong" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected startup pong: {response}"),
            ));
        }
        if self.has_exited()? {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "child exited before startup pong was observed",
            ));
        }
        Ok(())
    }

    fn connect_api(&self) -> io::Result<super::json_api::JsonApi> {
        let path = self.api.clone();
        let stages = self.stages.clone();
        let exited = self
            .exit_notice
            .as_ref()
            .expect("owned exit notification")
            .try_clone()?;
        exited.set_nonblocking(true)?;
        // Joining a bounded connector also works from callers inside a runtime.
        thread::Builder::new().name("fixture API connect".into()).spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_io().enable_time().build()?;
            runtime.block_on(async move {
                let exited = tokio::net::UnixStream::from_std(exited)?;
                let stream = tokio::time::timeout(stages.remaining(), async {
                    tokio::select! {
                        result = tokio::net::UnixStream::connect(path) => result,
                        result = exited.readable() => {
                            result?;
                            Err(io::Error::new(io::ErrorKind::UnexpectedEof, "child exit observer ended before API connect"))
                        }
                    }
                }).await.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "API connect stayed silent at startup ceiling"))??;
                let stream = stream.into_std()?;
                stream.set_nonblocking(false)?;
                let remaining = stages.remaining();
                if remaining.is_zero() {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "startup ceiling expired during API connect"));
                }
                super::json_api::JsonApi::from_stream(stream, remaining)
            })
        })?.join().map_err(|_| io::Error::other("API connector panicked"))?
    }

    fn failure(&self, error: io::Error) -> io::Error {
        let output = self.output.lock().unwrap_or_else(|e| e.into_inner());
        io::Error::new(
            error.kind(),
            format!(
                "{error}\ncaptured startup output:\n{}",
                String::from_utf8_lossy(&output)
            ),
        )
    }

    fn cancel_reader(&self) {
        if let Some(cancel) = &self.cancel {
            let _ = cancel.shutdown(Shutdown::Both);
        }
    }

    /// Stop the owned group, including descendants of an exited leader. Its
    /// unreaped PID reserves that group identity until after the final signal.
    #[expect(
        clippy::print_stderr,
        reason = "fixture cleanup reports exit-observer failures without hiding the startup error"
    )]
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if self.own_group {
                let group = self.pid.expect("owned group") as libc::pid_t;
                // SAFETY: exclusive ownership keeps the leader unreaped.
                unsafe {
                    libc::kill(-group, libc::SIGTERM);
                }
                let deadline = Instant::now() + STOP_GRACE;
                loop {
                    match observe_exit(group as u32, true) {
                        Ok(true) => break,
                        Ok(false) if Instant::now() < deadline => thread::yield_now(),
                        Ok(false) => break,
                        Err(error) => {
                            eprintln!("fixture child exit observation failed: {error}");
                            break;
                        }
                    }
                }
                // A dead leader can leave live members. Do not reap it first.
                unsafe {
                    libc::kill(-group, libc::SIGKILL);
                }
            } else {
                let _ = child.kill();
            }
            self.cancel_reader();
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
            // An exiting child can block flushing a full PTY. The output worker
            // has joined, so close every reader before waiting for child exit.
            self.master = None;
            self.source = None;
            if let Some(watcher) = self.watcher.take() {
                let _ = watcher.join();
            }
            {
                // Serialize the final reap with shared cleanup's group signals.
                let mut groups = owned_groups();
                let _ = child.wait();
                if let Some(pid) = self.pid {
                    groups.remove(&pid);
                }
            }
            super::unregister_spawned_flock_pid(self.pid);
        }
        self.master = None;
        self.source = None;
    }
}

impl Drop for ServerStartup {
    fn drop(&mut self) {
        self.stop();
    }
}

fn observe_exit(pid: u32, nonblocking: bool) -> io::Result<bool> {
    loop {
        // SAFETY: initialized storage is valid for waitid. WNOWAIT preserves
        // ownership; only ServerStartup::stop consumes the child's wait status.
        let (result, exited) = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let flags = libc::WEXITED | libc::WNOWAIT | if nonblocking { libc::WNOHANG } else { 0 };
            let result = libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, flags);
            (result, info.si_signo != 0)
        };
        if result == 0 {
            return Ok(exited);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

struct Output {
    reader: Box<dyn Read + Send>,
    fd: RawFd,
    stop: UnixStream,
    stages: Option<Arc<Stages>>,
    output: Option<Arc<Mutex<Vec<u8>>>>,
}

impl Read for Output {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if let Some(stages) = &self.stages {
            // Buffered fatal output wins over cancellation after child exit.
            if stages.readable(&[self.fd, self.stop.as_raw_fd()])? == 1 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "startup reader cancelled",
                ));
            }
        } else {
            loop {
                let mut polls = [
                    libc::pollfd {
                        fd: self.stop.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: self.fd,
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
                // SAFETY: both descriptors stay owned until this worker joins.
                let result = unsafe { libc::poll(polls.as_mut_ptr(), 2, -1) };
                if result > 0 {
                    if polls[0].revents != 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::ConnectionAborted,
                            "output drain cancelled",
                        ));
                    }
                    break;
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
        let len = if let Some(output) = &self.output {
            let remaining =
                OUTPUT_LIMIT.saturating_sub(output.lock().unwrap_or_else(|e| e.into_inner()).len());
            if remaining == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "startup output exceeds 64 KiB",
                ));
            }
            buf.len().min(remaining)
        } else {
            buf.len()
        };
        let n = self.reader.read(&mut buf[..len])?;
        if let Some(output) = &self.output {
            output
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend_from_slice(&buf[..n]);
        }
        Ok(n)
    }
}
