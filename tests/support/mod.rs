#![allow(dead_code)]

use std::collections::HashSet;
use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

pub mod compatibility;
pub mod fleet;
pub mod process_table;

pub mod environment;

static PID_REGISTRY: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
static RUNTIME_DIR_REGISTRY: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
static INIT: Once = Once::new();
static CLEANUP_GUARD: OnceLock<CleanupGuard> = OnceLock::new();
const WATCHDOG_SCAN_INTERVAL: Duration = Duration::from_secs(1);
const RUNTIME_OWNER_MARKER: &str = ".flock-test-owner-pid";

pub fn register_spawned_flock_pid(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };

    ensure_cleanup_hooks();
    let mut registry = pid_registry_lock();
    registry.insert(pid);
}

pub fn unregister_spawned_flock_pid(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };

    if let Some(registry) = PID_REGISTRY.get() {
        let mut guard = registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.remove(&pid);
    }
}

pub fn register_runtime_dir(path: &Path) {
    ensure_cleanup_hooks();

    let _ = fs::create_dir_all(path);
    let _ = fs::write(
        path.join(RUNTIME_OWNER_MARKER),
        std::process::id().to_string(),
    );

    let mut runtime_dirs = runtime_dir_registry_lock();
    runtime_dirs.insert(path.to_path_buf());
}

pub fn unregister_runtime_dir(path: &Path) {
    if let Some(registry) = RUNTIME_DIR_REGISTRY.get() {
        let mut guard = registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.remove(path);
    }
}

/// Every `flk server` on this machine whose runtime dir is `runtime_dir`.
///
/// No longer Linux-gated. The gate existed only because the sweep behind it
/// walked `/proc`, so on a Mac it answered "no servers" for servers that were
/// provably running — a wrong answer, not a small one, and callers had no way
/// to tell it from an honest one.
pub fn flock_server_pids_for_runtime_dir(runtime_dir: &Path) -> std::io::Result<Vec<u32>> {
    let mut pids = Vec::new();
    for (pid, _) in iter_worktree_server_pids()? {
        // Skip, not propagate. This helper is reachable on macOS as of #521,
        // where `process_environment` returns `Err` for any pid that exits
        // mid-sweep or cannot be inspected — measured at 407 of 1148 pids on a
        // real machine. Propagating would turn one unreadable process into an
        // empty answer for the entire machine, which is indistinguishable from
        // "no servers running".
        if runtime_dir_for(pid).as_deref() == Some(runtime_dir) {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    Ok(pids)
}

pub fn cleanup_test_base(base: &Path) {
    let runtime_dir = base.join("runtime");
    let runtime_dirs = HashSet::from([runtime_dir.clone()]);

    terminate_servers_for_runtime_dirs(&runtime_dirs);
    terminate_servers_under_base(base);
    unregister_runtime_dir(&runtime_dir);
    let _ = fs::remove_dir_all(base);
}

/// Kill anything still running out of `base`, matched on its command line.
///
/// The pid registry only knows servers this harness SPAWNED. A live handoff
/// replaces the server with a NEW process, and of the twelve
/// `server.live_handoff` calls in the suite only one looks that pid up (it
/// needs it for an fd assertion) — the other eleven leave a
/// `flk server --handoff-import <base>/...` behind that nothing owns.
///
/// `terminate_servers_for_runtime_dirs` is the other backstop, and one that
/// needs no argv match at all — but it can only attribute a process through its
/// environment, so it cannot see a daemon whose argv is a bare `flk server` with
/// no path in it (#521).
///
/// Matching on the base path is what makes this reliable without every call
/// site having to remember: `base` is a unique per-test temp dir, so any
/// process still referencing it belongs to this test. Cheap to run
/// unconditionally at teardown, and a no-op when nothing leaked.
#[allow(clippy::disallowed_methods)] // Test teardown shells out to pgrep — TracedCommand polices product code.
fn terminate_servers_under_base(base: &Path) {
    let Some(base) = base.to_str() else {
        return;
    };
    // `pgrep -f` matches the whole command line, which is where the base path
    // shows up (`--handoff-import <base>/config/...`).
    let Ok(output) = std::process::Command::new("pgrep")
        .args(["-f", base])
        .output()
    else {
        return;
    };
    let own_pid = std::process::id();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(pid) = line.trim().parse::<u32>() else {
            continue;
        };
        if pid != own_pid {
            terminate_pid(pid);
        }
    }
}

pub fn wait_for_socket(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() && UnixStream::connect(path).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("socket did not appear at {}", path.display());
}

pub fn wait_for_file(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("file did not appear at {}", path.display());
}

pub fn encode_varint_u32(v: u32) -> Vec<u8> {
    if v < 251 {
        vec![v as u8]
    } else if v < 65536 {
        let mut buf = vec![251u8];
        buf.extend_from_slice(&(v as u16).to_le_bytes());
        buf
    } else {
        let mut buf = vec![252u8];
        buf.extend_from_slice(&v.to_le_bytes());
        buf
    }
}

pub fn encode_varint_u16(v: u16) -> Vec<u8> {
    if v < 251 {
        vec![v as u8]
    } else {
        let mut buf = vec![251u8];
        buf.extend_from_slice(&v.to_le_bytes());
        buf
    }
}

pub fn frame_message(payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut framed = len.to_le_bytes().to_vec();
    framed.extend_from_slice(payload);
    framed
}

/// The wire protocol version these integration tests handshake with.
///
/// Restated here because integration tests link the binary, not the library,
/// so they cannot import `protocol::PROTOCOL_VERSION`. Keeping it in ONE place
/// means a protocol bump touches one line rather than the dozen call sites
/// that used to hardcode the number — which is how a bump used to fail a
/// terminal-size test for reasons that had nothing to do with terminal sizes.
pub const PROTOCOL_VERSION: u32 = 27;

/// The live-handoff refusal notice a server sends a connecting client while a
/// live update is in flight (#38). A client that reads it opens its retry
/// window. Mirror of `protocol::LIVE_HANDOFF_ATTACH_NOTICE`.
pub const LIVE_HANDOFF_ATTACH_NOTICE: &str =
    "live update in progress; reconnect after handoff completes";

/// A framed `Welcome { version, encoding: SemanticFrame, error: Some(notice) }`
/// rejecting the client with the live-handoff notice — the exact bytes a
/// mid-handoff server writes to a pending client. Used to drive a real `flock
/// client` into its #52 retry window from a test stand-in server.
pub fn encode_live_handoff_refusal(version: u32) -> Vec<u8> {
    let notice = LIVE_HANDOFF_ATTACH_NOTICE.as_bytes();
    let mut error_field = vec![1u8]; // Option::Some tag
    error_field.extend_from_slice(&encode_varint_u32(notice.len() as u32));
    error_field.extend_from_slice(notice);

    let payload = encode_varint_enum(
        0, // ServerMessage::Welcome
        &[
            &encode_varint_u32(version),
            &encode_varint_u32(0), // RenderEncoding::SemanticFrame
            &error_field,
        ],
    );
    frame_message(&payload)
}

pub fn decode_varint_u32(payload: &[u8], offset: usize) -> Result<(u32, usize), String> {
    if offset >= payload.len() {
        return Err("payload too short for varint".into());
    }
    let first_byte = payload[offset];
    match first_byte {
        0..=250 => Ok((first_byte as u32, 1)),
        251 => {
            if offset + 3 > payload.len() {
                return Err("payload too short for u16 varint".into());
            }
            let v = u16::from_le_bytes(
                payload[offset + 1..offset + 3]
                    .try_into()
                    .map_err(|e: std::array::TryFromSliceError| e.to_string())?,
            );
            Ok((v as u32, 3))
        }
        252 => {
            if offset + 5 > payload.len() {
                return Err("payload too short for u32 varint".into());
            }
            let v = u32::from_le_bytes(
                payload[offset + 1..offset + 5]
                    .try_into()
                    .map_err(|e: std::array::TryFromSliceError| e.to_string())?,
            );
            Ok((v, 5))
        }
        _ => Err(format!("unsupported varint tag: {first_byte}")),
    }
}

fn encode_varint_enum(variant_idx: u32, fields: &[&[u8]]) -> Vec<u8> {
    let mut buf = encode_varint_u32(variant_idx);
    for field in fields {
        buf.extend_from_slice(field);
    }
    buf
}

fn decode_welcome(payload: &[u8]) -> Result<(u32, Option<String>), String> {
    let mut offset = 0;
    let (variant, consumed) = decode_varint_u32(payload, offset)?;
    offset += consumed;
    if variant != 0 {
        return Err(format!(
            "expected Welcome (variant 0), got variant {variant}"
        ));
    }

    let (version, consumed) = decode_varint_u32(payload, offset)?;
    offset += consumed;

    let (_encoding, consumed) = decode_varint_u32(payload, offset)?;
    offset += consumed;

    if offset >= payload.len() {
        return Err("payload too short for Option tag".into());
    }
    let option_tag = payload[offset];
    offset += 1;

    let error = if option_tag == 1 {
        let (str_len, consumed) = decode_varint_u32(payload, offset)?;
        offset += consumed;
        let str_len = str_len as usize;
        if offset + str_len > payload.len() {
            return Err("payload too short for string content".into());
        }
        Some(
            String::from_utf8(payload[offset..offset + str_len].to_vec())
                .map_err(|e| e.to_string())?,
        )
    } else {
        None
    };

    Ok((version, error))
}

pub fn client_handshake(
    stream: &mut UnixStream,
    version: u32,
    cols: u16,
    rows: u16,
) -> Result<(u32, Option<String>), String> {
    // fleet: Option<FleetSnapshot> = None (single 0 byte),
    // host_theme: Option<TerminalTheme> = None (single 0 byte).
    client_handshake_with_fleet_and_theme(stream, version, cols, rows, &[0], &[0])
}

/// Handshake whose Hello carries pre-encoded `fleet: Option<FleetSnapshot>`
/// bytes — e.g. spliced verbatim from a received SwitchServer payload, the
/// same bytes a switching client would forward.
pub fn client_handshake_with_fleet(
    stream: &mut UnixStream,
    version: u32,
    cols: u16,
    rows: u16,
    fleet_option_bytes: &[u8],
) -> Result<(u32, Option<String>), String> {
    client_handshake_with_fleet_and_theme(stream, version, cols, rows, fleet_option_bytes, &[0])
}

/// Handshake whose Hello carries a `host_theme: Option<TerminalTheme>` with
/// both default colors set, e.g. `encode_host_theme_option(...)`.
#[allow(dead_code)]
pub fn client_handshake_with_theme(
    stream: &mut UnixStream,
    version: u32,
    cols: u16,
    rows: u16,
    theme_option_bytes: &[u8],
) -> Result<(u32, Option<String>), String> {
    client_handshake_with_fleet_and_theme(stream, version, cols, rows, &[0], theme_option_bytes)
}

/// Encodes `Some(TerminalTheme { foreground: Some(fg), background: Some(bg) })`
/// the way bincode lays it out inside the Hello.
#[allow(dead_code)]
pub fn encode_host_theme_option(fg: (u8, u8, u8), bg: (u8, u8, u8)) -> Vec<u8> {
    vec![1, 1, fg.0, fg.1, fg.2, 1, bg.0, bg.1, bg.2]
}

pub fn client_handshake_with_fleet_and_theme(
    stream: &mut UnixStream,
    version: u32,
    cols: u16,
    rows: u16,
    fleet_option_bytes: &[u8],
    theme_option_bytes: &[u8],
) -> Result<(u32, Option<String>), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;

    let hello_payload = encode_varint_enum(
        0,
        &[
            &encode_varint_u32(version),
            &encode_varint_u16(cols),
            &encode_varint_u16(rows),
            &encode_varint_u32(8),  // cell_width_px
            &encode_varint_u32(16), // cell_height_px
            &encode_varint_u32(0),  // RenderEncoding::SemanticFrame
            &encode_varint_u32(0),  // ClientKeybindings::Server
            &encode_varint_u32(0),  // ClientLaunchMode::App
            fleet_option_bytes,
            theme_option_bytes,
            &[0], // notice: Option<String> = None (single 0 byte)
        ],
    );
    let framed = frame_message(&hello_payload);
    stream.write_all(&framed).map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;

    let grace = completion_grace_for(stream);
    let payload = read_framed_payload(stream, grace).map_err(|e| e.to_string())?;
    decode_welcome(&payload)
}

/// How long a frame that has already *started* arriving is given to finish.
///
/// The server writes each message with a single `write_all`, so a frame either
/// arrives whole or the peer is gone; this exists only so a peer that sends a
/// prefix and then neither data nor EOF cannot hang a test. Five seconds is
/// far longer than a socket buffer needs to hand over a frame that is already
/// written, and it sits below every wait budget in this suite. It is *not* a
/// substitute for those budgets: a reader is given a grace derived from the
/// caller's own poll slice (see [`completion_grace_for`]) and capped here.
const FRAME_COMPLETION_CAP: Duration = Duration::from_secs(5);

/// How many poll slices a started frame gets to finish over and above the one
/// that read its prefix.
const FRAME_COMPLETION_SLICES: u32 = 8;

/// The least patience a started frame gets, whatever slice the caller polls on.
///
/// The slice answers "how often do I check", not "how long do I wait", so
/// multiplying it alone would let a reader polling every 50 ms declare a frame
/// lost after 400 ms — which is how the first version of this went flaky
/// against its own regression test. Two seconds is the floor because that is
/// roughly what a socket needs to hand over a frame the peer has already
/// written, and it is still well inside every wait budget in this suite.
const FRAME_COMPLETION_FLOOR: Duration = Duration::from_secs(2);

/// The grace to give a frame that has started arriving on `stream`.
///
/// Derived from the caller's own poll slice so a reader cannot quietly outlast
/// the deadline it sits inside, then clamped: a 200 ms `drain_messages` and a
/// 10 s `read_next_frame_payload` both stay bounded, and neither can talk a
/// reader into abandoning a frame that was merely slow. The cap also covers the
/// blocking handshake reads, which set no read timeout at all and would
/// otherwise get an unbounded grace.
pub fn completion_grace_for(stream: &UnixStream) -> Duration {
    let slice = stream
        .read_timeout()
        .ok()
        .flatten()
        .unwrap_or(DEFAULT_READ_SLICE);
    (slice * FRAME_COMPLETION_SLICES).clamp(FRAME_COMPLETION_FLOOR, FRAME_COMPLETION_CAP)
}

/// Read one length-prefixed message off `stream`, keeping whatever has already
/// arrived instead of throwing it away.
///
/// ## Why this is not `read_exact` twice
///
/// The wire is length-prefixed, so the reader has to know where a message
/// ends. `read_exact` cannot be interrupted without losing that knowledge: if
/// it times out having consumed part of a payload, those bytes are gone, and
/// the next call reads payload bytes as if they were the next length prefix.
/// Every frame after that is garbage, and no amount of further waiting
/// recovers — the message the test was looking for is unreachable for the rest
/// of the run.
///
/// That is not hypothetical. A pane frame is tens of kilobytes, and these
/// readers poll on short slices (75–400 ms) so they can notice a peer going
/// away. Under parallel load a slice is not a guarantee, and
/// `client_mode::resume_reasserts_geometry_so_panes_render_at_new_width` and
/// `multi_client::multi_client_broadcasts_frame_updates_to_all_clients` both
/// desynchronized this way and then waited out their full budget against a
/// dead stream (#444).
///
/// ## The policy, and what it promises
///
/// * **Before** any byte of the frame has arrived, a timeout is an ordinary
///   "nothing yet": the caller may retry, because framing is intact. This is
///   the only retryable outcome, and it is the only one with a timeout *kind*.
/// * **After** the prefix, the frame is finished or the stream is reported
///   lost. When `completion_grace` runs out the error is `InvalidData`, not a
///   timeout — a caller that polls on timeout would otherwise retry against a
///   half-consumed frame and desynchronize, which is the very thing this
///   function exists to prevent. Losing the stream loudly beats losing it
///   quietly.
///
/// The socket's read timeout, which the caller owns and sets as it always
/// has, still governs the first byte.
///
/// Note that a read timeout surfaces as `TimedOut` on Linux but as
/// `WouldBlock` on macOS, so both kinds are treated as "not yet" here; a
/// caller that genuinely cannot wait asks for
/// [`read_framed_io_nonblocking`], which is a different question rather than
/// a guess about which platform it is on.
pub fn read_framed_message(stream: &mut UnixStream) -> Result<(u32, Vec<u8>), String> {
    read_framed_io(stream, completion_grace_for(stream)).map_err(|e| e.to_string())
}

/// [`read_framed_message`] for a socket the caller has put in nonblocking mode
/// to poll it, which therefore cannot be made to wait out a frame it has
/// already started. Only `wait_for_disconnect` needs this.
pub fn read_framed_message_nonblocking(stream: &mut UnixStream) -> Result<(u32, Vec<u8>), String> {
    read_framed_io_nonblocking(stream).map_err(|e| e.to_string())
}

/// [`read_framed_message`] after setting the socket's read timeout, for callers
/// that would rather name the slice than set it themselves.
pub fn read_framed_message_within(
    stream: &mut UnixStream,
    timeout: Duration,
) -> Result<(u32, Vec<u8>), String> {
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("set read timeout: {e}"))?;
    read_framed_message(stream)
}

/// The slice a reader waits for a message's *first* byte. Short, so a wait
/// loop notices a peer that has gone away; long enough not to be a busy poll.
pub const DEFAULT_READ_SLICE: Duration = Duration::from_millis(200);

/// [`read_framed_message`] under the name the wait loops already use, with the
/// read timeout left exactly as the caller set it.
pub fn read_server_message(stream: &mut UnixStream) -> Result<(u32, Vec<u8>), String> {
    read_framed_message(stream)
}

/// Read one frame's raw payload, variant prefix included.
///
/// What the handshake decoders want: they parse the variant themselves out of
/// the front of the buffer. Sharing the framing with [`read_framed_io`] is the
/// point — these readers used to be private `read_exact` copies, which is how
/// the same defect ended up in five places.
pub fn read_framed_payload(
    stream: &mut UnixStream,
    completion_grace: Duration,
) -> io::Result<Vec<u8>> {
    let payload = read_frame(stream, completion_grace)?;
    if payload.len() < 2 {
        // A payload too short to even hold a varint variant.
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("payload too short for a variant: {} bytes", payload.len()),
        ));
    }
    Ok(payload)
}

/// [`read_framed_message`], keeping the `io::ErrorKind` intact so a caller that
/// tells "nothing yet" apart from "the stream is gone" — `multi_client.rs`
/// and `cross_area.rs` do, through their `is_timeout` — still can.
pub fn read_framed_io(
    stream: &mut UnixStream,
    completion_grace: Duration,
) -> io::Result<(u32, Vec<u8>)> {
    let payload = read_frame(stream, completion_grace)?;
    let (variant, consumed) =
        decode_varint_u32(&payload, 0).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    Ok((variant, payload[consumed..].to_vec()))
}

/// [`read_framed_io`] for a socket the caller is polling in nonblocking mode.
pub fn read_framed_io_nonblocking(stream: &mut UnixStream) -> io::Result<(u32, Vec<u8>)> {
    let payload = read_frame(stream, Duration::ZERO)?;
    let (variant, consumed) =
        decode_varint_u32(&payload, 0).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    Ok((variant, payload[consumed..].to_vec()))
}

/// The one place a frame is read off a socket. Everything above is a policy
/// choice about how long to wait; the framing lives here exactly once.
fn read_frame(stream: &mut UnixStream, completion_grace: Duration) -> io::Result<Vec<u8>> {
    // A zero grace means "a frame that has started cannot be waited for",
    // which is what a nonblocking poller needs.
    let wait_out_partial = !completion_grace.is_zero();
    let can_wait = |filled: usize| wait_out_partial && filled > 0;

    // The prefix is four bytes and nothing has been consumed yet, so a timeout
    // here is safe to report as "nothing yet" — but a timeout *partway* through
    // it has already eaten bytes, so those are retained and the wait continues.
    let mut len_buf = [0u8; 4];
    let mut prefix_filled = 0;
    let grace_deadline = Instant::now() + completion_grace;
    while prefix_filled < len_buf.len() {
        match stream.read(&mut len_buf[prefix_filled..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "peer closed before a length prefix",
                ))
            }
            Ok(n) => prefix_filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if is_read_timeout(&e) => {
                if can_wait(prefix_filled) && Instant::now() < grace_deadline {
                    continue;
                }
                // Nothing consumed: the only retryable outcome, so it keeps the
                // timeout kind and every existing wait loop still works.
                if prefix_filled == 0 {
                    return Err(prefix_error(&e));
                }
                return Err(abandoned("length prefix", prefix_filled, len_buf.len()));
            }
            Err(e) => return Err(prefix_error(&e)),
        }
    }

    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 2 * 1024 * 1024 {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("oversized frame: {len} bytes"),
        ));
    }
    if len == 0 {
        return Err(io::Error::new(ErrorKind::InvalidData, "zero-length frame"));
    }

    let mut payload = vec![0u8; len];
    let mut filled = 0;
    while filled < payload.len() {
        match stream.read(&mut payload[filled..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    format!(
                        "peer closed with {filled} of {} payload bytes",
                        payload.len()
                    ),
                ))
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if is_read_timeout(&e) => {
                if wait_out_partial && Instant::now() < grace_deadline {
                    continue;
                }
                // Never a timeout kind once bytes are in hand: a caller that
                // polls on timeout would retry against a half-read frame and
                // resynchronize, which is the failure this reader removes.
                return Err(abandoned("payload", filled, payload.len()));
            }
            Err(e) => return Err(payload_error(&e)),
        }
    }

    Ok(payload)
}

/// A frame abandoned partway through. `InvalidData`, never a timeout: see
/// [`read_frame`]. The byte counts are in the message because "the reader gave
/// up" without them is indistinguishable from "the peer sent a short frame".
fn abandoned(what: &str, filled: usize, total: usize) -> io::Error {
    io::Error::new(
        ErrorKind::InvalidData,
        format!(
            "frame abandoned after {filled} of {total} {what} bytes within the completion grace"
        ),
    )
}

/// Keep the *kind* a timeout arrived with — `TimedOut` on Linux, `WouldBlock`
/// on macOS — because that is what the wait loops poll on.
fn prefix_error(err: &io::Error) -> io::Error {
    io::Error::new(err.kind(), format!("read length prefix: {err}"))
}

fn payload_error(err: &io::Error) -> io::Error {
    io::Error::new(err.kind(), format!("read payload: {err}"))
}

/// A socket read that ran out of time. Linux says `TimedOut`, macOS says
/// `WouldBlock`; `tests/multi_client.rs::is_timeout` already matches both, and
/// this is that same rule with a name.
fn is_read_timeout(err: &io::Error) -> bool {
    matches!(err.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock)
}

pub fn send_input(stream: &mut UnixStream, data: &[u8]) -> Result<(), String> {
    let mut buf = encode_varint_u32(1);
    buf.extend_from_slice(&encode_varint_u32(data.len() as u32));
    buf.extend_from_slice(data);
    let framed = frame_message(&buf);
    stream
        .write_all(&framed)
        .map_err(|e| format!("write input: {e}"))?;
    stream.flush().map_err(|e| format!("flush input: {e}"))?;
    Ok(())
}

/// Send `ClientMessage::ClipboardImage { extension, data }` — the wire a
/// dropped file takes (#79/#286). Variant index 2, then the extension string
/// and the byte payload, each varint-length-prefixed.
#[allow(dead_code)]
pub fn send_clipboard_file(
    stream: &mut UnixStream,
    extension: &str,
    data: &[u8],
) -> Result<(), String> {
    let mut buf = encode_varint_u32(2);
    buf.extend_from_slice(&encode_varint_u32(extension.len() as u32));
    buf.extend_from_slice(extension.as_bytes());
    buf.extend_from_slice(&encode_varint_u32(data.len() as u32));
    buf.extend_from_slice(data);
    let framed = frame_message(&buf);
    stream
        .write_all(&framed)
        .map_err(|e| format!("write clipboard file: {e}"))?;
    stream
        .flush()
        .map_err(|e| format!("flush clipboard file: {e}"))?;
    Ok(())
}

pub fn send_detach(stream: &mut UnixStream) -> Result<(), String> {
    let detach_payload = encode_varint_u32(4);
    let framed = frame_message(&detach_payload);
    stream
        .write_all(&framed)
        .map_err(|e| format!("write detach: {e}"))?;
    stream.flush().map_err(|e| format!("flush detach: {e}"))?;
    Ok(())
}

/// Send `ClientMessage::SetFrameSubscription { enabled }` (#65, proto 18). The
/// variant is index 7 (the last in `ClientMessage`); the bool is one byte.
#[allow(dead_code)]
pub fn send_set_frame_subscription(stream: &mut UnixStream, enabled: bool) -> Result<(), String> {
    let mut buf = encode_varint_u32(7);
    buf.push(u8::from(enabled));
    let framed = frame_message(&buf);
    stream
        .write_all(&framed)
        .map_err(|e| format!("write set-frame-subscription: {e}"))?;
    stream
        .flush()
        .map_err(|e| format!("flush set-frame-subscription: {e}"))?;
    Ok(())
}

pub fn drain_messages(stream: &mut UnixStream) {
    stream.set_read_timeout(Some(DEFAULT_READ_SLICE)).unwrap();
    while read_server_message(stream).is_ok() {}
    stream.set_read_timeout(None).unwrap();
}

pub fn wait_until<F>(timeout: Duration, interval: Duration, mut predicate: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        thread::sleep(interval);
    }
    predicate()
}

pub fn wait_for_message_variant(
    stream: &mut UnixStream,
    timeout: Duration,
    variant: u32,
) -> Result<bool, String> {
    stream
        .set_read_timeout(Some(DEFAULT_READ_SLICE))
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match read_server_message(stream) {
            Ok((got, _)) if got == variant => return Ok(true),
            Ok(_) => continue,
            Err(_) => continue,
        }
    }
    Ok(false)
}

/// `ServerMessage::Notify` = variant 5.
pub const VARIANT_NOTIFY: u32 = 5;

/// Wait for a `ServerMessage::Notify` whose `message` is `expected`.
///
/// Narrower than [`wait_for_message_variant`] on purpose, and not just for
/// tidiness. Every state change on an agent pane raises a Notify, so matching
/// on the variant alone cannot tell the notification a test is about from the
/// ones it caused on the way there — `client_receives_notify_on_agent_state_change`
/// reports Blocked, then Working, then Idle, and all three can notify. Matching
/// the message is what makes the assertion mean what it says.
///
/// The `message` is the sound label the server sends (`"agent done"`,
/// `"Request"`, …); `src/client/mod.rs` maps it back to a `sound::Sound`, so it
/// is a real part of the wire rather than a display string.
pub fn wait_for_notify_message(
    stream: &mut UnixStream,
    timeout: Duration,
    expected: &str,
) -> Result<bool, String> {
    stream
        .set_read_timeout(Some(DEFAULT_READ_SLICE))
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match read_server_message(stream) {
            Ok((VARIANT_NOTIFY, payload)) => {
                if notify_message(&payload).as_deref() == Some(expected) {
                    return Ok(true);
                }
            }
            Ok(_) => continue,
            Err(_) => continue,
        }
    }
    Ok(false)
}

/// The `message` field of a `Notify` payload: `kind`, then the string.
///
/// Hand-decoded for the same reason [`decode_welcome`] is — it is the same
/// varint wire, and a partial decode here only means "not the one we want".
fn notify_message(payload: &[u8]) -> Option<String> {
    let (_, kind_len) = decode_varint_u32(payload, 0).ok()?; // NotifyKind
    let (len, len_bytes) = decode_varint_u32(payload, kind_len).ok()?;
    let start = kind_len + len_bytes;
    let end = start.checked_add(len as usize)?;
    let text = payload.get(start..end)?;
    std::str::from_utf8(text).ok().map(str::to_string)
}

pub fn wait_for_disconnect(stream: &mut UnixStream, timeout: Duration) -> Result<bool, String> {
    stream.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    let mut idle_since = None;
    let result = loop {
        match read_framed_message_nonblocking(stream) {
            Ok(_) => idle_since = None,
            Err(err)
                if err.to_ascii_lowercase().contains("would block")
                    || err.contains("Resource temporarily unavailable") =>
            {
                let idle_started = *idle_since.get_or_insert_with(Instant::now);
                if idle_started.elapsed() >= Duration::from_millis(200) {
                    break Ok(true);
                }
            }
            Err(_) => break Ok(true),
        }
        if Instant::now() >= deadline {
            break Ok(false);
        }
        thread::sleep(Duration::from_millis(25));
    };
    let _ = stream.set_nonblocking(false);
    result
}

pub fn cleanup_registered_flock_pids() {
    let pids: Vec<u32> = {
        let mut registry = pid_registry_lock();
        registry.drain().collect()
    };

    for pid in pids {
        terminate_pid(pid);
    }

    let runtime_dirs: HashSet<PathBuf> = {
        let mut runtime_dirs = runtime_dir_registry_lock();
        runtime_dirs.drain().collect()
    };

    terminate_servers_for_runtime_dirs(&runtime_dirs);
    let _ = cleanup_servers_with_missing_runtime_dir();
}

fn ensure_cleanup_hooks() {
    INIT.call_once(|| {
        let _ = cleanup_servers_with_missing_runtime_dir();
        start_global_watchdog();

        let _ = CLEANUP_GUARD.set(CleanupGuard);

        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            cleanup_registered_flock_pids();
            previous_hook(panic_info);
        }));

        let _ = ctrlc::set_handler(|| {
            cleanup_registered_flock_pids();
            std::process::exit(130);
        });

        unsafe {
            libc::atexit(run_atexit_cleanup);
        }
    });
}

fn pid_registry_lock() -> std::sync::MutexGuard<'static, HashSet<u32>> {
    PID_REGISTRY
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn runtime_dir_registry_lock() -> std::sync::MutexGuard<'static, HashSet<PathBuf>> {
    RUNTIME_DIR_REGISTRY
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn registered_runtime_dirs_snapshot() -> HashSet<PathBuf> {
    if let Some(runtime_dirs) = RUNTIME_DIR_REGISTRY.get() {
        runtime_dirs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    } else {
        HashSet::new()
    }
}

fn should_terminate_runtime_dir(
    runtime_dir: &Path,
    registered_runtime_dirs: &HashSet<PathBuf>,
) -> bool {
    if !registered_runtime_dirs.contains(runtime_dir) {
        return false;
    }

    if !runtime_dir.exists() {
        return true;
    }

    !runtime_dir_owner_alive(runtime_dir)
}

#[expect(
    clippy::print_stderr,
    reason = "test watchdog runs in a background thread with no tracing subscriber attached — surface cleanup failures on stderr so they show up in cargo test output"
)]
fn start_global_watchdog() {
    thread::spawn(|| loop {
        thread::sleep(WATCHDOG_SCAN_INTERVAL);

        if let Err(err) = cleanup_servers_with_missing_runtime_dir() {
            eprintln!("flock test cleanup watchdog error: {err}");
        }
    });
}

fn cleanup_servers_with_missing_runtime_dir() -> std::io::Result<()> {
    let registered_runtime_dirs = registered_runtime_dirs_snapshot();
    if registered_runtime_dirs.is_empty() {
        return Ok(());
    }

    for (pid, _) in iter_worktree_server_pids()? {
        // A pid we cannot inspect (it exited, or the kernel will not tell us
        // about it) is skipped rather than propagated: one unreadable process
        // must not abort the sweep of the other six hundred.
        let Some(runtime_dir) = runtime_dir_for(pid) else {
            continue;
        };

        if should_terminate_runtime_dir(&runtime_dir, &registered_runtime_dirs) {
            terminate_pid(pid);
        }
    }

    Ok(())
}

fn terminate_servers_for_runtime_dirs(runtime_dirs: &HashSet<PathBuf>) {
    if runtime_dirs.is_empty() {
        return;
    }

    let Ok(pids) = iter_worktree_server_pids() else {
        return;
    };

    for (pid, _) in pids {
        let Some(runtime_dir) = runtime_dir_for(pid) else {
            continue;
        };

        if runtime_dirs.contains(&runtime_dir) {
            terminate_pid(pid);
        }
    }
}

/// Test-spawned flock servers visible on this machine.
///
/// CROSS-PLATFORM since #521. It used to walk `/proc`, which does not exist on
/// Darwin: `read_dir("/proc")` is `NotFound`, the function returned an empty
/// list, and `terminate_servers_for_runtime_dirs` swept nothing while
/// reporting success. Any server the harness did not explicitly registered
/// therefore leaked, and a leaked process fails no test — four detached daemons
/// per `just check` run on a Mac, invisible to nextest because they hold no
/// output pipe. The pid registry cannot catch those: the daemon is spawned
/// inside the product, so the harness never learns its pid.
///
/// The scan itself now lives in the `process_table` module, which reads `/proc`
/// on Linux and the Darwin equivalents elsewhere. What identifies a process as ours has
/// not changed and is the reason this is safe to run on a developer's whole
/// machine: `is_test_flock_binary` requires the executable to be THIS
/// checkout's `target/debug/flk`, so a daemon built by another session from
/// its own worktree — or an installed `flock` — is never in the returned set.
/// The `server` argv check narrows it further to servers.
///
/// The `ProcessInfo` is returned alongside each pid rather than discarded, so
/// the caller can reuse it. Re-reading it was the other half of the cost
/// problem: on Linux a candidate's exe/cmdline/environ were read twice.
fn iter_worktree_server_pids() -> std::io::Result<Vec<(u32, process_table::ProcessInfo)>> {
    let own_pid = std::process::id();
    let mut pids = Vec::new();

    for pid in process_table::list_process_ids()? {
        if pid == own_pid {
            continue;
        }

        let Ok(info) = process_table::process_info(pid) else {
            // Exited, or unreadable (ENOENT/EIO/EPERM). Either way this pid is
            // not a server we should kill, and the rest of the sweep continues.
            continue;
        };

        if is_test_flock_server_process(&info) {
            pids.push((pid, info));
        }
    }

    Ok(pids)
}

fn is_test_flock_server_process(info: &process_table::ProcessInfo) -> bool {
    let Some(exe_path) = info.exe_path.as_deref() else {
        return false;
    };

    is_test_flock_binary(exe_path) && info.argv.iter().any(|arg| arg == "server")
}

/// The runtime dir a `flk server` is serving, from its own environment.
///
/// `XDG_RUNTIME_DIR` is authoritative; `FLOCK_SOCKET_PATH`'s parent is the
/// fallback for a server whose runtime dir was unset. Both are inherited from
/// the client that spawned the daemon, which is what lets the sweep connect a
/// process it never spawned back to the test that owns it.
///
/// Returns `None` for every way this can fail — an unreadable environment, an
/// unset variable, a socket path with no parent — because every caller treats
/// `None` as "not mine, skip it". A sweep must not abandon the remaining
/// processes over one that cannot be read.
fn runtime_dir_for(pid: u32) -> Option<PathBuf> {
    let environment = process_table::process_environment(pid).ok()?;

    let value = |key: &str| {
        environment.iter().find_map(|entry| {
            entry
                .split_once('=')
                .filter(|(name, _)| *name == key)
                .map(|(_, value)| value.to_string())
        })
    };

    if let Some(runtime_dir) = value("XDG_RUNTIME_DIR") {
        return Some(PathBuf::from(runtime_dir));
    }

    value("FLOCK_SOCKET_PATH")
        .map(PathBuf::from)
        .and_then(|path| path.parent().map(Path::to_path_buf))
}

fn runtime_dir_owner_alive(runtime_dir: &Path) -> bool {
    let marker = runtime_dir.join(RUNTIME_OWNER_MARKER);
    let Ok(contents) = fs::read_to_string(marker) else {
        return false;
    };

    let Ok(owner_pid) = contents.trim().parse::<libc::pid_t>() else {
        return false;
    };

    process_exists(owner_pid)
}

fn current_checkout_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// The one executable this sweep is ever allowed to kill: this checkout's own
/// debug build of `flk`.
///
/// Equality against a canonicalised path, not a component-prefix test. The
/// earlier `path.ends_with("target/debug/flk") && path.starts_with(root)` admits
/// `<root>/.worktrees/foo/target/debug/flk` — a nested worktree's binary, built
/// by whoever made that worktree, reaped by this checkout's tests. AGENTS.md
/// prescribes SIBLING worktrees (`../flock-worktrees/<slug>`), where that cannot
/// arise, but "cannot arise under the documented layout" is a weaker guarantee
/// than "is false", and this filter is what stands between the sweep and a
/// developer's other sessions.
///
/// Canonicalising both sides is also what makes the symlink case work. On Darwin
/// the kernel reports the exec path as it was invoked, so a checkout reached
/// through a symlink would match nothing — the product passes
/// `std::env::current_exe()` (`src/server/autodetect.rs`), which std resolves
/// with `realpath`, while `CARGO_MANIFEST_DIR` is however cargo was invoked.
/// Comparing resolved paths removes the disagreement instead of leaving it to
/// whichever side happens to be canonical.
///
/// Resolved lazily and cached: `canonicalize` is a syscall per component, and
/// this runs for every pid on the machine on a 1 Hz watchdog. `OnceLock` because
/// the answer cannot change within a process.
fn is_test_flock_binary(path: &Path) -> bool {
    static EXPECTED: OnceLock<Option<PathBuf>> = OnceLock::new();

    let expected = EXPECTED
        .get_or_init(|| fs::canonicalize(current_checkout_root().join("target/debug/flk")).ok())
        .as_deref();

    match expected {
        // No canonicalised binary to compare against (not built yet, or the
        // checkout moved): match nothing. A sweep that cannot prove a process is
        // ours must not kill it.
        None => false,
        Some(expected) => fs::canonicalize(path).ok().as_deref() == Some(expected),
    }
}

extern "C" fn run_atexit_cleanup() {
    cleanup_registered_flock_pids();
}

struct CleanupGuard;

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        cleanup_registered_flock_pids();
    }
}

fn terminate_pid(pid: u32) {
    let pid_t = pid as libc::pid_t;

    if process_exists(pid_t) {
        unsafe {
            libc::kill(pid_t, libc::SIGTERM);
        }
    }

    if wait_for_pid_exit(pid_t, Duration::from_millis(400)) {
        return;
    }

    if process_exists(pid_t) {
        unsafe {
            libc::kill(pid_t, libc::SIGKILL);
        }
    }

    let _ = wait_for_pid_exit(pid_t, Duration::from_secs(2));
}

fn wait_for_pid_exit(pid: libc::pid_t, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        if !process_exists(pid) {
            return true;
        }

        let mut status = 0;
        let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if result == pid {
            return true;
        }

        if result == -1 {
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::ECHILD) => {
                    // Not our child (or already reaped elsewhere). Poll /proc existence
                    // until the process is truly gone.
                    if !process_exists(pid) {
                        return true;
                    }
                }
                Some(libc::ESRCH) => return true,
                _ => {
                    if !process_exists(pid) {
                        return true;
                    }
                }
            }
        }

        thread::sleep(Duration::from_millis(20));
    }

    !process_exists(pid)
}

fn process_exists(pid: libc::pid_t) -> bool {
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        true
    } else {
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_missing_runtime_dir(label: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "flock-watchdog-scoping-{label}-{}-{unique}",
            std::process::id()
        ))
    }

    #[test]
    fn watchdog_scoping_does_not_terminate_missing_unregistered_runtime_dir() {
        let runtime_dir = unique_missing_runtime_dir("unregistered");
        let registered_runtime_dirs = HashSet::new();

        assert!(
            !should_terminate_runtime_dir(&runtime_dir, &registered_runtime_dirs),
            "missing runtime dirs must not be killable until they are proven session-owned"
        );
    }

    #[test]
    fn watchdog_scoping_terminates_missing_registered_runtime_dir() {
        let runtime_dir = unique_missing_runtime_dir("registered");
        let mut registered_runtime_dirs = HashSet::new();
        registered_runtime_dirs.insert(runtime_dir.clone());

        assert!(
            should_terminate_runtime_dir(&runtime_dir, &registered_runtime_dirs),
            "missing runtime dirs that are session-owned should be considered killable"
        );
    }

    #[test]
    fn test_binary_matcher_accepts_current_checkout_debug_binary() {
        let binary = current_checkout_root().join("target/debug/flk");
        assert!(
            is_test_flock_binary(&binary),
            "current checkout debug binary should be considered test-owned"
        );
    }

    #[test]
    fn test_binary_matcher_rejects_installed_binary() {
        assert!(
            !is_test_flock_binary(Path::new("/home/can/.local/bin/flock")),
            "installed binaries must not be considered test-owned"
        );
    }

    /// The shape that a component-prefix test admits and this one must not: a
    /// nested worktree's binary, `<root>/.worktrees/foo/target/debug/flk`. It is
    /// under the checkout and it ends in `target/debug/flk`, and it belongs to
    /// someone else.
    #[test]
    fn test_binary_matcher_rejects_a_nested_worktree_binary() {
        let nested = current_checkout_root()
            .join(".worktrees")
            .join("someone-elses")
            .join("target/debug/flk");

        assert!(
            nested.starts_with(current_checkout_root()),
            "the fixture must actually be the shape this test claims to reject"
        );
        assert!(
            nested.ends_with("target/debug/flk"),
            "the fixture must actually be the shape this test claims to reject"
        );
        assert!(
            !is_test_flock_binary(&nested),
            "another session's nested worktree binary must never be reaped by this \
             checkout's tests"
        );
    }

    /// A sibling worktree — the layout AGENTS.md prescribes — is a different
    /// path and stays out. Stated because it is the case that was measured
    /// against 172 real stray daemons, and it is worth a test rather than a
    /// comment given what is on the other end of it.
    #[test]
    fn test_binary_matcher_rejects_a_sibling_worktree_binary() {
        let sibling = current_checkout_root()
            .parent()
            .unwrap_or_else(|| Path::new("/"))
            .join("flock-worktrees")
            .join("issue-999")
            .join("target/debug/flk");

        assert!(
            !is_test_flock_binary(&sibling),
            "a sibling worktree's binary belongs to another session"
        );
    }

    /// A symlink INTO this checkout's own binary resolves to it and must still
    /// be accepted: the point of canonicalising is to compare resolved paths, so
    /// an equivalent spelling of the same file is the same answer, not a
    /// different one.
    #[test]
    fn test_binary_matcher_accepts_a_symlink_to_the_checkout_binary() {
        let real = current_checkout_root().join("target/debug/flk");
        if !real.exists() {
            // Not built in this test's environment; the accept-case above
            // already covers the direct path.
            return;
        }

        let link_dir = std::env::temp_dir().join(format!(
            "flock-binary-matcher-link-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&link_dir).expect("create link dir");
        let link = link_dir.join("flk");
        let linked = symlink(&real, &link);

        if linked.is_ok() {
            assert!(
                is_test_flock_binary(&link),
                "a symlink resolving to this checkout's binary is that binary"
            );
        }

        let _ = fs::remove_dir_all(&link_dir);
    }

    fn symlink(original: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(original, link)
    }
}
