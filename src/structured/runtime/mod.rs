//! Worker-owned structured process assembly (ADR-0036 §1, §1a, §4).
//!
//! `run` belongs on a dedicated worker. Published handle operations never wait
//! for pipes, disk, queues or a mutex. Preparation is not init readiness: registry
//! refusal runs immediately before first stdin bytes; init refusal follows input.
//! Only process lifecycle is interpreted here; result events remain turn facts.
#![deny(missing_docs)]

use super::launch::{
    registry_has_messaging_socket, LaunchError, LaunchProfile, SessionConfigDir, VerifiedBinary,
};
use super::supervision::{self, StopRequest};
use super::transport::{
    Completion, InputHandle, Received, RecordOrigin, StopHandle, StreamLocation, Transport,
};
use crate::{platform, process::TracedCommand};
use serde_json::Value;
use std::io;
use std::path::Path;
use std::process::{Child, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const TICK: Duration = Duration::from_millis(2);
const VERSION_LIMIT: Duration = Duration::from_secs(2);
const IO_LIMIT: Duration = Duration::from_secs(5);
const CANCEL_LIMIT: Duration = Duration::from_secs(2);

/// Process lifecycle, independent of successful/failed/unknown turn outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Successful process exit with no input awaiting a result.
    Exited,
    /// Requested stop with confirmed process and I/O termination and cleanup.
    Stopped,
    /// Unexpected nonzero exit, or any exit with input lacking a result.
    Crashed,
    /// Registry/init socket evidence or an unreadable registry refused the session.
    Refused,
    /// Transport, ownership, worker or cleanup failure; files may be retained.
    Failed,
}

/// Terminal process facts. Never infer a successful turn from the exit status.
#[derive(Debug)]
pub struct Exit {
    /// Lifecycle interpretation, with refusal/failure taking priority over stop.
    pub outcome: Outcome,
    /// Actual reaped status; None means termination could not be confirmed.
    pub status: Option<ExitStatus>,
    /// Explicit refusal and/or cleanup failure, without credential values.
    pub error: Option<String>,
    /// Accepted input still lacks a top-level result, including an Unknown result.
    pub input_inflight: bool,
    /// Notification delivery was cancelled; replay durable records for completeness.
    pub delivery_cancelled: bool,
}

/// Server-facing session handle; no public raw/control stdin bypass exists.
pub struct Runtime {
    transport: InputHandle,
    stop: StopRequest,
    control: StopHandle,
    events: mpsc::Receiver<Received>,
}
impl Runtime {
    /// Offer a bounded user envelope only while idle; WouldBlock never waits.
    /// Successful admission emits an internal accepted-input fact through
    /// `Received::state_input`, arming S4 silence observation before started.
    /// Success acknowledges admission, not delivery or provider execution.
    /// Cancellation errors also request process shutdown, not just pipe closure.
    /// `structured_turn_end_unknown` means loss concealed the boundary; await a
    /// subsequent result or explicitly stop, rather than retry as ordinary busy.
    pub fn try_submit(&self, user: &Value) -> io::Result<()> {
        let result = self.transport.try_submit(user);
        if result.is_err() && self.control.cancelled() {
            self.stop.request();
        }
        result
    }
    /// Server entry for a dispatcher-authenticated agent.decide command.
    /// Token identity is context, never a field decoded from command arguments.
    pub fn agent_decide(
        &self,
        principal: &super::permissions::Principal<'_>,
        args: &super::permissions::AgentDecide,
    ) -> Result<(), crate::api::error_code::ErrorCode> {
        self.transport.agent_decide(principal, args)
    }
    /// Audit a host cancellation/result/interrupt without sending a tool decision.
    /// The caller must carry an authenticated operate-token capability.
    pub fn close_permissions(
        &self,
        principal: &super::permissions::Principal<'_>,
        id: Option<&str>,
        reason: super::permissions::CloseReason,
    ) -> Result<(), crate::api::error_code::ErrorCode> {
        self.transport.close_permissions(principal, id, reason)
    }
    /// Current permission ids for S4 attention; a busy snapshot fails closed.
    pub fn waiting_on_human(&self) -> Result<Vec<String>, crate::api::error_code::ErrorCode> {
        self.transport.waiting_on_human()
    }
    /// Poll persisted events. Empty is not EOF; monitor the owning worker result too.
    /// Feed `Received::state_input` into S4 so book closures clear attention.
    pub fn try_receive(&self) -> Result<Received, mpsc::TryRecvError> {
        self.events.try_recv()
    }
    /// Wait for a persisted event on a worker, with a finite deadline.
    /// Server-loop callers must use `try_receive` instead.
    pub fn receive_timeout(&self, timeout: Duration) -> Result<Received, mpsc::RecvTimeoutError> {
        self.events.recv_timeout(timeout)
    }
    /// Record intent before cancellation/closure/signals; never join workers here.
    pub fn stop(&self) {
        self.stop.request();
        self.control.cancel();
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Run an already-admitted launch profile until termination on an owning worker.
/// Publish the handle through `ready` without blocking; publication panics trigger
/// owned shutdown. Default run-as admission belongs to LaunchProfile preparation.
/// Recheck the exact absolute executable with a timed version reader before spawn.
/// Path replacement between check and exec remains possible; only fexecve closes
/// that residual TOCTOU. Registry parent verification also has a residual path
/// lookup window; see `registry_has_messaging_socket`. Forward the returned exit
/// through the caller's event channel. This API does not expose agent.start or
/// reserve a lease; the caller must reserve before preparing the launch.
pub fn run(
    state: &Path,
    profile: LaunchProfile,
    lease: super::lease::Lease,
    location: StreamLocation,
    ready: impl FnOnce(Runtime),
) -> Result<Exit, LaunchError> {
    run_notified(state, profile, lease, location, ready, || {})
}

/// Worker assembly with a nonblocking wake after each queued stream notification.
/// Ready/completion notifications remain the launch owner's responsibility.
/// The callback must not perform I/O or wait for queue capacity.
pub(crate) fn run_notified(
    state: &Path,
    profile: LaunchProfile,
    lease: super::lease::Lease,
    location: StreamLocation,
    ready: impl FnOnce(Runtime),
    notify: impl Fn() + Send + 'static,
) -> Result<Exit, LaunchError> {
    run_controlled(
        state,
        profile,
        lease,
        location,
        ready,
        notify,
        StopRequest::default(),
    )
}

/// Worker-owned cancellation exists before spawn, including deadline escalation.
pub(crate) fn run_controlled(
    state: &Path,
    profile: LaunchProfile,
    mut lease: super::lease::Lease,
    location: StreamLocation,
    ready: impl FnOnce(Runtime),
    notify: impl Fn() + Send + 'static,
    stop: StopRequest,
) -> Result<Exit, LaunchError> {
    if !lease.matches(&location.agent, &location.session)
        || profile.session_id() != location.session
    {
        return Err(io::Error::other("structured lease/stream mismatch").into());
    }
    for entry in supervision::sweep_stale_sessions(state)? {
        crate::logging::structured_sweep_outcome(&entry);
    }
    let checked = VerifiedBinary::check(profile.binary(), |path, args| {
        let mut command = TracedCommand::new(path, "structured-version");
        command.args(args).env_clear().stdin(Stdio::null());
        profile.configure_environment(|key, value| {
            if key != "CLAUDE_CODE_OAUTH_TOKEN" {
                command.env(key, value);
            }
        });
        let output = command.output_traced_with_timeout(VERSION_LIMIT)?;
        if !output.status.success() {
            return Err(io::Error::other("harness version command failed"));
        }
        String::from_utf8(output.stdout).map_err(io::Error::other)
    })?;
    let version = checked.version().to_owned();
    let mut command = TracedCommand::new(checked.path(), "structured");
    command
        .args(profile.args())
        .env_clear()
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    profile.configure_environment(|key, value| {
        command.env(key, value);
    });
    if let Some(cwd) = profile.cwd() {
        command.current_dir(cwd);
    }
    let (mut child, profile) = match profile.try_spawn(|| lease.spawn(|| command.spawn_traced())) {
        Ok(started) => started,
        Err(error) => {
            lease.release();
            return Err(error.into());
        }
    };
    let config = profile.config_dir().to_owned();
    let files = profile.into_session_files();
    let pid = child.id();
    if let Ok(identity) = supervision::ProcessIdentity::capture(pid) {
        stop.track(&[identity]);
    }
    if let Err(error) = lease.bind(pid) {
        return Err(startup_failed(
            child, files, config, error, true, &mut lease,
        ));
    }
    let registry = config.clone();
    let (stdout, stdin) = match platform::structured::child_pipes(&mut child) {
        Ok(pipes) => pipes,
        Err(error) => {
            return Err(startup_failed(
                child, files, config, error, true, &mut lease,
            ))
        }
    };
    let transport = Transport::start_guarded(stdout, stdin, location, version, move || {
        match registry_has_messaging_socket(&registry, pid) {
            Ok(false) => Ok(()),
            Ok(true) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "structured_messaging_socket_forbidden",
            )),
            Err(error) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("structured_registry_refused: {error}"),
            )),
        }
    });
    let transport = match transport {
        Ok(transport) => transport,
        Err(error) => {
            return Err(startup_failed(
                child, files, config, error, false, &mut lease,
            ))
        }
    };
    let control = transport.stop_handle();
    let input = transport.input_handle();
    let (events_tx, events) = mpsc::sync_channel(4);
    let (io_tx, io_rx) = mpsc::sync_channel(1);
    let (pump_stop, pump_control) = (stop.clone(), control.clone());
    if let Err(error) = std::thread::Builder::new()
        .name("session-events".into())
        .spawn(move || {
            let facts = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                collect(&transport, &pump_stop, &pump_control, events_tx, &notify)
            }))
            .unwrap_or_else(|_| {
                pump_stop.request();
                pump_control.cancel();
                Facts {
                    error: Some(io::Error::other("session event worker panicked")),
                    ..Facts::default()
                }
            });
            let _ = io_tx.try_send(facts);
        })
    {
        control.cancel();
        return Err(startup_failed(
            child, files, config, error, false, &mut lease,
        ));
    }
    let publication_failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ready(Runtime {
            transport: input,
            stop: stop.clone(),
            control: control.clone(),
            events,
        })
    }))
    .is_err();
    if publication_failed {
        stop.request();
        control.cancel();
    }
    let mut facts = Facts::default();
    let report = supervision::supervise(
        child,
        files,
        config,
        stop.clone(),
        || control.close_stdin(),
        || {
            control.close_stdin();
            match io_rx.recv_timeout(IO_LIMIT) {
                Ok(received) => facts = received,
                Err(error) => {
                    control.cancel();
                    facts = io_rx.recv_timeout(CANCEL_LIMIT).map_err(|cancel_error| {
                        io::Error::other(format!(
                            "session I/O completion failed: {error}; cancellation: {cancel_error}"
                        ))
                    })?;
                    facts.delivery_cancelled = true;
                }
            }
            if publication_failed {
                return Err(io::Error::other("session publication panicked"));
            }
            facts.error.take().map_or(Ok(()), Err)
        },
        supervision::Schedule::default(),
    );
    control.cancel();
    if report.cleanup.is_ok() {
        lease.release();
    }
    let cleanup = report.cleanup.err().map(|error| error.to_string());
    let outcome = if facts.refusal.is_some() {
        Outcome::Refused
    } else if cleanup.is_some() {
        Outcome::Failed
    } else if report.stop_requested {
        Outcome::Stopped
    } else if facts.working || !report.status.is_some_and(|status| status.success()) {
        Outcome::Crashed
    } else {
        Outcome::Exited
    };
    let error = match (facts.refusal, cleanup) {
        (Some(refusal), Some(cleanup)) => Some(format!("{refusal}; cleanup: {cleanup}")),
        (refusal, cleanup) => refusal.or(cleanup),
    };
    Ok(Exit {
        outcome,
        status: report.status,
        error,
        input_inflight: facts.working,
        delivery_cancelled: facts.delivery_cancelled,
    })
}

fn startup_failed(
    child: Child,
    files: SessionConfigDir,
    config: std::path::PathBuf,
    error: io::Error,
    io_confirmed: bool,
    lease: &mut super::lease::Lease,
) -> LaunchError {
    let stop = StopRequest::default();
    stop.request();
    // Missing child pipes precede all I/O workers. A partially started transport
    // has no completion receiver, so its uncertain ownership retains files.
    let report = supervision::supervise(
        child,
        files,
        config,
        stop,
        || {},
        || {
            if io_confirmed {
                Ok(())
            } else {
                Err(io::Error::other("startup I/O ownership unconfirmed"))
            }
        },
        supervision::Schedule::default(),
    );
    if report.cleanup.is_ok() {
        lease.release();
    }
    let cleanup = match report.cleanup {
        Ok(()) => "cleanup confirmed".to_owned(),
        Err(error) => format!("cleanup failed: {error}"),
    };
    LaunchError::Io(io::Error::other(format!("{error}; {cleanup}")))
}

#[derive(Default)]
struct Facts {
    working: bool,
    error: Option<io::Error>,
    refusal: Option<String>,
    delivery_cancelled: bool,
}

/// E1 keeps successive turns (and init events) in one process; E16 binds its
/// messaging socket at startup. After a clean first init and the pre-input
/// registry guard, a lossy later line cannot introduce a new socket for that
/// process. Keep its data-loss evidence; still refuse every inspectable socket
/// report, including subsequent init events (ADR-0036 E1/E16).
fn collect(
    transport: &Transport,
    stop: &StopRequest,
    control: &StopHandle,
    output: mpsc::SyncSender<Received>,
    notify: &impl Fn(),
) -> Facts {
    let mut facts = Facts::default();
    let mut done = [false; 2];
    let mut pending = None;
    let mut init_inspected = false;
    loop {
        loop {
            let completion = match transport.try_completion() {
                Ok(completion) => completion,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) if done == [true; 2] => break,
                Err(error) => {
                    stop.request();
                    control.cancel();
                    facts.error = Some(io::Error::other(format!(
                        "transport completion missing: {error}"
                    )));
                    return facts;
                }
            };
            let (index, result) = match completion {
                Completion::Reader(result) => (0, result),
                Completion::Writer(result) => (1, result),
            };
            done[index] = true;
            if let Err(error) = result {
                if error.kind() == io::ErrorKind::Interrupted
                    && error.raw_os_error().is_none()
                    && control.cancelled()
                {
                    continue;
                }
                if index == 1 && error.kind() == io::ErrorKind::PermissionDenied {
                    facts.refusal.get_or_insert(error.to_string());
                } else {
                    facts.error.get_or_insert(error);
                }
                stop.request();
                control.cancel();
            }
        }
        facts.working = transport.input_inflight();
        facts.delivery_cancelled |= control.cancelled();
        if control.cancelled() {
            pending = None;
        }
        if pending.is_none() {
            match transport.try_receive() {
                Ok(received) => {
                    let init = serde_json::from_str::<Value>(&received.record.raw)
                        .ok()
                        .filter(|value| value["type"] == "system" && value["subtype"] == "init");
                    let forbidden = init.as_ref().is_some_and(|value| {
                        value
                            .as_object()
                            .is_some_and(|object| object.contains_key("messaging_socket_path"))
                    });
                    let uninspectable = received.record.origin == RecordOrigin::Flk
                        && !init_inspected
                        && !matches!(
                            received.state_input(),
                            Some(super::state::Input::Accepted { .. })
                        );
                    if init.is_some() && !forbidden {
                        init_inspected = true;
                    }
                    if uninspectable || forbidden {
                        facts.refusal = Some(
                            if uninspectable {
                                "structured_uninspectable_output"
                            } else {
                                "structured_messaging_socket_forbidden"
                            }
                            .into(),
                        );
                        stop.request();
                        control.cancel();
                        facts.delivery_cancelled = true;
                    }
                    if !control.cancelled() {
                        pending = Some(received);
                    }
                }
                Err(_) if done == [true; 2] => return facts,
                Err(_) => (),
            }
        }
        if let Some(received) = pending.take() {
            match output.try_send(received) {
                Ok(()) => notify(),
                Err(mpsc::TrySendError::Full(received)) => pending = Some(received),
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    stop.request();
                    control.cancel();
                    facts.delivery_cancelled = true;
                }
            }
        }
        std::thread::sleep(TICK);
    }
}

#[cfg(test)]
pub(crate) mod tests;
