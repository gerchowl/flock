#[cfg(unix)]
use crate::process::TracedCommand;
#[cfg(unix)]
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Child;
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use tracing::info;

#[cfg(unix)]
const HANDOFF_VERSION: u32 = 1;
#[cfg(unix)]
const READY_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(unix)]
const OWNED_ACK_TIMEOUT: Duration = Duration::from_millis(500);
#[cfg(unix)]
const IMPORT_REAP_TIMEOUT: Duration = Duration::from_millis(500);
// Restoration includes large scrollback buffers and up to five seconds waiting
// for public sockets. Give the whole pre-ready import more room than any single
// 30-second socket exchange, without allowing an indefinite startup stall.
#[cfg(unix)]
const IMPORT_STARTUP_TIMEOUT: Duration = Duration::from_secs(120);
/// Most pane descriptors one SCM_RIGHTS message carries. A handoff of more
/// panes sends several batches of at most this many, and a handoff of this
/// many or fewer sends exactly one, so its wire bytes match the pre-batching
/// protocol and [`HANDOFF_VERSION`] stays 1 (#471).
#[cfg(unix)]
pub(crate) const MAX_FDS_PER_BATCH: usize = 64;
/// The one data byte every fd batch rides on.
#[cfg(unix)]
const FD_BATCH_TAG: u8 = b'F';
/// Room the receiver leaves for one SCM_RIGHTS message. Linux caps a message
/// at 253 descriptors (`SCM_MAX_FD`), so no Linux sender can overflow this,
/// and anything above [`MAX_FDS_PER_BATCH`] is refused after it arrives.
#[cfg(unix)]
const RECV_FD_CAPACITY: usize = 256;
#[cfg(unix)]
pub(crate) const MAX_REPLAY_BYTES_PER_PANE: usize = 8 * 1024;
#[cfg(unix)]
pub(crate) const COMMIT_TIMEOUT: Duration = READY_TIMEOUT;

#[cfg(unix)]
#[derive(Serialize, Deserialize)]
pub(crate) struct HandoffManifest {
    pub version: u32,
    pub source_version: String,
    pub source_protocol: u32,
    #[serde(default)]
    pub store_generation: u64,
    pub expected_version: Option<String>,
    pub expected_protocol: Option<u32>,
    pub snapshot: crate::persist::SessionSnapshot,
    pub panes: Vec<crate::handoff_runtime::HandoffRuntimeState>,
}

#[cfg(unix)]
pub(crate) struct ReceivedHandoff {
    pub manifest: HandoffManifest,
    pub fds: Vec<RawFd>,
    pub stream: UnixStream,
}

#[cfg(unix)]
pub(crate) fn handoff_socket_path() -> PathBuf {
    crate::session::data_dir().join(format!("flock-handoff-{}.sock", std::process::id()))
}

#[cfg(unix)]
pub(crate) fn spawn_handoff_import(
    import_exe: Option<&Path>,
    socket_path: &Path,
    token: &str,
) -> io::Result<Child> {
    let fallback_exe;
    let exe = if let Some(import_exe) = import_exe {
        import_exe
    } else {
        fallback_exe = std::env::current_exe().map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("failed to determine flock executable path: {err}"),
            )
        })?;
        &fallback_exe
    };
    let mut command = TracedCommand::new(exe, "server");
    command
        .arg("server")
        .arg("--handoff-import")
        .arg(socket_path)
        .arg(token)
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command.spawn_traced().map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "failed to spawn handoff import server at {}: {err}",
                exe.display()
            ),
        )
    })
}

#[cfg(unix)]
const IMPORT_ARMED: u8 = 0;
#[cfg(unix)]
const IMPORT_DISARMED: u8 = 1;
#[cfg(unix)]
const IMPORT_EXPIRED: u8 = 2;

#[cfg(unix)]
pub(crate) struct ImportWatchdog {
    state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    cancel: std::sync::mpsc::Sender<()>,
}

#[cfg(unix)]
impl ImportWatchdog {
    pub(crate) fn disarm(&self) -> io::Result<()> {
        transition_import_deadline(&self.state, IMPORT_DISARMED).map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "handoff import startup deadline expired",
            )
        })?;
        let _ = self.cancel.send(());
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for ImportWatchdog {
    fn drop(&mut self) {
        let _ = self.disarm();
    }
}

#[cfg(unix)]
fn transition_import_deadline(state: &std::sync::atomic::AtomicU8, next: u8) -> Result<u8, u8> {
    state.compare_exchange(
        IMPORT_ARMED,
        next,
        std::sync::atomic::Ordering::SeqCst,
        std::sync::atomic::Ordering::SeqCst,
    )
}

/// Enforce the deadline only while the exporter can still roll back. Atomic
/// arbitration ensures an expired importer cannot announce readiness.
#[cfg(unix)]
pub(crate) fn start_import_watchdog() -> io::Result<ImportWatchdog> {
    // Isolated integration tests shorten the budget to exercise late commit.
    let timeout = std::env::var("FLOCK_TEST_HANDOFF_IMPORT_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(IMPORT_STARTUP_TIMEOUT);
    let (cancel, receiver) = std::sync::mpsc::channel();
    let state = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(IMPORT_ARMED));
    let deadline_state = state.clone();
    std::thread::Builder::new()
        .name("handoff-deadline".into())
        .spawn(move || {
            if import_deadline_expired(receiver, timeout)
                && transition_import_deadline(&deadline_state, IMPORT_EXPIRED).is_ok()
            {
                // SAFETY: _exit terminates the process without destructors or FS
                // cleanup. Winning the CAS means ready cannot be sent, so pane
                // ownership remains with the exporter, which can roll back.
                unsafe { libc::_exit(124) };
            }
        })?;
    Ok(ImportWatchdog { state, cancel })
}

#[cfg(unix)]
fn import_deadline_expired(receiver: std::sync::mpsc::Receiver<()>, timeout: Duration) -> bool {
    matches!(
        receiver.recv_timeout(timeout),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    )
}

#[cfg(unix)]
pub(crate) fn cleanup_failed_import_child(child: &mut Child) {
    let pid = child.id();
    match child.try_wait() {
        Ok(Some(status)) => {
            crate::logging::handoff_import_rollback_exited(pid, &status.to_string());
            return;
        }
        Ok(None) => {}
        Err(err) => {
            crate::logging::handoff_import_rollback_step_failed(pid, "inspect", &err.to_string());
        }
    }

    if let Err(err) = child.kill() {
        crate::logging::handoff_import_rollback_step_failed(pid, "kill", &err.to_string());
    }
    let deadline = std::time::Instant::now() + IMPORT_REAP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                crate::logging::handoff_import_rollback_reaped(pid, &status.to_string());
                return;
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                // The child remains unreaped if the kernel has not completed
                // SIGKILL. When it eventually exits it may remain a zombie until
                // this exporter exits and the system reaper adopts it.
                crate::logging::handoff_import_rollback_step_failed(
                    pid,
                    "reap",
                    "timed out waiting for killed importer to exit",
                );
                return;
            }
            Err(err) => {
                crate::logging::handoff_import_rollback_step_failed(pid, "reap", &err.to_string());
                return;
            }
        }
    }
}

#[cfg(unix)]
pub(crate) fn bind_listener(socket_path: &Path) -> io::Result<UnixListener> {
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;
    restrict_socket_permissions(socket_path)?;
    Ok(listener)
}

/// Prefix the importer writes before closing the handoff stream when it
/// refuses the manifest, so the exporter can tell the operator WHY instead of
/// seeing a closed stream. Deliberately a bytes constant so both sides refer
/// to the same string.
#[cfg(unix)]
pub(crate) const IMPORT_REFUSAL_PREFIX: &str = "error: ";

#[cfg(unix)]
pub(crate) fn accept_and_validate_on(
    listener: UnixListener,
    socket_path: &Path,
    token: &str,
    manifest: &HandoffManifest,
) -> io::Result<UnixStream> {
    let (mut stream, _) = accept_with_timeout(&listener, READY_TIMEOUT)?;
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    stream.set_write_timeout(Some(READY_TIMEOUT))?;
    let token_line = read_line_unbuffered(&mut stream)?;
    if token_line.trim_end() != token {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "handoff import token mismatch",
        ));
    }

    serde_json::to_writer(&mut stream, manifest).map_err(io::Error::other)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    expect_line(
        &mut stream,
        "validated",
        "handoff import did not validate manifest",
    )?;
    let _ = std::fs::remove_file(socket_path);
    Ok(stream)
}

/// Read one line from the importer and require it to be `expected`. A line
/// carrying [`IMPORT_REFUSAL_PREFIX`] is the importer naming why it refused,
/// and surfaces as `handoff import refused: <reason>` at whichever step the
/// exporter was waiting on. Any other line is reported under `what`.
#[cfg(unix)]
fn expect_line(stream: &mut UnixStream, expected: &str, what: &str) -> io::Result<()> {
    let line = read_line_unbuffered(&mut *stream)?;
    let trimmed = line.trim_end();
    if trimmed == expected {
        return Ok(());
    }
    if let Some(reason) = trimmed.strip_prefix(IMPORT_REFUSAL_PREFIX) {
        return Err(io::Error::other(format!(
            "handoff import refused: {reason}"
        )));
    }
    Err(io::Error::other(format!(
        "{what}: unexpected response {trimmed:?}"
    )))
}

/// Write a one-line `error: <reason>` back to the handoff stream before the
/// import process exits. The exporter's `accept_and_validate_on` recognises
/// the prefix and lifts the reason into the `handoff_failed` message so an
/// operator sees WHY the import refused, instead of the opaque "handoff
/// stream closed while reading line" that is all it saw before this call
/// existed. Best-effort — if the exporter has already moved on, the write
/// drops.
#[cfg(unix)]
pub(crate) fn report_import_refusal(stream: &mut UnixStream, reason: &str) -> io::Result<()> {
    // One line out of a possibly multi-line error message: embedded newlines
    // would terminate the line reader at the wrong place.
    let single_line: String = reason
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    stream.write_all(IMPORT_REFUSAL_PREFIX.as_bytes())?;
    stream.write_all(single_line.trim().as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn send_fds_and_wait_restored(stream: &mut UnixStream, fds: &[RawFd]) -> io::Result<()> {
    send_fds(stream, fds)?;

    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    expect_line(
        stream,
        "restored",
        "handoff import did not report restored runtimes",
    )
}

#[cfg(unix)]
pub(crate) fn wait_ready(stream: &mut UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    expect_line(stream, "ready", "handoff import did not report ready")
}

#[cfg(unix)]
pub(crate) fn report_committed(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"committed\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn wait_owned_ack(stream: &mut UnixStream) {
    if let Err(err) = stream.set_read_timeout(Some(OWNED_ACK_TIMEOUT)) {
        crate::logging::handoff_owned_ack_setup_failed(&err.to_string());
        return;
    }
    match read_line_unbuffered(&mut *stream) {
        Ok(owned) if owned.trim_end() == "owned" => {}
        Ok(other) => {
            crate::logging::handoff_owned_ack_unexpected(other.trim_end());
        }
        Err(err) => {
            crate::logging::handoff_owned_ack_read_failed(&err.to_string());
        }
    }
}

#[cfg(unix)]
pub(crate) fn receive(socket_path: &Path, token: &str) -> io::Result<ReceivedHandoff> {
    let mut stream = connect_import(socket_path, token)?;

    match receive_after_token(&mut stream) {
        Ok((manifest, fds)) => Ok(ReceivedHandoff {
            manifest,
            fds,
            stream,
        }),
        Err(err) => {
            // Log first: a pre-fix exporter SIGKILLs this process the moment
            // it sees the closed stream, so anything after the write may never
            // run. Stderr is `/dev/null`, so this is the only durable record.
            crate::logging::handoff_import_refused(&err.to_string());
            // Tell the exporter WHY before dropping the stream. Without this
            // line the exporter sees only "handoff stream closed while reading
            // line" and the import's stderr goes to /dev/null, so the real
            // reason — version mismatch, manifest shape, fd exchange — was
            // unobservable from either side.
            if let Err(report_err) = report_import_refusal(&mut stream, &err.to_string()) {
                crate::logging::handoff_refusal_report_failed(&report_err.to_string());
            }
            Err(err)
        }
    }
}

#[cfg(unix)]
fn connect_import(socket_path: &Path, token: &str) -> io::Result<UnixStream> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    stream.set_write_timeout(Some(READY_TIMEOUT))?;
    stream.write_all(token.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    Ok(stream)
}

/// Everything after the token write that may fail without the exporter
/// learning why. Split out so the error-reporting path in [`receive`] has
/// one place to intercept.
#[cfg(unix)]
fn receive_after_token(stream: &mut UnixStream) -> io::Result<(HandoffManifest, Vec<RawFd>)> {
    let manifest_line = read_line_unbuffered(stream)?;
    let manifest: HandoffManifest = serde_json::from_str(&manifest_line)
        .map_err(|err| io::Error::other(format!("handoff manifest deserialize failed: {err}")))?;
    if manifest.version != HANDOFF_VERSION {
        return Err(io::Error::other(format!(
            "unsupported handoff version {}",
            manifest.version
        )));
    }
    if manifest
        .expected_protocol
        .is_some_and(|protocol| protocol != crate::protocol::PROTOCOL_VERSION)
    {
        return Err(io::Error::other(format!(
            "handoff expected protocol {}, but this server speaks protocol {}",
            manifest.expected_protocol.unwrap_or_default(),
            crate::protocol::PROTOCOL_VERSION
        )));
    }
    if manifest
        .expected_version
        .as_deref()
        .is_some_and(|version| version != crate::build_info::version())
    {
        return Err(io::Error::other(format!(
            "handoff expected flock v{}, but this server is v{}",
            manifest.expected_version.as_deref().unwrap_or("unknown"),
            crate::build_info::version()
        )));
    }
    stream.write_all(b"validated\n")?;
    stream.flush()?;
    if cfg!(debug_assertions)
        && std::env::var("FLOCK_TEST_HANDOFF_IMPORT_FAIL").as_deref() == Ok("mid_fds")
    {
        let partial = recv_fds(stream, manifest.panes.len().min(MAX_FDS_PER_BATCH))?;
        close_fds(&partial);
        return Err(io::Error::other(format!(
            "test handoff import failure after {} of {} fds",
            partial.len(),
            manifest.panes.len()
        )));
    }
    let fds = recv_fds(stream, manifest.panes.len())?;
    Ok((manifest, fds))
}

#[cfg(unix)]
pub(crate) fn report_restored(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"restored\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn report_ready(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"ready\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn wait_committed(stream: &mut UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    let committed = read_line_unbuffered(&mut *stream)?;
    if committed.trim_end() != "committed" {
        return Err(io::Error::other("handoff source did not commit"));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn report_owned(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"owned\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn manifest_for(
    snapshot: crate::persist::SessionSnapshot,
    panes: Vec<crate::handoff_runtime::HandoffRuntimeState>,
    expected_protocol: Option<u32>,
    expected_version: Option<String>,
) -> HandoffManifest {
    HandoffManifest {
        version: HANDOFF_VERSION,
        source_version: crate::build_info::version(),
        source_protocol: crate::protocol::PROTOCOL_VERSION,
        store_generation: 0,
        expected_version,
        expected_protocol,
        snapshot,
        panes,
    }
}

#[cfg(unix)]
fn restrict_socket_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(unix)]
fn accept_with_timeout(
    listener: &UnixListener,
    timeout: Duration,
) -> io::Result<(UnixStream, std::os::unix::net::SocketAddr)> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok(accepted) => return Ok(accepted),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out waiting for handoff import connection",
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
}

#[cfg(unix)]
fn read_line_unbuffered(stream: &mut UnixStream) -> io::Result<String> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = stream.read(&mut byte)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "handoff stream closed while reading line",
            ));
        }
        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            return String::from_utf8(bytes)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err));
        }
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handoff line exceeded maximum size",
            ));
        }
    }
}

/// Send every descriptor in SCM_RIGHTS batches of at most
/// [`MAX_FDS_PER_BATCH`], each riding one `F` byte. The caller keeps its own
/// copies: a failure part-way leaves the receiver short, it refuses, and the
/// kernel closes whatever was still in flight when the stream drops.
#[cfg(unix)]
fn send_fds(stream: &UnixStream, fds: &[RawFd]) -> io::Result<()> {
    for batch in fds.chunks(MAX_FDS_PER_BATCH) {
        send_fd_batch(stream, batch)?;
    }
    Ok(())
}

#[cfg(unix)]
fn send_fd_batch(stream: &UnixStream, fds: &[RawFd]) -> io::Result<()> {
    send_tagged_fd_batch(stream, fds, FD_BATCH_TAG)
}

#[cfg(unix)]
fn send_tagged_fd_batch(stream: &UnixStream, fds: &[RawFd], tag: u8) -> io::Result<()> {
    let byte = [tag];
    let iov = [libc::iovec {
        iov_base: byte.as_ptr() as *mut libc::c_void,
        iov_len: byte.len(),
    }];
    let fd_bytes = std::mem::size_of_val(fds);
    let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(fd_bytes as u32) as usize }];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = iov.as_ptr() as *mut libc::iovec;
    msg.msg_iovlen = iov.len() as _;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = control.len() as _;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::other("failed to allocate fd control message"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_bytes as u32) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, libc::CMSG_DATA(cmsg), fd_bytes);
        loop {
            if libc::sendmsg(stream.as_raw_fd(), &msg, 0) >= 0 {
                break;
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
    Ok(())
}

/// Receive `expected` descriptors across as many batches as the sender split
/// them into. Every descriptor taken from the kernel is held in `out` from
/// the moment it arrives, so any refusal, including one part-way through a
/// batch, closes all of them rather than leaking into the importer.
#[cfg(unix)]
fn recv_fds(stream: &UnixStream, expected: usize) -> io::Result<Vec<RawFd>> {
    let mut out = Vec::with_capacity(expected);
    match recv_fds_into(stream, expected, &mut out) {
        Ok(()) => Ok(out),
        Err(err) => {
            close_fds(&out);
            Err(err)
        }
    }
}

#[cfg(unix)]
fn close_fds(fds: &[RawFd]) {
    for &fd in fds {
        let _ = unsafe { libc::close(fd) };
    }
}

#[cfg(unix)]
fn recv_fds_into(stream: &UnixStream, expected: usize, out: &mut Vec<RawFd>) -> io::Result<()> {
    // Sized well past one batch, so a batch carrying more than the remainder,
    // or more than the batch limit, arrives whole and is refused by count.
    // Truncation is not a safe way to refuse: on macOS the kernel installs
    // the descriptors that did not fit into this process without reporting
    // them, and they leak.
    let space = unsafe {
        libc::CMSG_SPACE((RECV_FD_CAPACITY * std::mem::size_of::<RawFd>()) as u32) as usize
    };
    // u64 backing keeps the control buffer aligned for `cmsghdr`.
    let mut control = vec![0u64; space.div_ceil(std::mem::size_of::<u64>())];
    while out.len() < expected {
        let mut byte = [0u8; 1];
        let mut iov = [libc::iovec {
            iov_base: byte.as_mut_ptr() as *mut libc::c_void,
            iov_len: byte.len(),
        }];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = iov.as_mut_ptr();
        msg.msg_iovlen = iov.len() as _;
        msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = space as _;

        let read = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
        if read < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        let before = out.len();
        // Collect before judging the message, so descriptors that did arrive
        // with a malformed or truncated batch are still closed.
        let collected = unsafe { collect_scm_rights(&msg, out) };
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "handoff stream closed after {} of {expected} handoff fds",
                    out.len()
                ),
            ));
        }
        if msg.msg_flags & libc::MSG_CTRUNC != 0 {
            return Err(io::Error::other("handoff fd control message was truncated"));
        }
        collected?;
        if byte[0] != FD_BATCH_TAG {
            return Err(io::Error::other(format!(
                "handoff fd batch carried data byte {:#04x}, expected {:#04x}",
                byte[0], FD_BATCH_TAG
            )));
        }
        let batch = out.len() - before;
        if batch == 0 {
            return Err(io::Error::other("handoff fd message missing SCM_RIGHTS"));
        }
        if batch > MAX_FDS_PER_BATCH {
            return Err(io::Error::other(format!(
                "handoff fd batch of {batch} exceeds the {MAX_FDS_PER_BATCH} fd batch limit"
            )));
        }
        if out.len() > expected {
            return Err(io::Error::other(format!(
                "handoff fd batch of {batch} overruns the {expected} expected handoff fds"
            )));
        }
    }
    Ok(())
}

/// Append every SCM_RIGHTS descriptor in `msg` to `out`. Each payload is
/// bounded by the control length the kernel returned, never by the sender's
/// claimed `cmsg_len` alone, so a header that overstates its size cannot
/// make this read past what was written.
///
/// # Safety
/// `msg` must be the header of a completed `recvmsg` whose control buffer is
/// still alive.
#[cfg(unix)]
unsafe fn collect_scm_rights(msg: &libc::msghdr, out: &mut Vec<RawFd>) -> io::Result<()> {
    let control = msg.msg_control as *const u8;
    // `as _`: msg_controllen is usize on Linux and u32 on macOS.
    let controllen: usize = msg.msg_controllen as _;
    let control_end = control.wrapping_add(controllen);
    let mut result = Ok(());
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(msg) };
    while !cmsg.is_null() {
        let header = cmsg as *const u8;
        let available = (control_end as usize).saturating_sub(header as usize);
        let claimed = unsafe { (*cmsg).cmsg_len as usize };
        let header_len = unsafe { libc::CMSG_LEN(0) as usize };
        if available < header_len || claimed < header_len {
            result = Err(io::Error::other("handoff fd control header is malformed"));
            break;
        }
        let len = claimed.min(available);
        if claimed > available {
            result = Err(io::Error::other(
                "handoff fd control message overstates its length",
            ));
        }
        let is_rights = unsafe {
            (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS
        };
        if is_rights {
            let data = unsafe { libc::CMSG_DATA(cmsg) };
            let data_offset = data as usize - header as usize;
            let count = len.saturating_sub(data_offset) / std::mem::size_of::<RawFd>();
            for idx in 0..count {
                out.push(unsafe { std::ptr::read_unaligned((data as *const RawFd).add(idx)) });
            }
        } else if result.is_ok() {
            result = Err(io::Error::other(
                "handoff fd message carried an unexpected control message",
            ));
        }
        if claimed > available {
            break;
        }
        cmsg = unsafe { libc::CMSG_NXTHDR(msg, cmsg) };
    }
    result
}

#[cfg(unix)]
pub(crate) fn log_import_result(panes: usize) {
    info!(panes, "handoff import ready");
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use std::thread;

    #[test]
    fn import_deadline_disarm_and_expiry_race_has_only_one_winner() {
        for _ in 0..100 {
            let state = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(IMPORT_ARMED));
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let expire_state = state.clone();
            let expire_barrier = barrier.clone();
            let expire = thread::spawn(move || {
                expire_barrier.wait();
                transition_import_deadline(&expire_state, IMPORT_EXPIRED).is_ok()
            });
            barrier.wait();
            let disarmed = transition_import_deadline(&state, IMPORT_DISARMED).is_ok();
            let expired = expire.join().expect("expiry thread");
            assert_ne!(disarmed, expired);
            assert_eq!(
                state.load(std::sync::atomic::Ordering::SeqCst),
                if disarmed {
                    IMPORT_DISARMED
                } else {
                    IMPORT_EXPIRED
                }
            );
        }
    }

    #[test]
    fn expired_importer_cannot_disarm_and_announce_ready() {
        let (cancel, _receiver) = std::sync::mpsc::channel();
        let watchdog = ImportWatchdog {
            state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(IMPORT_EXPIRED)),
            cancel,
        };
        assert_eq!(
            watchdog.disarm().expect_err("already expired").kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn importer_stalled_after_manifest_times_out_receiving_fds() {
        let (mut importer, mut exporter) = UnixStream::pair().expect("socket pair");
        importer
            .set_read_timeout(Some(Duration::from_millis(20)))
            .expect("read timeout");
        exporter
            .write_all(OLD_SHAPE_MANIFEST.as_bytes())
            .expect("manifest");
        exporter.write_all(b"\n").expect("manifest newline");
        let error = receive_after_token(&mut importer)
            .err()
            .expect("missing descriptors");
        assert!(matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ));
    }

    #[test]
    fn wait_committed_returns_eof_when_exporter_closes_stream() {
        let (mut importer, exporter) = UnixStream::pair().expect("socket pair");
        exporter
            .shutdown(std::net::Shutdown::Write)
            .expect("close exporter write side");
        assert_eq!(
            wait_committed(&mut importer)
                .expect_err("exporter exited")
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn import_deadline_expires_while_startup_is_stalled() {
        let (_cancel, receiver) = std::sync::mpsc::channel();
        assert!(import_deadline_expired(receiver, Duration::from_millis(20)));
    }

    #[test]
    fn import_deadline_cancels_after_disarm_or_startup_error() {
        let (cancel, receiver) = std::sync::mpsc::channel();
        cancel.send(()).expect("cancel deadline");
        assert!(!import_deadline_expired(receiver, Duration::from_secs(1)));
        let (cancel, receiver) = std::sync::mpsc::channel();
        drop(cancel);
        assert!(!import_deadline_expired(receiver, Duration::from_secs(1)));
    }

    /// A paired manifest JSON written by `0.6.8-fork.<rev>` for a two-pane
    /// handoff, captured off the wire from an isolated server. The hostnames
    /// in `agent_id` have been replaced with a declared fictional host so
    /// `scripts/fixture_hosts.py` does not blow this up — the shape is what
    /// matters, not the specific id.
    const OLD_SHAPE_MANIFEST: &str = r#"{"version":1,"source_version":"0.6.8-fork.old","source_protocol":25,"expected_version":null,"expected_protocol":null,"snapshot":{"version":3,"workspaces":[{"id":"w1","custom_name":null,"identity_cwd":"/tmp","public_pane_numbers":{"1":1},"next_public_pane_number":2,"public_tab_numbers":[1],"next_public_tab_number":2,"tabs":[{"custom_name":null,"layout":{"Pane":1},"panes":{"1":{"cwd":"/tmp","agent_id":"agent_atlas_old0001","header_reserved":true,"agent_session":{"source":"flock:claude","agent":"claude","kind":"id","value":"abc-123"}}},"zoomed":false,"focused":1,"root_pane":1}],"active_tab":0}],"active":0,"selected":0,"agent_panel_scope":"AllWorkspaces","servers_panel_scope":"All","spaces_panel_scope":"All","sidebar_width":26,"sidebar_section_split":0.5,"collapsed_space_keys":[]},"panes":[{"pane_id":1,"child_pid":4321,"rows":20,"cols":52,"cell_width_px":0,"cell_height_px":0,"keyboard_protocol_flags":0,"input_state":{"alternate_screen":false,"application_cursor":false,"bracketed_paste":false,"focus_reporting":false,"mouse_protocol_mode":"none","mouse_protocol_encoding":"default","mouse_alternate_scroll":true,"modify_other_keys":true}}]}"#;

    #[test]
    fn captured_old_shape_manifest_deserializes_here() {
        // The regression this guards against is the one #600 looked like: a
        // nested type on either side renaming or dropping a field so the
        // import's `serde_json::from_str` fails on a manifest a running old
        // server produced. If that happens again, this test fails with the
        // serde error path pointing at the field — a long way from the
        // opaque "handoff stream closed while reading line" the operator
        // sees today.
        let manifest: HandoffManifest = serde_json::from_str(OLD_SHAPE_MANIFEST)
            .expect("old-shape handoff manifest must deserialize");
        assert_eq!(manifest.version, HANDOFF_VERSION);
        assert_eq!(manifest.source_protocol, 25);
        assert_eq!(manifest.panes.len(), 1);
        assert_eq!(manifest.snapshot.workspaces.len(), 1);
    }

    #[test]
    fn pinned_version_mismatch_reaches_the_exporter_as_a_named_refusal() {
        // The #600 shape: a remote attach pins `--expected-version` to the
        // CLIENT'S build, and the binary at the import path is another build.
        // The importer refuses before `validated`, so the exporter used to see
        // only "handoff stream closed while reading line". Driven through the
        // real exporter and importer halves over a real listener.
        let mut manifest: HandoffManifest = serde_json::from_str(OLD_SHAPE_MANIFEST)
            .expect("old-shape handoff manifest must deserialize");
        manifest.expected_version = Some("0.0.0-pinned-elsewhere".to_string());
        let socket = std::env::temp_dir().join(format!("flk-h600-{}.sock", std::process::id()));
        let listener = bind_listener(&socket).expect("bind handoff listener");
        let token = "tok-600";
        let importer_socket = socket.clone();
        let importer = thread::spawn(move || receive(&importer_socket, token).map(|_| ()));

        let err = accept_and_validate_on(listener, &socket, token, &manifest)
            .expect_err("a pinned version that is not this build must be refused");
        let importer_err = importer
            .join()
            .expect("importer thread does not panic")
            .expect_err("importer refuses the pinned version");
        let _ = std::fs::remove_file(&socket);

        let message = err.to_string();
        assert!(
            message.starts_with("handoff import refused: "),
            "exporter must name the refusal, got {message:?}"
        );
        assert!(
            message.contains("0.0.0-pinned-elsewhere"),
            "refusal must carry the pinned version, got {message:?}"
        );
        assert!(importer_err.to_string().contains("expected flock v"));
    }

    #[test]
    fn refusal_where_restored_is_expected_surfaces_as_a_named_refusal() {
        let (mut importer_side, mut exporter_side) =
            UnixStream::pair().expect("socketpair available");
        report_import_refusal(&mut importer_side, "pane restore failed")
            .expect("refusal line writes");
        exporter_side
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout settable");
        let err = expect_line(&mut exporter_side, "restored", "did not restore")
            .expect_err("a refusal is not `restored`");
        assert_eq!(
            err.to_string(),
            "handoff import refused: pane restore failed"
        );

        let (mut importer_side, mut exporter_side) =
            UnixStream::pair().expect("socketpair available");
        importer_side.write_all(b"bogus\n").expect("write");
        let err = expect_line(&mut exporter_side, "restored", "did not restore")
            .expect_err("an unexpected line is an error");
        assert_eq!(
            err.to_string(),
            "did not restore: unexpected response \"bogus\""
        );
    }

    #[test]
    fn importer_refusal_is_reported_to_exporter_as_handoff_import_refused() {
        // Simulates what the exporter sees when the importer refuses. The
        // importer-side writes `error: <reason>` back on the stream before
        // dropping it; the exporter's wait for "validated" must lift that
        // reason into the final error. Without this round trip the operator
        // sees only "handoff stream closed while reading line" (#600).
        let (mut importer_side, exporter_side) =
            UnixStream::pair().expect("socketpair available in test");
        let importer = thread::spawn(move || {
            report_import_refusal(&mut importer_side, "expected flock v0.8.0 but got v0.6.8")
                .expect("refusal line writes to the stream");
        });

        let mut exporter_side = exporter_side;
        exporter_side
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout settable on a socketpair");
        let mut received = String::new();
        exporter_side
            .read_to_string(&mut received)
            .expect("exporter reads the refusal line before EOF");
        importer.join().expect("importer thread exits cleanly");

        assert!(
            received.starts_with(IMPORT_REFUSAL_PREFIX),
            "exporter should see the refusal prefix, got {received:?}"
        );
        let reason = received
            .trim_end()
            .strip_prefix(IMPORT_REFUSAL_PREFIX)
            .expect("prefix present");
        assert_eq!(reason, "expected flock v0.8.0 but got v0.6.8");
    }

    #[test]
    fn importer_refusal_single_lines_newlines_in_reason() {
        // serde_json error messages ride onto the same read-line protocol as
        // "validated" / "restored" / "committed", so an embedded `\n` would
        // terminate the line early and the exporter would read half the
        // reason. The refusal writer collapses newlines to spaces for that
        // reason, and this is where that collapse is pinned.
        let (mut importer_side, mut exporter_side) =
            UnixStream::pair().expect("socketpair available");
        exporter_side
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout settable before peer closes");
        report_import_refusal(
            &mut importer_side,
            "line one\nline two with\r\nembedded newlines",
        )
        .expect("refusal line writes");
        drop(importer_side);

        let mut received = String::new();
        exporter_side
            .read_to_string(&mut received)
            .expect("exporter reads the refusal line");
        // Exactly one trailing newline, no embedded CR/LF inside the reason.
        assert_eq!(received.matches('\n').count(), 1);
        assert!(received.ends_with('\n'));
        assert!(!received[..received.len() - 1].contains('\n'));
    }

    /// `count` pipes. The write ends travel through the handoff; the read
    /// ends stay here so [`assert_all_write_ends_closed`] can prove no copy of
    /// a write end survived anywhere in this process.
    fn pipes(count: usize) -> (Vec<RawFd>, Vec<RawFd>) {
        let mut reads = Vec::new();
        let mut writes = Vec::new();
        for _ in 0..count {
            let mut pair = [0; 2];
            assert_eq!(unsafe { libc::pipe(pair.as_mut_ptr()) }, 0, "pipe");
            let flags = unsafe { libc::fcntl(pair[0], libc::F_GETFL) };
            assert_eq!(
                unsafe { libc::fcntl(pair[0], libc::F_SETFL, flags | libc::O_NONBLOCK) },
                0
            );
            reads.push(pair[0]);
            writes.push(pair[1]);
        }
        (reads, writes)
    }

    /// A read end returns EOF only once every copy of its write end is
    /// closed, including copies the kernel delivered to a receiver. So EOF on
    /// all of them is the no-leak proof, and `EAGAIN` names a leaked one.
    fn assert_all_write_ends_closed(reads: &[RawFd]) {
        for (idx, &fd) in reads.iter().enumerate() {
            let mut byte = 0u8;
            let read = unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) };
            assert_eq!(
                read,
                0,
                "pipe {idx}: a write end leaked ({})",
                io::Error::last_os_error()
            );
        }
        close_fds(reads);
    }

    fn pair_with_timeout() -> (UnixStream, UnixStream) {
        let (sender, receiver) = UnixStream::pair().expect("socket pair");
        receiver
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        (sender, receiver)
    }

    /// Count the SCM_RIGHTS messages `send_fds` emits for `count` fds by
    /// reading them back one recvmsg at a time with room for far more than a
    /// batch, so a batching change cannot hide inside a small buffer.
    fn batch_sizes_on_the_wire(count: usize) -> Vec<usize> {
        let (_reads, writes) = pipes(count);
        let (sender, receiver) = pair_with_timeout();
        send_fds(&sender, &writes).expect("send");
        drop(sender);
        let mut sizes = Vec::new();
        let space = unsafe { libc::CMSG_SPACE((1024 * std::mem::size_of::<RawFd>()) as u32) };
        let mut control = vec![0u64; space as usize / 8 + 1];
        loop {
            let mut byte = [0u8; 4];
            let mut iov = [libc::iovec {
                iov_base: byte.as_mut_ptr().cast(),
                iov_len: byte.len(),
            }];
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_iov = iov.as_mut_ptr();
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = space as _;
            let read = unsafe { libc::recvmsg(receiver.as_raw_fd(), &mut msg, 0) };
            if read <= 0 {
                break;
            }
            assert_eq!(read, 1, "each batch rides exactly one byte");
            assert_eq!(byte[0], b'F');
            let mut got = Vec::new();
            unsafe { collect_scm_rights(&msg, &mut got) }.expect("well-formed batch");
            sizes.push(got.len());
            close_fds(&got);
        }
        close_fds(&writes);
        sizes
    }

    #[test]
    fn a_handoff_of_up_to_one_batch_is_one_message_and_more_is_split_by_64() {
        assert_eq!(batch_sizes_on_the_wire(1), vec![1]);
        assert_eq!(batch_sizes_on_the_wire(64), vec![64]);
        assert_eq!(batch_sizes_on_the_wire(65), vec![64, 1]);
        assert_eq!(batch_sizes_on_the_wire(150), vec![64, 64, 22]);
    }

    #[test]
    fn receiver_accumulates_150_fds_across_batches_in_order() {
        let (reads, writes) = pipes(150);
        let (sender, receiver) = pair_with_timeout();
        let sent = writes.clone();
        let send = thread::spawn(move || send_fds(&sender, &sent));
        let received = recv_fds(&receiver, 150).expect("all 150 fds");
        send.join().expect("sender").expect("send");
        assert_eq!(received.len(), 150);
        close_fds(&writes);
        // Each received fd must be the write end of the pipe at the same
        // index: the importer pairs fds with manifest panes by position.
        for (idx, &fd) in received.iter().enumerate() {
            let tag = idx as u8;
            assert_eq!(unsafe { libc::write(fd, (&tag as *const u8).cast(), 1) }, 1);
            let mut got = 0u8;
            assert_eq!(
                unsafe { libc::read(reads[idx], (&mut got as *mut u8).cast(), 1) },
                1
            );
            assert_eq!(got, tag, "fd {idx} arrived out of order");
        }
        close_fds(&received);
        assert_all_write_ends_closed(&reads);
    }

    #[test]
    fn over_count_batch_is_refused_and_every_held_fd_closed() {
        let (reads, writes) = pipes(3);
        let (sender, receiver) = pair_with_timeout();
        send_fd_batch(&sender, &writes).expect("send three");
        close_fds(&writes);
        let err = recv_fds(&receiver, 2).expect_err("three fds where two were expected");
        assert!(err.to_string().contains("overruns"), "{err}");
        drop(sender);
        drop(receiver);
        assert_all_write_ends_closed(&reads);
    }

    #[test]
    fn over_count_batch_after_earlier_batches_closes_the_earlier_ones_too() {
        let (reads, writes) = pipes(70);
        let (sender, receiver) = pair_with_timeout();
        send_fd_batch(&sender, &writes[..64]).expect("first batch");
        send_fd_batch(&sender, &writes[64..]).expect("six where one is left");
        close_fds(&writes);
        let err = recv_fds(&receiver, 65).expect_err("overrun in the second batch");
        assert!(err.to_string().contains("overruns"), "{err}");
        drop(sender);
        drop(receiver);
        assert_all_write_ends_closed(&reads);
    }

    #[test]
    fn batch_larger_than_the_limit_is_refused_without_leaking() {
        let (reads, writes) = pipes(MAX_FDS_PER_BATCH + 1);
        let (sender, receiver) = pair_with_timeout();
        send_fd_batch(&sender, &writes).expect("one oversized batch");
        close_fds(&writes);
        let err = recv_fds(&receiver, 200).expect_err("65 fds in one batch");
        assert!(err.to_string().contains("batch limit"), "{err}");
        drop(sender);
        drop(receiver);
        assert_all_write_ends_closed(&reads);
    }

    #[test]
    fn batch_on_a_byte_other_than_f_is_refused_and_its_fds_closed() {
        let (reads, writes) = pipes(2);
        let (sender, receiver) = pair_with_timeout();
        send_tagged_fd_batch(&sender, &writes, b'X').expect("send");
        close_fds(&writes);
        let err = recv_fds(&receiver, 2).expect_err("wrong data byte");
        assert!(err.to_string().contains("data byte 0x58"), "{err}");
        drop(sender);
        drop(receiver);
        assert_all_write_ends_closed(&reads);
    }

    #[test]
    fn sender_dying_mid_transfer_closes_every_fd_already_received() {
        let (reads, writes) = pipes(150);
        let (sender, receiver) = pair_with_timeout();
        send_fds(&sender, &writes[..100]).expect("first 100 of 150");
        drop(sender);
        close_fds(&writes);
        let err = recv_fds(&receiver, 150).expect_err("stream ends short");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof, "{err}");
        assert!(err.to_string().contains("100 of 150"), "{err}");
        drop(receiver);
        assert_all_write_ends_closed(&reads);
    }

    #[test]
    fn scm_rights_payload_is_bounded_by_the_kernel_control_length() {
        // A header claiming 64 descriptors inside a control buffer the kernel
        // reported as holding only two must yield at most those two, never
        // the 62 that follow in memory.
        let space = unsafe { libc::CMSG_SPACE((64 * std::mem::size_of::<RawFd>()) as u32) };
        let mut control = vec![0u64; space as usize / 8 + 1];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN((64 * std::mem::size_of::<RawFd>()) as u32) as _;
            let data = libc::CMSG_DATA(cmsg) as *mut RawFd;
            for idx in 0..64 {
                data.add(idx).write_unaligned(-1 - idx as RawFd);
            }
        }
        msg.msg_controllen =
            unsafe { libc::CMSG_LEN((2 * std::mem::size_of::<RawFd>()) as u32) } as _;
        let mut out = Vec::new();
        let result = unsafe { collect_scm_rights(&msg, &mut out) };
        assert!(result.is_err(), "an overstated cmsg_len must be refused");
        assert_eq!(out, vec![-1, -2]);
    }
}
