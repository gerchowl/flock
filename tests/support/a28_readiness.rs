//! a28's one-request-per-connection, newline-JSON API observer.
//!
//! Before freezing, responses pass through unchanged. During the freeze every
//! request reaches the real backend, but no response is forwarded: backend
//! readability is a failure. Client EOF proves the CLI abandoned each request.
//! A single shared failure-only deadline bounds all socket and channel waits.
//! After CLI exit the listener drains its backlog and joins its workers before
//! sealing the proof, so delayed EOF observations or extra requests cannot hide.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::delegate_timeouts::REQUEST_TIMEOUT;

use super::support::readiness::{read_socket_line, Stages};

/// The product's documented frozen-readiness path: subscription, diagnostics,
/// then rollback identity. An extra request, retry or different order fails.
const FROZEN_METHODS: [&str; 4] = [
    "events.subscribe",
    "pane.read",
    "agent.get",
    "workspace.get",
];

/// Ordered startup evidence preceding the frozen-server proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// The backend committed the agent, before its reply reaches the CLI.
    StartResponseCaptured,
    /// The proxy received the readiness subscription, held until SIGSTOP.
    ReadinessRequested,
}

/// Proxy and child-liveness evidence, consumed by the strict proof machine.
#[derive(Debug)]
pub enum Event {
    /// The startup protocol reached the named phase.
    Phase(Phase),
    /// The next frozen request arrived, indexed by FROZEN_METHODS.
    Requested(usize),
    /// Client EOF, with elapsed time since the causal agent.start response release.
    Abandoned(usize, Duration),
    /// The owned CLI terminated, without reaping or releasing its PID.
    Exited,
    /// CLI exit was followed by backlog drain and every worker joining.
    Sealed(usize),
    /// A proxy worker or child observer failed instead of producing evidence.
    Failed(String),
}

/// Own all proxy workers and hold the readiness request until SIGSTOP.
/// A separate seal wakeup leaves worker cancellation untouched while draining.
pub struct ApiProxy {
    path: PathBuf,
    cancel: UnixStream,
    seal: UnixStream,
    permission: Option<mpsc::Sender<()>>,
    supervisor: Option<JoinHandle<()>>,
    frozen: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

impl ApiProxy {
    /// Bind only the socket passed to this fixture's observed CLI.
    pub fn new(
        backend: &Path,
        path: PathBuf,
        notice: mpsc::Sender<Event>,
        stages: Arc<Stages>,
    ) -> Self {
        let listener = UnixListener::bind(&path).expect("bind readiness proxy");
        let (cancel, stop) = UnixStream::pair().expect("proxy cancellation pipe");
        let (seal, finish) = UnixStream::pair().expect("proxy seal pipe");
        let (permission, permitted) = mpsc::channel();
        let permitted = Arc::new(Mutex::new(Some(permitted)));
        let backend = backend.to_path_buf();
        let frozen = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::new(AtomicBool::new(false));
        let start_released = Arc::new(Mutex::new(None));
        let worker_frozen = frozen.clone();
        let worker_cancelled = cancelled.clone();
        let supervisor = thread::spawn(move || {
            let mut workers = Vec::new();
            let mut count = 0;
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> io::Result<bool> {
                    let mut finishing = false;
                    loop {
                        if !finishing {
                            match stages.readable(&[
                                stop.as_raw_fd(),
                                finish.as_raw_fd(),
                                listener.as_raw_fd(),
                            ])? {
                                0 => return Ok(false),
                                1 => {
                                    finishing = true;
                                    listener.set_nonblocking(true)?;
                                }
                                _ => {}
                            }
                        }
                        let (mut client, _) = match listener.accept() {
                            Ok(peer) => peer,
                            Err(error)
                                if finishing && error.kind() == io::ErrorKind::WouldBlock =>
                            {
                                return Ok(true)
                            }
                            Err(error) => return Err(error),
                        };
                        // Read headers in accept order. Worker scheduling must not
                        // reorder the sequence after the CLI abandons a request.
                        let request = line(&mut client, &stop, &stages)?;
                        let value: serde_json::Value = serde_json::from_str(&request)?;
                        let method = value["method"]
                            .as_str()
                            .ok_or_else(|| invalid("request has no method"))?;
                        let ready =
                            method == "events.subscribe" && value["id"] == "cli:delegate:ready";
                        let index =
                            if ready || worker_frozen.load(Ordering::Acquire) {
                                if FROZEN_METHODS.get(count) != Some(&method) {
                                    return Err(invalid(format!(
                                        "unexpected frozen request {count}: {method}"
                                    )));
                                }
                                let index = count;
                                count += 1;
                                if ready {
                                    send(&notice, Event::Phase(Phase::ReadinessRequested))?;
                                } else {
                                    send(&notice, Event::Requested(index))?;
                                }
                                Some((
                                    index,
                                    start_released.lock().expect("release lock").ok_or_else(
                                        || invalid("readiness preceded agent.start response"),
                                    )?,
                                ))
                            } else {
                                None
                            };
                        let backend = backend.clone();
                        let stop = stop.try_clone()?;
                        let notice = notice.clone();
                        let permitted = permitted.clone();
                        let cancelled = worker_cancelled.clone();
                        let stages = stages.clone();
                        let start_released = start_released.clone();
                        workers.push(thread::spawn(move || {
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    forward(
                                        client,
                                        request,
                                        &backend,
                                        &stop,
                                        &notice,
                                        &permitted,
                                        index,
                                        &start_released,
                                        &stages,
                                    )
                                }));
                            if !cancelled.load(Ordering::Acquire) {
                                let error = match result {
                                    Ok(Ok(())) => None,
                                    Ok(Err(error)) => Some(error.to_string()),
                                    Err(_) => Some("API proxy worker panicked".into()),
                                };
                                if let Some(error) = error {
                                    let _ = notice.send(Event::Failed(error));
                                }
                            }
                        }));
                    }
                }));
            let sealed = matches!(result, Ok(Ok(true)));
            if !worker_cancelled.load(Ordering::Acquire) && !sealed {
                let reason = match result {
                    Ok(Err(error)) => error.to_string(),
                    Err(_) => "API proxy supervisor panicked".into(),
                    _ => "API proxy stopped before sealing".into(),
                };
                let _ = notice.send(Event::Failed(reason));
            }
            for worker in workers {
                worker.join().expect("proxy worker join");
            }
            if sealed {
                let _ = notice.send(Event::Sealed(count));
            }
        });
        Self {
            path,
            cancel,
            seal,
            permission: Some(permission),
            supervisor: Some(supervisor),
            frozen,
            cancelled,
        }
    }

    /// Socket passed only to the CLI being observed.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Release after stopping the owned server. No fixture response is supplied.
    pub fn release_frozen(&mut self) {
        self.frozen.store(true, Ordering::Release);
        self.permission
            .take()
            .expect("one readiness permission")
            .send(())
            .expect("readiness worker lives");
    }

    /// CLI exit makes the accept backlog finite; drain it and join all workers.
    pub fn seal(&mut self) {
        self.seal.write_all(&[1]).expect("seal proxy");
    }
}

impl Drop for ApiProxy {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.permission.take();
        let _ = self.cancel.shutdown(Shutdown::Both);
        if let Some(supervisor) = self.supervisor.take() {
            supervisor.join().expect("proxy supervisor join");
        }
    }
}

fn invalid(reason: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason.into())
}
fn send(notice: &mpsc::Sender<Event>, event: Event) -> io::Result<()> {
    notice.send(event).map_err(io::Error::other)
}

fn line(stream: &mut UnixStream, stop: &UnixStream, stages: &Stages) -> io::Result<String> {
    read_socket_line(
        stream,
        |fd| {
            if stages.readable(&[stop.as_raw_fd(), fd])? == 0 {
                Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "API proxy cancelled",
                ))
            } else {
                Ok(())
            }
        },
        "API proxy peer closed",
    )
}

/// Only EOF from the CLI satisfies abandonment. Backend bytes or EOF, extra
/// client bytes, cancellation and silence all fail without supplying a reply.
fn abandoned(
    client: &mut UnixStream,
    upstream: &UnixStream,
    stop: &UnixStream,
    stages: &Stages,
) -> io::Result<()> {
    match stages.readable(&[stop.as_raw_fd(), upstream.as_raw_fd(), client.as_raw_fd()])? {
        0 => Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "API proxy cancelled",
        )),
        1 => Err(invalid(
            "frozen backend answered or closed before client abandonment",
        )),
        _ => {
            let mut byte = [0];
            if client.read(&mut byte)? == 0 {
                Ok(())
            } else {
                Err(invalid("unexpected bytes after the CLI request"))
            }
        }
    }
}

#[allow(clippy::too_many_arguments)] // The worker borrows its owned connection and shared fixture controls.
fn forward(
    mut client: UnixStream,
    request: String,
    backend: &Path,
    stop: &UnixStream,
    notice: &mpsc::Sender<Event>,
    permitted: &Mutex<Option<mpsc::Receiver<()>>>,
    index: Option<(usize, Instant)>,
    start_released: &Mutex<Option<Instant>>,
    stages: &Stages,
) -> io::Result<()> {
    if matches!(index, Some((0, _))) {
        permitted
            .lock()
            .expect("permission lock")
            .take()
            .ok_or_else(|| invalid("duplicate readiness request"))?
            .recv_timeout(stages.remaining())
            .map_err(io::Error::other)?;
    }
    let mut upstream = UnixStream::connect(backend)?;
    writeln!(upstream, "{request}")?;
    if let Some((index, released_at)) = index {
        abandoned(&mut client, &upstream, stop, stages)?;
        return send(notice, Event::Abandoned(index, released_at.elapsed()));
    }
    let response = line(&mut upstream, stop, stages)?;
    let value: serde_json::Value = serde_json::from_str(&request)?;
    if value["method"] == "agent.start" {
        let value: serde_json::Value = serde_json::from_str(&response)?;
        if !value["result"]["agent"].is_object() {
            return Err(invalid(format!("agent.start failed: {response}")));
        }
        send(notice, Event::Phase(Phase::StartResponseCaptured))?;
        // Stamp before writing: the CLI must read this response before it can
        // start its readiness deadline. Thus this release precedes every timer
        // in the frozen chain, even if accepting a request or observing EOF is
        // delayed. None of the four frozen requests receives a response, so
        // there is no later causal response release for a per-request bound.
        *start_released.lock().expect("release lock") = Some(Instant::now());
    }
    writeln!(client, "{response}")?;
    Ok(())
}

fn receive(events: &mpsc::Receiver<Event>, stages: &Stages) -> io::Result<Event> {
    events
        .recv_timeout(stages.remaining())
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => io::Error::new(
                io::ErrorKind::TimedOut,
                "fixture producer stayed silent at failure ceiling",
            ),
            mpsc::RecvTimeoutError::Disconnected => {
                io::Error::new(io::ErrorKind::UnexpectedEof, "fixture producers closed")
            }
        })
}

/// Require the exact phase; errors and early CLI death cannot imply readiness.
pub fn expect_phase(
    events: &mpsc::Receiver<Event>,
    expected: Phase,
    stages: &Stages,
) -> io::Result<()> {
    match receive(events, stages)? {
        Event::Phase(actual) if actual == expected => Ok(()),
        event => Err(invalid(format!("expected {expected:?}, got {event:?}"))),
    }
}

/// Require every documented request, client EOF for each, CLI exit and a sealed
/// backlog. All four bounds are cumulative from agent.start's response release:
/// readiness uses its caller budget, then each diagnostic/rollback adds one
/// REQUEST_TIMEOUT. No frozen response exists to anchor a per-request bound.
/// There is no success upper bound. Call after ReadinessRequested and SIGSTOP.
/// EOF/exit notifications may race across workers, but cannot be lost.
pub fn prove_abandonment(
    events: &mpsc::Receiver<Event>,
    stages: &Stages,
    ready_budget: Duration,
    mut seal: impl FnMut(),
) -> io::Result<()> {
    let mut requested = 1;
    let mut abandoned = [false; FROZEN_METHODS.len()];
    let mut exited = false;
    loop {
        match receive(events, stages)? {
            Event::Requested(index) if index == requested && index < FROZEN_METHODS.len() => requested += 1,
            Event::Abandoned(index, elapsed) if index < requested && !abandoned[index] => {
                let minimum = ready_budget + REQUEST_TIMEOUT * index as u32;
                if elapsed < minimum {
                    return Err(invalid(format!("request {index} gave up too soon: {elapsed:?} since agent.start release, expected at least {minimum:?}")));
                }
                abandoned[index] = true;
            },
            Event::Exited if !exited => { exited = true; seal(); }
            Event::Sealed(count) if exited && count == FROZEN_METHODS.len() && requested == count && abandoned.iter().all(|&yes| yes) => return Ok(()),
            event => return Err(invalid(format!("incomplete or unexpected frozen proof: {event:?}; requests={requested}, abandoned={abandoned:?}, exited={exited}"))),
        }
    }
}

/// Own an unreaped CLI throughout observation, including failure unwinding.
/// waitid observes termination without releasing the PID to unrelated processes.
pub struct Cli {
    child: Option<std::process::Child>,
    observer: Option<JoinHandle<()>>,
}
impl Cli {
    /// Observe termination without reaping or releasing ownership of this PID.
    pub fn new(child: std::process::Child, notice: mpsc::Sender<Event>) -> Self {
        let pid = child.id();
        let observer = thread::spawn(move || loop {
            // SAFETY: initialized siginfo storage belongs to this call. WNOWAIT
            // retains the owned child's PID until its guard finishes or drops.
            let result = unsafe {
                let mut info: libc::siginfo_t = std::mem::zeroed();
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if result == 0 {
                let _ = notice.send(Event::Exited);
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                let _ = notice.send(Event::Failed(format!("CLI observer: {error}")));
                break;
            }
        });
        Self {
            child: Some(child),
            observer: Some(observer),
        }
    }
    /// Call after observing exit. Join before reaping the child or releasing PID ownership.
    pub fn finish(mut self) -> std::process::Output {
        self.observer
            .take()
            .expect("CLI observer")
            .join()
            .expect("CLI observer join");
        self.child
            .take()
            .expect("owned CLI")
            .wait_with_output()
            .expect("CLI output")
    }
}
impl Drop for Cli {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            if let Some(observer) = self.observer.take() {
                let _ = observer.join();
            }
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const LIMIT: Duration = Duration::from_secs(2);
    const READY_BUDGET: Duration = Duration::from_secs(3);

    fn complete() -> Vec<Event> {
        // Exit may race the final EOF observer, but sealing follows worker join.
        vec![
            Event::Requested(1),
            Event::Abandoned(0, READY_BUDGET),
            Event::Requested(2),
            Event::Requested(3),
            Event::Abandoned(2, READY_BUDGET + REQUEST_TIMEOUT * 2),
            Event::Exited,
            Event::Abandoned(1, READY_BUDGET + REQUEST_TIMEOUT),
            Event::Abandoned(3, READY_BUDGET + REQUEST_TIMEOUT * 3),
            Event::Sealed(4),
        ]
    }

    fn proof(events: Vec<Event>) -> io::Result<()> {
        let (tx, rx) = mpsc::channel();
        for event in events {
            tx.send(event).unwrap();
        }
        drop(tx);
        prove_abandonment(&rx, &Stages::new(LIMIT), READY_BUDGET, || {})
    }

    #[test]
    fn all_abandonments_exit_and_backlog_seal_are_required() {
        proof(complete()).unwrap();
        for omitted in 0..complete().len() {
            let mut events = complete();
            events.remove(omitted);
            assert!(proof(events).is_err(), "missing evidence at {omitted}");
        }
    }

    #[test]
    fn every_request_must_wait_out_its_cap() {
        for index in 0..FROZEN_METHODS.len() {
            let mut events = complete();
            for event in &mut events {
                if let Event::Abandoned(observed, elapsed) = event {
                    if *observed >= index {
                        // Remove exactly this wait from every subsequent prefix.
                        *elapsed -= if index == 0 {
                            READY_BUDGET
                        } else {
                            REQUEST_TIMEOUT
                        };
                    }
                }
            }
            assert!(
                proof(events).is_err(),
                "request {index} gave up immediately"
            );
        }
    }

    #[test]
    fn duplicate_extra_reordered_and_failed_requests_are_rejected() {
        for bad in [
            Event::Requested(4),
            Event::Requested(2),
            Event::Abandoned(3, READY_BUDGET + REQUEST_TIMEOUT * 3),
            Event::Failed("worker died".into()),
            Event::Sealed(0),
        ] {
            let mut events = complete();
            events.insert(0, bad);
            assert!(proof(events).is_err());
        }
        let mut events = complete();
        events.insert(2, Event::Abandoned(0, READY_BUDGET));
        assert!(proof(events).is_err());
    }

    #[test]
    fn live_silent_producer_fails_at_the_ceiling() {
        let (_tx, rx) = mpsc::channel();
        assert_eq!(
            prove_abandonment(
                &rx,
                &Stages::new(Duration::from_millis(10)),
                READY_BUDGET,
                || {}
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn phases_reject_dead_unexpected_and_failed_producers() {
        for event in [
            Event::Phase(Phase::ReadinessRequested),
            Event::Failed("fixture failed".into()),
            Event::Exited,
        ] {
            let (tx, rx) = mpsc::channel();
            tx.send(event).unwrap();
            assert_eq!(
                expect_phase(&rx, Phase::StartResponseCaptured, &Stages::new(LIMIT))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
        let (tx, rx) = mpsc::channel();
        drop(tx);
        assert_eq!(
            expect_phase(&rx, Phase::StartResponseCaptured, &Stages::new(LIMIT))
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn only_client_eof_proves_abandonment() {
        let (mut client, sender) = UnixStream::pair().unwrap();
        let (backend, _server) = UnixStream::pair().unwrap();
        let (_cancel, stop) = UnixStream::pair().unwrap();
        drop(sender);
        abandoned(&mut client, &backend, &stop, &Stages::new(LIMIT)).unwrap();
    }

    #[test]
    fn backend_answer_or_death_cannot_prove_client_abandonment() {
        for reply in [true, false] {
            let (mut client, _sender) = UnixStream::pair().unwrap();
            let (backend, mut server) = UnixStream::pair().unwrap();
            let (_cancel, stop) = UnixStream::pair().unwrap();
            if reply {
                server.write_all(b"reply\n").unwrap();
            } else {
                drop(server);
            }
            assert_eq!(
                abandoned(&mut client, &backend, &stop, &Stages::new(LIMIT))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn unexpected_client_bytes_and_live_silence_fail() {
        for bytes in [true, false] {
            let (mut client, mut sender) = UnixStream::pair().unwrap();
            let (backend, _server) = UnixStream::pair().unwrap();
            let (_cancel, stop) = UnixStream::pair().unwrap();
            if bytes {
                sender.write_all(b"extra").unwrap();
            }
            let kind = abandoned(
                &mut client,
                &backend,
                &stop,
                &Stages::new(Duration::from_millis(10)),
            )
            .unwrap_err()
            .kind();
            assert_eq!(
                kind,
                if bytes {
                    io::ErrorKind::InvalidData
                } else {
                    io::ErrorKind::TimedOut
                }
            );
        }
    }

    #[test]
    fn cli_death_wakes_a_phase_wait() {
        let (tx, rx) = mpsc::channel();
        let child = super::super::support::environment::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let cli = Cli::new(child, tx);
        let error = expect_phase(&rx, Phase::ReadinessRequested, &Stages::new(LIMIT)).unwrap_err();
        assert!(error.to_string().contains("Exited"), "{error}");
        assert!(cli.finish().status.success());
    }
}
