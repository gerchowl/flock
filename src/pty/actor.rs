use std::{
    collections::VecDeque,
    io::{Read, Write},
    os::fd::{AsRawFd, OwnedFd, RawFd},
    sync::{mpsc as std_mpsc, Arc, Mutex},
    time::{Duration, Instant},
};

use bytes::Bytes;
use tokio::sync::mpsc::{self, error::TryRecvError as DataTryRecvError};
use tracing::debug;

use crate::pty::fd;

#[cfg(not(test))]
const ACTOR_POLL_MS: i32 = 50;
#[cfg(test)]
const ACTOR_POLL_MS: i32 = 1000;
const ACTOR_COMMAND_BUFFER: usize = 1024;
const HANDOFF_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the handle waits past the actor's own drain deadline, so the actor's
/// named drain error (#981) reaches the caller before the handle's generic
/// "timed out waiting" one does.
const HANDOFF_REPLY_MARGIN: Duration = Duration::from_millis(250);
/// Consecutive `write` calls that may return 0 for a non-empty buffer before the
/// pane's writer is declared broken (#1011). A PTY master never legitimately
/// accepts zero bytes of a non-empty write, so this only bounds the retry.
const ZERO_WRITE_LIMIT: u32 = 8;
/// How long a broken writer rejects input before it is re-armed and the next
/// write probes it again. A writer that recovered takes input from then on,
/// one that did not breaks again after `ZERO_WRITE_LIMIT` attempts.
const WRITER_REARM_AFTER: Duration = Duration::from_secs(5);
/// At most one "writer broken" warning per pane per interval; repeats inside
/// it are counted and reported with the next warning (#997).
const WRITER_BROKEN_WARN_INTERVAL: Duration = Duration::from_secs(60);

/// Whether a pane's writer is currently refusing input, shared between the
/// handle (which rejects writes while it is) and the actor (which sets and
/// re-arms it).
#[derive(Debug, Default)]
struct WriterHealth {
    broken: std::sync::atomic::AtomicBool,
    rejected_writes: std::sync::atomic::AtomicU64,
}

impl WriterHealth {
    fn is_broken(&self) -> bool {
        self.broken.load(std::sync::atomic::Ordering::Acquire)
    }

    fn set_broken(&self, broken: bool) {
        self.broken
            .store(broken, std::sync::atomic::Ordering::Release);
    }

    fn note_rejected(&self) {
        self.rejected_writes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn take_rejected(&self) -> u64 {
        self.rejected_writes
            .swap(0, std::sync::atomic::Ordering::Relaxed)
    }
}

/// The pending writes a pane still held when its handoff drain deadline passed
/// (#981, #1011). Carried as the payload of the `TimedOut` error so the server
/// can name the pane and its byte count instead of an anonymous timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HandoffDrainTimeout {
    pub pending_bytes: usize,
    /// The drain stopped because the writer kept accepting zero bytes, not
    /// because the deadline passed.
    pub writer_stuck: bool,
}

impl std::fmt::Display for HandoffDrainTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.writer_stuck {
            write!(
                f,
                "PTY writer accepted zero bytes while draining before handoff ({} bytes pending)",
                self.pending_bytes
            )
        } else {
            write!(
                f,
                "timed out draining PTY writes before handoff ({} bytes pending)",
                self.pending_bytes
            )
        }
    }
}

impl std::error::Error for HandoffDrainTimeout {}

impl HandoffDrainTimeout {
    /// The drain timeout carried by `err`, if that is what it is.
    pub(crate) fn from_io_error(err: &std::io::Error) -> Option<Self> {
        err.get_ref()?.downcast_ref::<Self>().copied()
    }
}

/// What a successful handoff quiesce left behind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct HandoffQuiesce {
    /// Pending bytes dropped because the caller opted into `--skip-undrained`.
    pub dropped_bytes: usize,
    /// The dropped bytes were stuck behind a writer accepting zero bytes.
    pub writer_stuck: bool,
}

/// A quiesce request sent to the actor and not yet answered, so a handoff can
/// start every pane's drain before waiting on any of them.
pub(crate) struct PendingHandoff {
    handle: PtyIoActorHandle,
    reply_rx: std_mpsc::Receiver<std::io::Result<HandoffQuiesce>>,
    deadline: Instant,
}

impl PendingHandoff {
    /// Waits for the actor's answer. A failure rolls the actor back, so the
    /// pane keeps running and keeps its queued input.
    pub(crate) fn wait(self) -> std::io::Result<HandoffQuiesce> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        match self.reply_rx.recv_timeout(left) {
            Ok(Ok(quiesce)) => Ok(quiesce),
            Ok(Err(err)) => {
                let _ = self.handle.rollback_handoff();
                Err(err)
            }
            Err(_) => {
                let _ = self.handle.rollback_handoff();
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out waiting for PTY actor to quiesce",
                ))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActorState {
    Running,
    Quiesced,
    Released,
}

pub(crate) struct PtyReadResult {
    pub terminal_responses: Vec<Bytes>,
}

impl PtyReadResult {
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            terminal_responses: Vec::new(),
        }
    }
}

type ReadCallback = Box<dyn FnMut(&[u8]) -> PtyReadResult + Send + 'static>;
type ReaderExitCallback = Box<dyn FnOnce() + Send + 'static>;
#[cfg(test)]
type WriteHook = Box<dyn FnMut(&[u8]) -> std::io::Result<usize> + Send>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PtyResize {
    rows: u16,
    cols: u16,
    cell_width_px: u32,
    cell_height_px: u32,
}

#[derive(Default)]
struct SharedPtyControls {
    resize: Option<PtyResize>,
    nudge: Option<PtyResize>,
}

pub(crate) struct PtyIoActorConfig {
    pub pane_id: u32,
    pub master_fd: OwnedFd,
    pub initially_quiesced: bool,
    pub on_read: ReadCallback,
    pub on_reader_exit: Option<ReaderExitCallback>,
}

enum PtyIoDataCommand {
    WriteUserInput(Bytes),
    WriteTrackedInput(Bytes, std_mpsc::Sender<Instant>),
}

struct BeginHandoffRequest {
    drain_timeout: Duration,
    drop_undrained: bool,
    reply: std_mpsc::Sender<std::io::Result<HandoffQuiesce>>,
}

enum PtyIoControlCommand {
    BeginHandoff(BeginHandoffRequest),
    DuplicateForHandoff(std_mpsc::Sender<std::io::Result<RawFd>>),
    ForegroundProcessGroup(std_mpsc::Sender<Option<u32>>),
    RollbackHandoff(std_mpsc::Sender<std::io::Result<()>>),
    ReleaseAfterCommit(std_mpsc::Sender<std::io::Result<()>>),
    Shutdown,
}

#[derive(Clone)]
pub(crate) struct PtyIoActorHandle {
    data_tx: mpsc::Sender<PtyIoDataCommand>,
    control_tx: std_mpsc::Sender<PtyIoControlCommand>,
    wake: fd::WakeWriter,
    user_writes: Arc<Mutex<UserWriteGate>>,
    controls: Arc<Mutex<SharedPtyControls>>,
    writer_health: Arc<WriterHealth>,
}

#[derive(Debug)]
struct UserWriteGate {
    accepting: bool,
}

impl PtyIoActorHandle {
    pub(crate) async fn write_user_input(
        &self,
        bytes: Bytes,
    ) -> Result<(), mpsc::error::SendError<Bytes>> {
        // #1011: an empty payload can never be written, and queued it wedged
        // the flush loop and every later handoff drain behind it.
        if bytes.is_empty() {
            return Ok(());
        }
        if self.writer_health.is_broken() {
            self.writer_health.note_rejected();
            return Err(mpsc::error::SendError(bytes));
        }
        {
            let user_writes = self
                .user_writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !user_writes.accepting {
                return Err(mpsc::error::SendError(bytes));
            }
        }

        let permit = match self.data_tx.reserve().await {
            Ok(permit) => permit,
            Err(_) => return Err(mpsc::error::SendError(bytes)),
        };

        let user_writes = self
            .user_writes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !user_writes.accepting {
            return Err(mpsc::error::SendError(bytes));
        }
        permit.send(PtyIoDataCommand::WriteUserInput(bytes));
        self.wake_actor();
        Ok(())
    }

    pub(crate) fn try_write_user_input(
        &self,
        bytes: Bytes,
    ) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.try_write_command(PtyIoDataCommand::WriteUserInput(bytes))
    }

    pub(crate) fn try_write_tracked_input(
        &self,
        bytes: Bytes,
    ) -> Result<crate::pty::write_receipt::WriteReceipt, mpsc::error::TrySendError<Bytes>> {
        let (completion, receipt) = crate::pty::write_receipt::WriteReceipt::channel();
        self.try_write_command(PtyIoDataCommand::WriteTrackedInput(bytes, completion))?;
        Ok(receipt)
    }

    fn try_write_command(
        &self,
        command: PtyIoDataCommand,
    ) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        let rejected = |command| match command {
            PtyIoDataCommand::WriteUserInput(bytes)
            | PtyIoDataCommand::WriteTrackedInput(bytes, _) => bytes,
        };
        if self.writer_health.is_broken() {
            self.writer_health.note_rejected();
            return Err(mpsc::error::TrySendError::Closed(rejected(command)));
        }
        let user_writes = self.user_writes.lock().unwrap_or_else(|p| p.into_inner());
        if !user_writes.accepting {
            return Err(mpsc::error::TrySendError::Closed(rejected(command)));
        }
        match self.data_tx.try_send(command) {
            Ok(()) => {
                self.wake_actor();
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(command)) => {
                Err(mpsc::error::TrySendError::Full(rejected(command)))
            }
            Err(mpsc::error::TrySendError::Closed(command)) => {
                Err(mpsc::error::TrySendError::Closed(rejected(command)))
            }
        }
    }

    pub(crate) fn resize(&self, rows: u16, cols: u16, cell_width_px: u32, cell_height_px: u32) {
        {
            let mut controls = self
                .controls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            controls.resize = Some(PtyResize {
                rows,
                cols,
                cell_width_px,
                cell_height_px,
            });
        }
        self.wake_actor();
    }

    pub(crate) fn nudge_child_redraw_after_handoff(
        &self,
        rows: u16,
        cols: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) {
        {
            let mut controls = self
                .controls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            controls.nudge = Some(PtyResize {
                rows,
                cols,
                cell_width_px,
                cell_height_px,
            });
        }
        self.wake_actor();
    }

    pub(crate) fn begin_handoff(&self, timeout: Duration) -> std::io::Result<()> {
        self.begin_handoff_with(timeout, false).map(|_| ())
    }

    /// Quiesces the actor for handoff. With `drop_undrained`, writes still
    /// pending at the drain deadline are dropped and counted instead of failing
    /// the handoff (`flk server live-handoff --skip-undrained`).
    pub(crate) fn begin_handoff_with(
        &self,
        timeout: Duration,
        drop_undrained: bool,
    ) -> std::io::Result<HandoffQuiesce> {
        self.start_handoff(timeout, drop_undrained)?.wait()
    }

    /// Sends the quiesce request without waiting for it, so many panes can
    /// drain at once. [`PendingHandoff::wait`] collects the answer.
    pub(crate) fn start_handoff(
        &self,
        timeout: Duration,
        drop_undrained: bool,
    ) -> std::io::Result<PendingHandoff> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        let drain_timeout = timeout
            .saturating_sub(HANDOFF_REPLY_MARGIN)
            .min(HANDOFF_DRAIN_TIMEOUT);
        let deadline = Instant::now() + timeout;
        {
            let mut user_writes = self
                .user_writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            user_writes.accepting = false;
            if self
                .control_tx
                .send(PtyIoControlCommand::BeginHandoff(BeginHandoffRequest {
                    drain_timeout,
                    drop_undrained,
                    reply: reply_tx,
                }))
                .is_err()
            {
                user_writes.accepting = true;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "pty actor closed",
                ));
            }
            self.wake_actor();
        }
        Ok(PendingHandoff {
            handle: self.clone(),
            reply_rx,
            deadline,
        })
    }

    /// Whether this pane's writer is refusing input after accepting zero bytes.
    pub(crate) fn writer_broken(&self) -> bool {
        self.writer_health.is_broken()
    }

    pub(crate) fn duplicate_for_handoff(&self) -> std::io::Result<RawFd> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.control_tx
            .send(PtyIoControlCommand::DuplicateForHandoff(reply_tx))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty actor closed"))?;
        self.wake_actor();
        reply_rx.recv_timeout(Duration::from_secs(1)).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for PTY handoff duplicate",
            )
        })?
    }

    pub(crate) fn foreground_process_group_id(&self) -> Option<u32> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.control_tx
            .send(PtyIoControlCommand::ForegroundProcessGroup(reply_tx))
            .ok()?;
        self.wake_actor();
        reply_rx.recv_timeout(Duration::from_secs(1)).ok()?
    }

    pub(crate) fn rollback_handoff(&self) -> std::io::Result<()> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.control_tx
            .send(PtyIoControlCommand::RollbackHandoff(reply_tx))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty actor closed"))?;
        self.wake_actor();
        let result = reply_rx.recv_timeout(Duration::from_secs(1)).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for PTY handoff rollback",
            )
        })?;
        if result.is_ok() {
            let mut user_writes = self
                .user_writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            user_writes.accepting = true;
        }
        result
    }

    pub(crate) fn release_after_commit(&self) -> std::io::Result<()> {
        {
            let mut user_writes = self
                .user_writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            user_writes.accepting = false;
        }
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.control_tx
            .send(PtyIoControlCommand::ReleaseAfterCommit(reply_tx))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty actor closed"))?;
        self.wake_actor();
        reply_rx.recv_timeout(Duration::from_secs(1)).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for PTY actor release",
            )
        })?
    }

    pub(crate) fn shutdown(&self) {
        {
            let mut user_writes = self
                .user_writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            user_writes.accepting = false;
        }
        if self.control_tx.send(PtyIoControlCommand::Shutdown).is_ok() {
            self.wake_actor();
        }
    }

    fn wake_actor(&self) {
        if let Err(err) = self.wake.wake() {
            crate::logging::pty_wake_failed(&err.to_string());
        }
    }
}

pub(crate) struct PtyIoActor;

impl PtyIoActor {
    pub(crate) fn spawn(config: PtyIoActorConfig) -> std::io::Result<PtyIoActorHandle> {
        Self::spawn_inner(config, None)
    }

    fn spawn_inner(
        config: PtyIoActorConfig,
        poll_observer: Option<std_mpsc::Sender<()>>,
    ) -> std::io::Result<PtyIoActorHandle> {
        fd::set_cloexec(config.master_fd.as_raw_fd())?;
        fd::set_nonblocking(config.master_fd.as_raw_fd())?;

        let (data_tx, data_rx) = mpsc::channel(ACTOR_COMMAND_BUFFER);
        let (control_tx, control_rx) = std_mpsc::channel();
        let wake_pipe = fd::create_wake_pipe()?;
        let user_writes = Arc::new(Mutex::new(UserWriteGate {
            accepting: !config.initially_quiesced,
        }));
        let controls = Arc::new(Mutex::new(SharedPtyControls::default()));
        let writer_health = Arc::new(WriterHealth::default());
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake: wake_pipe.writer,
            user_writes,
            controls: Arc::clone(&controls),
            writer_health: Arc::clone(&writer_health),
        };

        let mut runner = PtyIoActorRunner {
            pane_id: config.pane_id,
            file: std::fs::File::from(config.master_fd),
            data_rx,
            control_rx,
            state: if config.initially_quiesced {
                ActorState::Quiesced
            } else {
                ActorState::Running
            },
            pending_writes: VecDeque::new(),
            current_write_offset: 0,
            zero_write_streak: 0,
            writer_health,
            broken_at: None,
            last_broken_warn: None,
            suppressed_breaks: 0,
            draining_for_handoff: false,
            #[cfg(test)]
            write_hook: None,
            wake_read_fd: wake_pipe.read_fd,
            controls,
            on_read: config.on_read,
            on_reader_exit: config.on_reader_exit,
            poll_observer,
        };
        std::thread::Builder::new()
            .name(format!("flock-pty-{}", config.pane_id))
            .spawn(move || runner.run())
            .map_err(|err| std::io::Error::other(err.to_string()))?;

        Ok(handle)
    }

    #[cfg(test)]
    fn spawn_with_poll_observer(
        config: PtyIoActorConfig,
        poll_observer: std_mpsc::Sender<()>,
    ) -> std::io::Result<PtyIoActorHandle> {
        Self::spawn_inner(config, Some(poll_observer))
    }
}

struct PtyIoActorRunner {
    pane_id: u32,
    file: std::fs::File,
    data_rx: mpsc::Receiver<PtyIoDataCommand>,
    control_rx: std_mpsc::Receiver<PtyIoControlCommand>,
    state: ActorState,
    pending_writes: VecDeque<(Bytes, Option<std_mpsc::Sender<Instant>>)>,
    current_write_offset: usize,
    /// Consecutive zero-byte writes of a non-empty buffer (#1011).
    zero_write_streak: u32,
    /// Set once `ZERO_WRITE_LIMIT` is hit. The handle then rejects input with
    /// an error until the writer is re-armed (after `WRITER_REARM_AFTER`, or
    /// on handoff rollback).
    writer_health: Arc<WriterHealth>,
    broken_at: Option<Instant>,
    last_broken_warn: Option<Instant>,
    suppressed_breaks: u32,
    /// While the handoff drain runs, a stuck writer fails the drain with its
    /// queue intact instead of being declared broken and emptied.
    draining_for_handoff: bool,
    /// Stands in for `write(2)` so a test can make it return 0.
    #[cfg(test)]
    write_hook: Option<WriteHook>,
    wake_read_fd: OwnedFd,
    controls: Arc<Mutex<SharedPtyControls>>,
    on_read: ReadCallback,
    on_reader_exit: Option<ReaderExitCallback>,
    poll_observer: Option<std_mpsc::Sender<()>>,
}

impl PtyIoActorRunner {
    fn run(&mut self) {
        let mut should_exit = false;
        while !should_exit {
            should_exit = self.drain_commands();
            if should_exit || self.state == ActorState::Released {
                break;
            }

            self.apply_pending_controls();
            self.maybe_rearm_writer();

            if !self.pending_writes.is_empty() {
                self.flush_pending_writes_once();
            }

            if let Some(poll_observer) = &self.poll_observer {
                let _ = poll_observer.send(());
            }

            match fd::poll_pty_and_wake(
                self.file.as_raw_fd(),
                self.wake_read_fd.as_raw_fd(),
                self.state == ActorState::Running,
                !self.pending_writes.is_empty(),
                ACTOR_POLL_MS,
            ) {
                Ok(readiness) => {
                    if readiness.wake_ready {
                        if let Err(err) = fd::drain_wake_fd(self.wake_read_fd.as_raw_fd()) {
                            crate::logging::pty_wake_drain_failed(self.pane_id, &err.to_string());
                            break;
                        }
                        continue;
                    }
                    if readiness.pty_write_ready && !self.pending_writes.is_empty() {
                        self.flush_pending_writes_once();
                    }
                    if self.state == ActorState::Running
                        && readiness.pty_read_ready
                        && !self.read_once()
                    {
                        break;
                    }
                }
                Err(err) => {
                    crate::logging::pty_poll_failed(self.pane_id, &err.to_string());
                    break;
                }
            }
        }

        let rejected = self.writer_health.take_rejected();
        if rejected > 0 {
            crate::logging::pty_writes_rejected_while_broken(self.pane_id, rejected);
        }
        if let Some(on_reader_exit) = self.on_reader_exit.take() {
            on_reader_exit();
        }
        debug!(pane = self.pane_id, "PTY actor exiting");
    }

    fn drain_commands(&mut self) -> bool {
        if self.drain_control_commands() {
            return true;
        }
        self.drain_data_commands()
    }

    fn drain_control_commands(&mut self) -> bool {
        let mut should_exit = false;
        loop {
            match self.control_rx.try_recv() {
                Ok(command) => {
                    if self.handle_control_command(command) {
                        should_exit = true;
                        break;
                    }
                }
                Err(std_mpsc::TryRecvError::Empty) => break,
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    should_exit = true;
                    break;
                }
            }
        }
        should_exit
    }

    fn drain_data_commands(&mut self) -> bool {
        let mut should_exit = false;
        loop {
            match self.data_rx.try_recv() {
                Ok(command) => {
                    if self.handle_data_command(command) {
                        should_exit = true;
                        break;
                    }
                }
                Err(DataTryRecvError::Empty) => break,
                Err(DataTryRecvError::Disconnected) => {
                    should_exit = true;
                    break;
                }
            }
        }
        should_exit
    }

    fn handle_data_command(&mut self, command: PtyIoDataCommand) -> bool {
        if self.state == ActorState::Running {
            self.enqueue_write(command);
        }
        false
    }

    fn handle_control_command(&mut self, command: PtyIoControlCommand) -> bool {
        match command {
            PtyIoControlCommand::BeginHandoff(request) => {
                let result = self.begin_handoff(request.drain_timeout, request.drop_undrained);
                let _ = request.reply.send(result);
            }
            PtyIoControlCommand::DuplicateForHandoff(reply) => {
                let result = if self.state == ActorState::Quiesced {
                    fd::duplicate_cloexec_fd(self.file.as_raw_fd())
                } else {
                    Err(std::io::Error::other(
                        "PTY actor must be quiesced before handoff duplication",
                    ))
                };
                let _ = reply.send(result);
            }
            PtyIoControlCommand::ForegroundProcessGroup(reply) => {
                let result =
                    crate::platform::foreground_process_group_id_for_tty_fd(self.file.as_raw_fd());
                let _ = reply.send(result);
            }
            PtyIoControlCommand::RollbackHandoff(reply) => {
                let result = if self.state == ActorState::Released {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "PTY actor was released before handoff rollback",
                    ))
                } else {
                    self.state = ActorState::Running;
                    self.rearm_writer();
                    Ok(())
                };
                let _ = reply.send(result);
            }
            PtyIoControlCommand::ReleaseAfterCommit(reply) => {
                self.state = ActorState::Released;
                self.pending_writes.clear();
                let _ = reply.send(Ok(()));
                return true;
            }
            PtyIoControlCommand::Shutdown => return true,
        }
        false
    }

    fn begin_handoff(
        &mut self,
        drain_timeout: Duration,
        drop_undrained: bool,
    ) -> std::io::Result<HandoffQuiesce> {
        self.draining_for_handoff = true;
        let result = self.drain_for_handoff(drain_timeout, drop_undrained);
        self.draining_for_handoff = false;
        result
    }

    fn drain_for_handoff(
        &mut self,
        drain_timeout: Duration,
        drop_undrained: bool,
    ) -> std::io::Result<HandoffQuiesce> {
        self.drain_pre_quiesce_commands();
        self.apply_pending_controls();
        if self.state == ActorState::Released {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "PTY actor was released before handoff quiesce",
            ));
        }
        let deadline = Instant::now() + drain_timeout;
        let mut quiesce = HandoffQuiesce::default();
        while !self.pending_writes.is_empty() {
            let writer_stuck = self.zero_write_streak >= ZERO_WRITE_LIMIT;
            if writer_stuck || Instant::now() >= deadline {
                let pending_bytes = self.pending_bytes();
                if !drop_undrained {
                    // #1011 review: the queue stays as it is, so the old server
                    // keeps every byte when the handoff is refused. The streak
                    // restarts too, or the next zero write after the rollback
                    // would declare the writer broken and drop that queue.
                    self.zero_write_streak = 0;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        HandoffDrainTimeout {
                            pending_bytes,
                            writer_stuck,
                        },
                    ));
                }
                crate::logging::pty_handoff_dropped_undrained(self.pane_id, pending_bytes);
                self.pending_writes.clear();
                self.current_write_offset = 0;
                self.zero_write_streak = 0;
                quiesce.dropped_bytes = pending_bytes;
                quiesce.writer_stuck = writer_stuck;
                break;
            }
            self.flush_pending_writes_once();
        }
        self.state = ActorState::Quiesced;
        Ok(quiesce)
    }

    fn drain_pre_quiesce_commands(&mut self) {
        while let Ok(command) = self.data_rx.try_recv() {
            if self.state != ActorState::Released {
                self.enqueue_write(command);
            }
        }
    }

    fn enqueue_write(&mut self, command: PtyIoDataCommand) {
        match command {
            PtyIoDataCommand::WriteUserInput(bytes) => self.push_pending(bytes),
            PtyIoDataCommand::WriteTrackedInput(bytes, completion) => {
                self.pending_writes.push_back((bytes, Some(completion)));
            }
        }
    }

    fn complete_front_write(&mut self, at: Instant) {
        if let Some((_, Some(completion))) = self.pending_writes.pop_front() {
            let _ = completion.send(at);
        }
        self.current_write_offset = 0;
    }

    /// Untracked input and terminal replies drop empty payloads here (#1011).
    fn push_pending(&mut self, bytes: Bytes) {
        if bytes.is_empty() {
            return;
        }
        self.pending_writes.push_back((bytes, None));
    }

    fn pending_bytes(&self) -> usize {
        self.pending_writes
            .iter()
            .map(|(bytes, _)| bytes.len())
            .sum::<usize>()
            .saturating_sub(self.current_write_offset)
    }

    fn apply_pending_controls(&mut self) {
        let (resize, nudge) = {
            let mut controls = self
                .controls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (controls.resize.take(), controls.nudge.take())
        };
        if self.state == ActorState::Released {
            return;
        }
        if let Some(resize) = resize {
            self.resize(resize);
        }
        if let Some(nudge) = nudge {
            self.nudge(nudge);
        }
    }

    fn read_once(&mut self) -> bool {
        let mut buf = [0u8; 8192];
        match self.file.read(&mut buf) {
            Ok(0) => false,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => true,
            Err(err) => {
                crate::logging::pty_read_failed(self.pane_id, &err.to_string());
                false
            }
            Ok(n) => {
                let result = (self.on_read)(&buf[..n]);
                for response in result.terminal_responses {
                    if self.state != ActorState::Released {
                        self.push_pending(response);
                    }
                }
                true
            }
        }
    }

    fn flush_pending_writes_once(&mut self) {
        while let Some(bytes) = self.pending_writes.front().map(|(bytes, _)| bytes.clone()) {
            if self.current_write_offset >= bytes.len() {
                // An empty entry (or a fully written one) has nothing left to
                // write. `write(&[])` returns 0, which is how one empty payload
                // used to pin the queue forever (#1011).
                self.complete_front_write(Instant::now());
                continue;
            }
            match self.write_chunk(&bytes[self.current_write_offset..]) {
                Ok(0) => {
                    self.note_zero_write();
                    return;
                }
                Ok(written) => {
                    self.zero_write_streak = 0;
                    self.current_write_offset += written;
                    if self.current_write_offset >= bytes.len() {
                        self.complete_front_write(Instant::now());
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    let _ = fd::poll_write_ready(self.file.as_raw_fd(), ACTOR_POLL_MS);
                    return;
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => return,
                Err(err) => {
                    crate::logging::pty_write_failed(self.pane_id, &err.to_string());
                    self.pending_writes.clear();
                    self.current_write_offset = 0;
                    return;
                }
            }
        }
        let _ = self.file.flush();
    }

    fn write_chunk(&mut self, chunk: &[u8]) -> std::io::Result<usize> {
        #[cfg(test)]
        if let Some(hook) = self.write_hook.as_mut() {
            return hook(chunk);
        }
        self.file.write(chunk)
    }

    /// A `write` of a non-empty buffer returned 0. Retried a bounded number of
    /// times, then the writer is given up on. During a handoff drain the drain
    /// itself reads the streak and refuses with the queue intact.
    fn note_zero_write(&mut self) {
        self.zero_write_streak = self.zero_write_streak.saturating_add(1);
        if self.zero_write_streak >= ZERO_WRITE_LIMIT && !self.draining_for_handoff {
            self.mark_writer_broken();
        }
    }

    /// Gives up on a writer that keeps accepting zero bytes of a non-empty
    /// buffer: the queue is dropped and the handle rejects input with an
    /// error until the writer is re-armed. The warning is rate-limited
    /// (#1011, #997).
    fn mark_writer_broken(&mut self) {
        let pending_bytes = self.pending_bytes();
        let now = Instant::now();
        let warn_due = self
            .last_broken_warn
            .is_none_or(|last| now.duration_since(last) >= WRITER_BROKEN_WARN_INTERVAL);
        if warn_due {
            crate::logging::pty_writer_broken(
                self.pane_id,
                self.zero_write_streak,
                pending_bytes,
                self.suppressed_breaks,
            );
            self.last_broken_warn = Some(now);
            self.suppressed_breaks = 0;
        } else {
            self.suppressed_breaks = self.suppressed_breaks.saturating_add(1);
        }
        self.writer_health.set_broken(true);
        self.broken_at = Some(now);
        self.zero_write_streak = 0;
        self.pending_writes.clear();
        self.current_write_offset = 0;
    }

    fn maybe_rearm_writer(&mut self) {
        if self
            .broken_at
            .is_some_and(|at| at.elapsed() >= WRITER_REARM_AFTER)
        {
            self.rearm_writer();
        }
    }

    /// Lets input through again; the next write is the probe.
    fn rearm_writer(&mut self) {
        if self.broken_at.take().is_none() {
            return;
        }
        self.writer_health.set_broken(false);
        let rejected = self.writer_health.take_rejected();
        crate::logging::pty_writer_rearmed(self.pane_id, rejected);
    }

    fn resize(&self, resize: PtyResize) {
        self.log_resize_result(fd::resize_pty_fd(
            self.file.as_raw_fd(),
            resize.rows,
            resize.cols,
            resize.cell_width_px,
            resize.cell_height_px,
        ));
    }

    fn nudge(&mut self, resize: PtyResize) {
        if self.state == ActorState::Released {
            return;
        }
        let nudge = if resize.rows > 2 {
            (
                resize.rows - 1,
                resize.cols,
                resize.cell_width_px,
                resize.cell_height_px,
            )
        } else {
            (
                resize.rows,
                resize.cols.saturating_sub(1).max(4),
                resize.cell_width_px,
                resize.cell_height_px,
            )
        };
        if nudge
            == (
                resize.rows,
                resize.cols,
                resize.cell_width_px,
                resize.cell_height_px,
            )
        {
            return;
        }
        self.log_resize_result(fd::resize_pty_fd(
            self.file.as_raw_fd(),
            nudge.0,
            nudge.1,
            nudge.2,
            nudge.3,
        ));
        std::thread::sleep(Duration::from_millis(30));
        self.log_resize_result(fd::resize_pty_fd(
            self.file.as_raw_fd(),
            resize.rows,
            resize.cols,
            resize.cell_width_px,
            resize.cell_height_px,
        ));
    }

    fn log_resize_result(&self, result: std::io::Result<()>) {
        if let Err(err) = result {
            crate::logging::pty_resize_failed(self.pane_id, &err.to_string());
        }
    }
}

#[cfg(test)]
impl PtyIoActorHandle {
    /// A real actor on one end of a socket pair, for tests outside this module.
    pub(crate) fn test_spawn_on_socket_pair() -> (Self, std::os::unix::net::UnixStream) {
        use std::os::fd::{FromRawFd, IntoRawFd};
        let (actor_socket, peer) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        let master_fd = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let handle = PtyIoActor::spawn(PtyIoActorConfig {
            pane_id: 1,
            master_fd,
            initially_quiesced: false,
            on_read: Box::new(|_| PtyReadResult::empty()),
            on_reader_exit: None,
        })
        .expect("actor spawn");
        (handle, peer)
    }

    /// A handle with no actor behind it: nothing drains its `capacity`-slot
    /// queue, so a test can fill it. Keep the guard alive for the handle's
    /// lifetime.
    pub(crate) fn test_unserviced(capacity: usize) -> (Self, Box<dyn std::any::Any>) {
        let (data_tx, data_rx) = mpsc::channel(capacity);
        let (control_tx, control_rx) = std_mpsc::channel();
        let wake_pipe = fd::create_wake_pipe().expect("wake pipe");
        let handle = Self {
            data_tx,
            control_tx,
            wake: wake_pipe.writer,
            user_writes: Arc::new(Mutex::new(UserWriteGate { accepting: true })),
            controls: Arc::new(Mutex::new(SharedPtyControls::default())),
            writer_health: Arc::default(),
        };
        (handle, Box::new((data_rx, control_rx, wake_pipe.read_fd)))
    }

    /// Puts the writer in the state `ZERO_WRITE_LIMIT` zero-byte writes leave.
    pub(crate) fn test_mark_writer_broken(&self) {
        self.writer_health.set_broken(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::fd::{AsRawFd, FromRawFd, IntoRawFd},
        os::unix::net::UnixStream,
    };

    fn test_wake_pair() -> (fd::WakeWriter, OwnedFd) {
        let pipe = fd::create_wake_pipe().expect("wake pipe");
        (pipe.writer, pipe.read_fd)
    }

    fn actor_with_socket_pair(
        initially_quiesced: bool,
    ) -> (PtyIoActorHandle, UnixStream, std_mpsc::Receiver<Bytes>) {
        actor_with_socket_pair_and_poll_observer(initially_quiesced, None)
    }

    fn actor_with_socket_pair_and_poll_observer(
        initially_quiesced: bool,
        poll_observer: Option<std_mpsc::Sender<()>>,
    ) -> (PtyIoActorHandle, UnixStream, std_mpsc::Receiver<Bytes>) {
        let (actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let (read_tx, read_rx) = std_mpsc::channel();
        let config = PtyIoActorConfig {
            pane_id: 1,
            master_fd: owned,
            initially_quiesced,
            on_read: Box::new(move |bytes| {
                read_tx
                    .send(Bytes::copy_from_slice(bytes))
                    .expect("read callback receiver alive");
                PtyReadResult::empty()
            }),
            on_reader_exit: None,
        };
        let handle = if let Some(poll_observer) = poll_observer {
            PtyIoActor::spawn_with_poll_observer(config, poll_observer)
        } else {
            PtyIoActor::spawn(config)
        }
        .expect("actor spawn");
        (handle, peer, read_rx)
    }

    #[test]
    fn actor_writes_user_input_to_owned_fd() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);

        handle
            .try_write_user_input(Bytes::from_static(b"hello"))
            .expect("write command accepted");

        let mut buf = [0u8; 5];
        peer.read_exact(&mut buf).expect("peer receives write");
        assert_eq!(&buf, b"hello");
        handle.shutdown();
    }

    #[test]
    fn actor_wakes_idle_poll_for_user_input() {
        let (poll_tx, poll_rx) = std_mpsc::channel();
        let (handle, mut peer, _read_rx) =
            actor_with_socket_pair_and_poll_observer(false, Some(poll_tx));
        peer.set_read_timeout(Some(Duration::from_millis(500)))
            .expect("peer timeout");
        poll_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor entered idle poll");

        let start = Instant::now();
        handle
            .try_write_user_input(Bytes::from_static(b"x"))
            .expect("write command accepted");

        let mut buf = [0u8; 1];
        peer.read_exact(&mut buf)
            .expect("peer receives write without waiting for actor poll timeout");
        assert_eq!(&buf, b"x");
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "actor write should be driven by wake fd, not the idle poll timeout"
        );
        handle.shutdown();
    }

    #[test]
    fn poll_ignores_pty_hup_without_pty_interest() {
        let (actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        drop(peer);
        let wake_pipe = fd::create_wake_pipe().expect("wake pipe");

        let readiness = fd::poll_pty_and_wake(
            actor_socket.as_raw_fd(),
            wake_pipe.read_fd.as_raw_fd(),
            false,
            false,
            10,
        )
        .expect("poll succeeds");

        assert!(!readiness.pty_read_ready);
        assert!(!readiness.pty_write_ready);
        assert!(!readiness.wake_ready);
    }

    #[test]
    fn actor_delivers_fd_reads_to_callback() {
        let (handle, mut peer, read_rx) = actor_with_socket_pair(false);

        peer.write_all(b"from-peer").expect("peer write");

        let read = read_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor read callback");
        assert_eq!(read, Bytes::from_static(b"from-peer"));
        handle.shutdown();
    }

    #[test]
    fn begin_handoff_stops_reads_and_rejects_user_writes_until_rollback() {
        let (handle, mut peer, read_rx) = actor_with_socket_pair(false);

        handle
            .begin_handoff(Duration::from_secs(1))
            .expect("handoff quiesced");
        assert!(handle
            .try_write_user_input(Bytes::from_static(b"blocked"))
            .is_err());

        peer.write_all(b"held").expect("peer write during quiesce");
        assert!(
            read_rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "actor must not read while quiesced"
        );

        handle.rollback_handoff().expect("rollback resumes actor");
        let read = read_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reads held bytes after rollback");
        assert_eq!(read, Bytes::from_static(b"held"));

        handle
            .try_write_user_input(Bytes::from_static(b"after"))
            .expect("write accepted after rollback");
        let mut buf = [0u8; 5];
        peer.read_exact(&mut buf).expect("peer receives after");
        assert_eq!(&buf, b"after");
        handle.shutdown();
    }

    #[test]
    fn duplicate_for_handoff_requires_quiesced_actor() {
        let (handle, mut peer, read_rx) = actor_with_socket_pair(false);

        assert!(handle.duplicate_for_handoff().is_err());
        handle
            .begin_handoff(Duration::from_secs(1))
            .expect("handoff quiesced");
        let duplicate = handle
            .duplicate_for_handoff()
            .expect("handoff duplicate created");
        assert!(duplicate >= 0);
        unsafe {
            libc::close(duplicate);
        }
        handle.rollback_handoff().expect("rollback resumes actor");

        peer.write_all(b"still-live").expect("peer write");
        let read = read_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor still reads after duplicate closes");
        assert_eq!(read, Bytes::from_static(b"still-live"));
        handle.shutdown();
    }

    #[test]
    fn resize_and_nudge_keep_latest_request_when_command_queue_is_full() {
        let (data_tx, _data_rx) = mpsc::channel(1);
        let (control_tx, _control_rx) = std_mpsc::channel();
        data_tx
            .try_send(PtyIoDataCommand::WriteUserInput(Bytes::from_static(
                b"fill",
            )))
            .expect("fill command queue");
        let controls = Arc::new(Mutex::new(SharedPtyControls::default()));
        let (wake, _wake_read_fd) = test_wake_pair();
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake,
            user_writes: Arc::new(Mutex::new(UserWriteGate { accepting: true })),
            controls: Arc::clone(&controls),
            writer_health: Arc::default(),
        };

        handle.resize(20, 80, 8, 16);
        handle.resize(40, 120, 9, 18);
        handle.nudge_child_redraw_after_handoff(41, 121, 10, 20);

        let controls = controls.lock().expect("controls lock");
        assert_eq!(
            controls.resize,
            Some(PtyResize {
                rows: 40,
                cols: 120,
                cell_width_px: 9,
                cell_height_px: 18,
            })
        );
        assert_eq!(
            controls.nudge,
            Some(PtyResize {
                rows: 41,
                cols: 121,
                cell_width_px: 10,
                cell_height_px: 20,
            })
        );
    }

    #[tokio::test]
    async fn async_user_input_waits_for_queue_capacity() {
        let (data_tx, mut data_rx) = mpsc::channel(1);
        let (control_tx, _control_rx) = std_mpsc::channel();
        data_tx
            .try_send(PtyIoDataCommand::WriteUserInput(Bytes::from_static(
                b"fill",
            )))
            .expect("fill data queue");
        let (wake, _wake_read_fd) = test_wake_pair();
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake,
            user_writes: Arc::new(Mutex::new(UserWriteGate { accepting: true })),
            controls: Arc::new(Mutex::new(SharedPtyControls::default())),
            writer_health: Arc::default(),
        };

        let write = tokio::spawn(async move {
            handle
                .write_user_input(Bytes::from_static(b"wait-for-capacity"))
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !write.is_finished(),
            "async input should wait for queue capacity"
        );

        assert!(matches!(
            data_rx.recv().await,
            Some(PtyIoDataCommand::WriteUserInput(_))
        ));
        write
            .await
            .expect("write task joins")
            .expect("write succeeds after capacity opens");
        match data_rx.recv().await {
            Some(PtyIoDataCommand::WriteUserInput(bytes)) => {
                assert_eq!(bytes, Bytes::from_static(b"wait-for-capacity"));
            }
            _ => panic!("expected queued user input"),
        }
    }

    #[tokio::test]
    async fn async_user_input_waiting_for_capacity_is_rejected_after_handoff_begins() {
        let (data_tx, mut data_rx) = mpsc::channel(1);
        let (control_tx, control_rx) = std_mpsc::channel();
        data_tx
            .try_send(PtyIoDataCommand::WriteUserInput(Bytes::from_static(
                b"fill",
            )))
            .expect("fill data queue");
        let (wake, _wake_read_fd) = test_wake_pair();
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake,
            user_writes: Arc::new(Mutex::new(UserWriteGate { accepting: true })),
            controls: Arc::new(Mutex::new(SharedPtyControls::default())),
            writer_health: Arc::default(),
        };
        let write_handle = handle.clone();
        let write = tokio::spawn(async move {
            write_handle
                .write_user_input(Bytes::from_static(b"after-handoff-start"))
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let handoff = std::thread::spawn(move || handle.begin_handoff(Duration::from_secs(1)));
        match control_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("handoff control command")
        {
            PtyIoControlCommand::BeginHandoff(request) => {
                request
                    .reply
                    .send(Ok(HandoffQuiesce::default()))
                    .expect("handoff waiter alive");
            }
            _ => panic!("expected begin handoff command"),
        }
        handoff
            .join()
            .expect("handoff thread joins")
            .expect("handoff succeeds");
        assert!(matches!(
            data_rx.recv().await,
            Some(PtyIoDataCommand::WriteUserInput(_))
        ));

        let err = write.await.expect("write task joins").expect_err(
            "write waiting for capacity must be rejected after handoff closes the input gate",
        );
        assert_eq!(err.0, Bytes::from_static(b"after-handoff-start"));
        match tokio::time::timeout(Duration::from_millis(50), data_rx.recv()).await {
            Err(_) | Ok(None) => {}
            Ok(Some(_)) => panic!("rejected write must not be queued"),
        }
    }

    #[test]
    fn handoff_control_is_not_blocked_by_full_data_queue() {
        let (data_tx, _data_rx) = mpsc::channel(1);
        let (control_tx, control_rx) = std_mpsc::channel();
        data_tx
            .try_send(PtyIoDataCommand::WriteUserInput(Bytes::from_static(
                b"fill",
            )))
            .expect("fill data queue");
        let (wake, _wake_read_fd) = test_wake_pair();
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake,
            user_writes: Arc::new(Mutex::new(UserWriteGate { accepting: true })),
            controls: Arc::new(Mutex::new(SharedPtyControls::default())),
            writer_health: Arc::default(),
        };

        let handoff = std::thread::spawn(move || handle.begin_handoff(Duration::from_secs(1)));
        match control_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("handoff control command")
        {
            PtyIoControlCommand::BeginHandoff(request) => {
                request
                    .reply
                    .send(Ok(HandoffQuiesce::default()))
                    .expect("handoff waiter alive");
            }
            _ => panic!("expected begin handoff command"),
        }

        handoff
            .join()
            .expect("handoff thread joins")
            .expect("handoff succeeds despite full data queue");
    }

    #[test]
    fn begin_handoff_drains_user_writes_already_in_command_queue() {
        let (mut runner, mut peer, data_tx) = socket_pair_runner();
        data_tx
            .try_send(PtyIoDataCommand::WriteUserInput(Bytes::from_static(
                b"queued-before-ack",
            )))
            .expect("queued write");

        runner
            .begin_handoff(HANDOFF_DRAIN_TIMEOUT, false)
            .expect("handoff drains queued write");

        let mut buf = [0u8; 17];
        peer.read_exact(&mut buf)
            .expect("queued write reaches peer before quiesce ack");
        assert_eq!(&buf, b"queued-before-ack");
        assert_eq!(runner.state, ActorState::Quiesced);
    }

    fn runner_on(
        actor_socket: UnixStream,
        data_rx: mpsc::Receiver<PtyIoDataCommand>,
    ) -> PtyIoActorRunner {
        let (_control_tx, control_rx) = std_mpsc::channel();
        PtyIoActorRunner {
            pane_id: 1,
            file: std::fs::File::from(unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) }),
            data_rx,
            control_rx,
            state: ActorState::Running,
            pending_writes: VecDeque::new(),
            current_write_offset: 0,
            zero_write_streak: 0,
            writer_health: Arc::default(),
            broken_at: None,
            last_broken_warn: None,
            suppressed_breaks: 0,
            draining_for_handoff: false,
            write_hook: None,
            wake_read_fd: fd::create_wake_pipe().expect("wake pipe").read_fd,
            controls: Arc::new(Mutex::new(SharedPtyControls::default())),
            on_read: Box::new(|_| PtyReadResult::empty()),
            on_reader_exit: None,
            poll_observer: None,
        }
    }

    fn socket_pair_runner() -> (PtyIoActorRunner, UnixStream, mpsc::Sender<PtyIoDataCommand>) {
        let (actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");
        let (data_tx, data_rx) = mpsc::channel(ACTOR_COMMAND_BUFFER);
        (runner_on(actor_socket, data_rx), peer, data_tx)
    }

    #[test]
    fn tracked_write_completes_only_after_the_final_partial_write() {
        let (mut runner, mut peer, _) = socket_pair_runner();
        let (completion, receipt) = crate::pty::write_receipt::WriteReceipt::channel();
        runner.enqueue_write(PtyIoDataCommand::WriteTrackedInput(
            Bytes::from_static(b"paste"),
            completion,
        ));
        let mut calls = 0;
        runner.write_hook = Some(Box::new(move |_| {
            calls += 1;
            if calls == 1 {
                Ok(2)
            } else {
                Err(std::io::ErrorKind::WouldBlock.into())
            }
        }));
        runner.flush_pending_writes_once();
        assert_eq!(runner.current_write_offset, 2);
        assert_eq!(receipt.try_completed(), Err(std_mpsc::TryRecvError::Empty));
        runner.write_hook = None;
        let before = Instant::now();
        runner.flush_pending_writes_once();
        let after = Instant::now();
        let at = receipt.try_completed().expect("final write acknowledged");
        assert!(before <= at && at <= after);
        let mut tail = [0; 3];
        peer.read_exact(&mut tail).unwrap();
        assert_eq!(&tail, b"ste");
    }

    #[test]
    fn discarded_tracked_write_does_not_acknowledge_completion() {
        let (mut runner, _, _) = socket_pair_runner();
        let (completion, receipt) = crate::pty::write_receipt::WriteReceipt::channel();
        runner.enqueue_write(PtyIoDataCommand::WriteTrackedInput(
            Bytes::from_static(b"paste"),
            completion,
        ));
        runner.pending_writes.clear();
        assert_eq!(
            receipt.try_completed(),
            Err(std_mpsc::TryRecvError::Disconnected)
        );
    }

    #[test]
    fn empty_user_input_is_never_enqueued() {
        let (data_tx, mut data_rx) = mpsc::channel(4);
        let (control_tx, _control_rx) = std_mpsc::channel();
        let (wake, _wake_read_fd) = test_wake_pair();
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake,
            user_writes: Arc::new(Mutex::new(UserWriteGate { accepting: true })),
            controls: Arc::new(Mutex::new(SharedPtyControls::default())),
            writer_health: Arc::default(),
        };

        handle
            .try_write_user_input(Bytes::new())
            .expect("empty input is accepted as a no-op");
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(handle.write_user_input(Bytes::new()))
            .expect("empty async input is accepted as a no-op");

        assert!(
            data_rx.try_recv().is_err(),
            "an empty payload must not reach the actor's queue (#1011)"
        );
    }

    #[test]
    fn handoff_drains_past_an_empty_write_that_reached_the_actor() {
        // #1011: the live failure. An empty payload at the head of the queue
        // made `write(&[])` return 0 forever and the drain hit its deadline.
        let (mut runner, mut peer, data_tx) = socket_pair_runner();
        data_tx
            .try_send(PtyIoDataCommand::WriteUserInput(Bytes::new()))
            .expect("queued empty write");
        data_tx
            .try_send(PtyIoDataCommand::WriteUserInput(Bytes::from_static(
                b"after-empty",
            )))
            .expect("queued write");
        runner.pending_writes.push_back((Bytes::new(), None));

        let started = Instant::now();
        runner
            .begin_handoff(HANDOFF_DRAIN_TIMEOUT, false)
            .expect("handoff drains past the empty write");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "drain must not spin to its deadline on an empty write"
        );
        let mut buf = [0u8; 11];
        peer.read_exact(&mut buf).expect("later write reaches peer");
        assert_eq!(&buf, b"after-empty");
        assert_eq!(runner.state, ActorState::Quiesced);
    }

    fn zero_write_runner() -> (PtyIoActorRunner, PtyIoActorHandle) {
        let (mut runner, _peer, _data_tx) = socket_pair_runner();
        runner.write_hook = Some(Box::new(|_| Ok(0)));
        let (data_tx, _data_rx) = mpsc::channel(4);
        let (control_tx, _control_rx) = std_mpsc::channel();
        let (wake, _wake_read_fd) = test_wake_pair();
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake,
            user_writes: Arc::new(Mutex::new(UserWriteGate { accepting: true })),
            controls: Arc::new(Mutex::new(SharedPtyControls::default())),
            writer_health: Arc::clone(&runner.writer_health),
        };
        (runner, handle)
    }

    #[test]
    fn zero_byte_flushes_break_the_writer_and_refuse_input_with_an_error() {
        let (mut runner, handle) = zero_write_runner();
        runner.push_pending(Bytes::from_static(b"never-accepted"));

        for _ in 1..ZERO_WRITE_LIMIT {
            runner.flush_pending_writes_once();
        }
        assert!(
            !handle.writer_broken(),
            "retries are bounded, not immediate"
        );
        assert_eq!(runner.pending_writes.len(), 1);
        runner.flush_pending_writes_once();

        assert!(handle.writer_broken());
        assert!(
            runner.pending_writes.is_empty(),
            "queue is dropped, not retried"
        );
        // Input is refused with an error the caller sees, never queued to be
        // dropped behind its back.
        assert!(matches!(
            handle.try_write_user_input(Bytes::from_static(b"later")),
            Err(mpsc::error::TrySendError::Closed(_))
        ));
        assert_eq!(runner.writer_health.take_rejected(), 1);
    }

    #[test]
    fn a_broken_writer_is_rearmed_after_its_cooldown() {
        let (mut runner, handle) = zero_write_runner();
        runner.mark_writer_broken();
        assert!(handle.writer_broken());

        runner.maybe_rearm_writer();
        assert!(handle.writer_broken(), "still inside the cooldown");
        runner.broken_at = Some(Instant::now() - WRITER_REARM_AFTER);
        runner.maybe_rearm_writer();

        assert!(!handle.writer_broken());
        runner.write_hook = None;
        runner.push_pending(Bytes::from_static(b"probe"));
        runner.flush_pending_writes_once();
        assert!(runner.pending_writes.is_empty(), "the probe write lands");
        assert!(!handle.writer_broken());
    }

    #[test]
    fn handoff_rollback_rearms_a_broken_writer() {
        let (mut runner, handle) = zero_write_runner();
        runner.mark_writer_broken();
        let (reply_tx, reply_rx) = std_mpsc::channel();

        runner.handle_control_command(PtyIoControlCommand::RollbackHandoff(reply_tx));

        reply_rx
            .recv()
            .expect("rollback reply")
            .expect("rollback ok");
        assert!(!handle.writer_broken());
    }

    #[test]
    fn writer_broken_warnings_are_rate_limited() {
        let (mut runner, _handle) = zero_write_runner();

        runner.mark_writer_broken();
        runner.mark_writer_broken();
        runner.mark_writer_broken();

        assert_eq!(runner.suppressed_breaks, 2);
        runner.last_broken_warn = Some(Instant::now() - WRITER_BROKEN_WARN_INTERVAL);
        runner.mark_writer_broken();
        assert_eq!(
            runner.suppressed_breaks, 0,
            "reported with the next warning"
        );
    }

    #[test]
    fn plain_handoff_refuses_a_stuck_writer_and_keeps_its_queue() {
        // #1011 review: a writer breaking inside the drain must not empty the
        // queue and report success.
        let (mut runner, handle) = zero_write_runner();
        runner.push_pending(Bytes::from_static(b"keep-me"));

        let started = Instant::now();
        let err = runner
            .begin_handoff(HANDOFF_DRAIN_TIMEOUT, false)
            .expect_err("a stuck writer fails the handoff");

        let timeout = HandoffDrainTimeout::from_io_error(&err).expect("typed drain error");
        assert!(timeout.writer_stuck);
        assert_eq!(timeout.pending_bytes, 7);
        assert_eq!(runner.pending_bytes(), 7, "the old server keeps every byte");
        assert!(!handle.writer_broken());
        assert_eq!(runner.state, ActorState::Running);
        // The streak restarts, so the next zero write after the refused
        // handoff does not break the writer and drop the queue just kept.
        assert_eq!(runner.zero_write_streak, 0);
        runner.flush_pending_writes_once();
        assert!(!handle.writer_broken());
        assert_eq!(runner.pending_bytes(), 7);
        assert!(
            started.elapsed() < HANDOFF_DRAIN_TIMEOUT,
            "a stuck writer fails fast, not at the deadline"
        );
    }

    #[test]
    fn skip_undrained_reports_a_stuck_writer() {
        let (mut runner, _handle) = zero_write_runner();
        runner.push_pending(Bytes::from_static(b"drop-me"));

        let quiesce = runner
            .begin_handoff(HANDOFF_DRAIN_TIMEOUT, true)
            .expect("--skip-undrained hands off past a stuck writer");

        assert_eq!(quiesce.dropped_bytes, 7);
        assert!(quiesce.writer_stuck);
        assert_eq!(runner.state, ActorState::Quiesced);
    }

    fn runner_with_undrainable_write() -> (PtyIoActorRunner, UnixStream) {
        let (mut runner, peer, _data_tx) = socket_pair_runner();
        // Larger than any socket buffer, and the peer never reads.
        runner.push_pending(Bytes::from(vec![b'x'; 8 * 1024 * 1024]));
        (runner, peer)
    }

    #[test]
    fn handoff_drain_timeout_reports_pending_bytes() {
        let (mut runner, _peer) = runner_with_undrainable_write();

        let err = runner
            .begin_handoff(Duration::from_millis(50), false)
            .expect_err("an unread peer cannot drain");

        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        let timeout = HandoffDrainTimeout::from_io_error(&err).expect("typed drain timeout");
        assert!(timeout.pending_bytes > 0);
        assert_eq!(timeout.pending_bytes, runner.pending_bytes());
        assert!(err
            .to_string()
            .contains(&format!("({} bytes pending)", timeout.pending_bytes)));
        assert_eq!(runner.state, ActorState::Running);
    }

    #[test]
    fn handoff_drop_undrained_quiesces_and_counts_what_it_dropped() {
        let (mut runner, _peer) = runner_with_undrainable_write();

        let quiesce = runner
            .begin_handoff(Duration::from_millis(50), true)
            .expect("--skip-undrained hands off anyway");

        assert!(quiesce.dropped_bytes > 0);
        assert!(runner.pending_writes.is_empty());
        assert_eq!(runner.state, ActorState::Quiesced);
    }

    #[test]
    fn release_after_commit_prevents_further_io() {
        let (handle, mut peer, read_rx) = actor_with_socket_pair(false);

        handle.release_after_commit().expect("actor released");
        assert!(handle
            .try_write_user_input(Bytes::from_static(b"blocked"))
            .is_err());

        let _ = peer.write_all(b"ignored");
        assert!(read_rx.recv_timeout(Duration::from_millis(150)).is_err());
    }
}
