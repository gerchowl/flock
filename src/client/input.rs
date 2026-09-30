//! Stdin input reading for the thin client.
//!
//! Reads stdin bytes and forwards framed input to the main event loop.
//! Unlike the monolithic flock, the thin client does NOT parse input into
//! key/mouse/paste events. It keeps enough byte-framing state to avoid splitting
//! terminal control strings, then sends bytes to the server as `ClientMessage::Input`.
//! The server handles semantic parsing.
//!
//! This is simpler and more reliable because:
//! - The server has the same input parsing code
//! - We avoid duplicating parsing logic in the client
//! - Host terminal control replies can be buffered or discarded before they leak

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use tokio::sync::mpsc;

/// Depth of the stdin → main-loop channel, in framed chunks.
const STDIN_CHANNEL_DEPTH: usize = 256;

/// How long the framer waits for the rest of a split escape sequence before
/// flushing what it holds.
const STDIN_FRAME_FLUSH_MS: i32 = 10;

// ---------------------------------------------------------------------------
// Stdin reader thread
// ---------------------------------------------------------------------------

/// The client's stdin reader: one thread that reads raw bytes from the
/// terminal and hands framed chunks to the main loop through [`Self::rx`].
///
/// It is spawned once per client leg and outlives individual attach attempts
/// (the live-handoff retry loop, the #436 reconnect), so no stdin byte is ever
/// stranded in a session-scoped reader.
///
/// It MUST end with its leg. The launcher runs the first leg in process, and
/// a reader left parked in `read(0)` after that leg returned keeps competing
/// for the tty with every later leg (#352). The next leg's theme capture polls
/// stdin, sees the terminal's color reply arrive, and calls `read` — but the
/// orphan reader in the launcher has already taken those bytes, so the capture
/// blocks until the operator's next keystroke. That was the 8 s grey screen:
/// the new client could not connect to its bridge until someone pressed Esc.
/// The reader therefore parks in `poll` on stdin AND a wake pipe, and
/// dropping this handle writes the pipe and joins the thread.
pub struct StdinReader {
    /// Framed stdin chunks. Declared before the thread so it drops first: a
    /// reader blocked on a full channel then sees the send fail and exits,
    /// and the join below cannot hang on it.
    pub rx: mpsc::Receiver<Vec<u8>>,
    wake: Option<OwnedFd>,
    thread: Option<JoinHandle<()>>,
}

impl StdinReader {
    /// Spawn the reader over the process's stdin.
    pub fn spawn_stdin(should_quit: Arc<AtomicBool>) -> io::Result<Self> {
        Self::spawn(libc::STDIN_FILENO, should_quit)
    }

    /// Spawn the reader over `fd`, which must stay open for the reader's
    /// lifetime. Separate from [`Self::spawn_stdin`] so tests can drive it
    /// with a pipe.
    pub(crate) fn spawn(fd: RawFd, should_quit: Arc<AtomicBool>) -> io::Result<Self> {
        let (wake_read, wake_write) = wake_pipe()?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>(STDIN_CHANNEL_DEPTH);
        let thread = std::thread::Builder::new()
            .name("flock-stdin".to_string())
            .spawn(move || {
                stdin_reader_loop(fd, wake_read, tx, &should_quit);
            })?;
        Ok(Self {
            rx,
            wake: Some(wake_write),
            thread: Some(thread),
        })
    }
}

impl Drop for StdinReader {
    fn drop(&mut self) {
        // Closing the channel first unblocks a reader parked in a full send.
        self.rx.close();
        if let Some(wake) = self.wake.take() {
            let byte = [1u8];
            // SAFETY: `wake` is the open write end of our own pipe and `byte`
            // outlives the call. A failed write still leaves the close below,
            // which the reader sees as POLLHUP.
            unsafe {
                libc::write(wake.as_raw_fd(), byte.as_ptr().cast(), byte.len());
            }
            drop(wake);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A close-on-exec pipe: `(read end, write end)`. Close-on-exec so a child the
/// client spawns never holds the write end open.
fn wake_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a valid two-element buffer for pipe(2).
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe(2) succeeded, so both descriptors are open and owned here.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in [&read, &write] {
        // SAFETY: plain fcntl on a descriptor we own.
        unsafe {
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    Ok((read, write))
}

/// What woke the reader.
#[derive(Debug, PartialEq, Eq)]
enum ReaderWake {
    Input,
    Stop,
}

/// Park until stdin is readable or the wake pipe fires. The wake pipe wins a
/// tie: a leg that is ending must not take one more chunk from the next leg.
fn wait_for_input_or_stop(fd: RawFd, wake: RawFd) -> io::Result<ReaderWake> {
    let mut fds = [
        libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        // SAFETY: `fds` is a valid array of two pollfd structs.
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if result < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if fds[1].revents != 0 {
            return Ok(ReaderWake::Stop);
        }
        if fds[0].revents != 0 {
            return Ok(ReaderWake::Input);
        }
    }
}

/// One `read(2)` on `fd`, straight to the descriptor.
///
/// Deliberately not `io::stdin()`: its handle is a process-wide lock around a
/// buffer, and a reader parked inside it held that lock across the in-process
/// leg boundary, so the next leg's `stdin().lock()` waited for a keystroke
/// too (#352). Reading the descriptor keeps no lock and no hidden buffer.
pub(crate) fn read_fd(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: `buf` is a valid writable buffer of `buf.len()` bytes.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// Reads raw bytes from `fd` and sends framed chunks to the main event loop
/// until EOF, a read error, the main loop hanging up, or the wake pipe firing.
fn stdin_reader_loop(
    fd: RawFd,
    wake: OwnedFd,
    stdin_tx: mpsc::Sender<Vec<u8>>,
    should_quit: &Arc<AtomicBool>,
) {
    let mut scratch = [0u8; 4096];
    let mut framer = crate::raw_input::RawInputByteFramer::default();

    while !should_quit.load(Ordering::Acquire) {
        match wait_for_input_or_stop(fd, wake.as_raw_fd()) {
            Ok(ReaderWake::Input) => {}
            Ok(ReaderWake::Stop) | Err(_) => break,
        }
        match read_fd(fd, &mut scratch) {
            Ok(0) => break,
            Ok(n) => {
                for data in framer.push(&scratch[..n]) {
                    if stdin_tx.blocking_send(data).is_err() {
                        return;
                    }
                }

                if poll_read_ready(fd, STDIN_FRAME_FLUSH_MS) == Some(false) {
                    for data in framer.flush_timeout() {
                        if stdin_tx.blocking_send(data).is_err() {
                            return;
                        }
                    }
                }
            }
            Err(err) => {
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
        }
    }
}

/// Whether `fd` has input within `timeout_ms`; `None` when poll itself failed.
pub(crate) fn poll_read_ready(fd: i32, timeout_ms: i32) -> Option<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };

    // SAFETY: `pfd` is a single valid pollfd.
    let result = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if result < 0 {
        None
    } else {
        Some(result > 0)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn stdin_input_event_carries_raw_bytes() {
        let data = vec![0x1b, b'[', b'A']; // Up arrow escape sequence
        let event = crate::client::ClientLoopEvent::StdinInput(data.clone());
        match event {
            crate::client::ClientLoopEvent::StdinInput(d) => assert_eq!(d, data),
            _ => panic!("expected StdinInput event"),
        }
    }

    /// Drop `reader` on a helper thread and report whether the drop (which
    /// joins the reader thread) finished within `timeout`.
    fn drops_within(reader: StdinReader, timeout: Duration) -> bool {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(reader);
            let _ = done_tx.send(());
        });
        done_rx.recv_timeout(timeout).is_ok()
    }

    fn write_all(fd: &OwnedFd, bytes: &[u8]) {
        // SAFETY: `fd` is the open write end of a test pipe.
        let n = unsafe { libc::write(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
        assert_eq!(n, bytes.len() as isize, "short write to the test pipe");
    }

    /// #352: a leg's reader must stop when its leg ends, with no keystroke to
    /// wake it, and must not take the bytes meant for the NEXT leg's reader.
    ///
    /// Before the fix the reader parked in a blocking `read(0)`: the drop
    /// below never returned, and the first bytes written afterwards went to
    /// the orphan instead of the reader that was waiting for them.
    #[test]
    fn an_ended_leg_reader_stops_without_input_and_leaves_later_bytes_alone() {
        let (tty_read, tty_write) = wake_pipe().expect("test pipe");
        let quit = Arc::new(AtomicBool::new(false));

        let first_leg =
            StdinReader::spawn(tty_read.as_raw_fd(), quit.clone()).expect("spawn first reader");
        assert!(
            drops_within(first_leg, Duration::from_secs(5)),
            "the first leg's reader stayed parked on stdin after its leg ended"
        );

        write_all(&tty_write, b"next");
        let mut next_leg =
            StdinReader::spawn(tty_read.as_raw_fd(), quit).expect("spawn next reader");
        let mut received = Vec::new();
        while received.len() < b"next".len() {
            match next_leg.rx.blocking_recv() {
                Some(chunk) => received.extend_from_slice(&chunk),
                None => break,
            }
        }
        assert_eq!(received, b"next", "the next leg must get every byte");
        assert!(drops_within(next_leg, Duration::from_secs(5)));
    }

    /// A reader parked on a FULL channel (a main loop that stopped draining
    /// stdin) must still stop when dropped.
    #[test]
    fn a_reader_blocked_on_a_full_channel_still_stops() {
        let (tty_read, tty_write) = wake_pipe().expect("test pipe");
        let reader = StdinReader::spawn(tty_read.as_raw_fd(), Arc::new(AtomicBool::new(false)))
            .expect("spawn reader");
        for _ in 0..(STDIN_CHANNEL_DEPTH + 8) {
            write_all(&tty_write, b"x");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(drops_within(reader, Duration::from_secs(5)));
    }
}
