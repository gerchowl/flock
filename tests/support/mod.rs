#![allow(dead_code)]

use std::collections::HashSet;
use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

pub mod fleet;

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

#[cfg(target_os = "linux")]
pub fn flock_server_pids_for_runtime_dir(runtime_dir: &Path) -> std::io::Result<Vec<u32>> {
    let mut pids = Vec::new();
    for pid in iter_worktree_server_pids()? {
        let Some(process_runtime_dir) = process_runtime_dir(pid)? else {
            continue;
        };
        if process_runtime_dir == runtime_dir {
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
/// `terminate_servers_for_runtime_dirs` would be the backstop, but it walks
/// `/proc`, so on macOS it enumerates nothing and reports success.
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
pub const PROTOCOL_VERSION: u32 = 25;

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

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).map_err(|e| e.to_string())?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 2 * 1024 * 1024 {
        return Err(format!("oversized response: {len}"));
    }

    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).map_err(|e| e.to_string())?;
    decode_welcome(&payload)
}

/// How long a frame that has already *started* arriving is given to finish.
///
/// The server writes each message with a single `write_all`, so a frame either
/// arrives whole or the peer is gone. This bound only exists so a wedged peer
/// cannot hang a test forever; reaching it means the stream is already lost.
const FRAME_COMPLETION_GRACE: Duration = Duration::from_secs(30);

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
/// So the policy is deliberately asymmetric:
///
/// * **Before** the length prefix, a timeout is an ordinary "nothing yet" and
///   the caller may retry — no bytes were consumed, so framing is intact.
/// * **After** it, the frame is finished or the reader reports the stream
///   lost. A partially-arrived frame is never abandoned halfway.
///
/// The socket's read timeout — which the caller owns and sets, as it always
/// has — still governs the first byte, so "is the peer there?" keeps its
/// existing behaviour and every existing wait loop works unchanged.
///
/// Note that a read timeout surfaces as `TimedOut` on Linux but as
/// `WouldBlock` on macOS, so both kinds are treated as "not yet" here; a
/// caller that genuinely cannot wait asks for
/// [`read_framed_message_nonblocking`], which is a different question rather
/// than a guess about which platform it is on.
pub fn read_framed_message(stream: &mut UnixStream) -> Result<(u32, Vec<u8>), String> {
    read_framed_message_with_policy(stream, true)
}

/// [`read_framed_message`] for a socket the caller has put in nonblocking mode
/// to poll it, which therefore cannot be made to wait out a frame it has
/// already started. Only `wait_for_disconnect` needs this.
pub fn read_framed_message_nonblocking(stream: &mut UnixStream) -> Result<(u32, Vec<u8>), String> {
    read_framed_message_with_policy(stream, false)
}

fn read_framed_message_with_policy(
    stream: &mut UnixStream,
    wait_out_partial: bool,
) -> Result<(u32, Vec<u8>), String> {
    read_framed_io(stream, wait_out_partial).map_err(|e| e.to_string())
}

/// [`read_framed_message`], keeping the `io::ErrorKind` intact so a caller that
/// tells "nothing yet" apart from "the stream is gone" — `multi_client.rs`
/// does, through its `is_timeout` — still can.
pub fn read_framed_io(
    stream: &mut UnixStream,
    wait_out_partial: bool,
) -> io::Result<(u32, Vec<u8>)> {
    let can_wait = |filled: usize| wait_out_partial && filled > 0;

    // The prefix is four bytes and nothing has been consumed yet, so a timeout
    // here is safe to report as "nothing yet" — but a timeout *partway* through
    // it has already eaten bytes, so those are retained and the wait continues.
    let mut len_buf = [0u8; 4];
    let mut prefix_filled = 0;
    let prefix_grace = Instant::now() + FRAME_COMPLETION_GRACE;
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
                if can_wait(prefix_filled) && Instant::now() < prefix_grace {
                    continue;
                }
                return Err(prefix_error(&e));
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
    let grace = Instant::now() + FRAME_COMPLETION_GRACE;
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
                if wait_out_partial && Instant::now() < grace {
                    continue;
                }
                return Err(payload_error(&e));
            }
            Err(e) => return Err(payload_error(&e)),
        }
    }

    let (variant, consumed) =
        decode_varint_u32(&payload, 0).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    Ok((variant, payload[consumed..].to_vec()))
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

    for pid in iter_worktree_server_pids()? {
        let Some(runtime_dir) = process_runtime_dir(pid)? else {
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

    for pid in pids {
        let Ok(runtime_dir) = process_runtime_dir(pid) else {
            continue;
        };

        let Some(runtime_dir) = runtime_dir else {
            continue;
        };

        if runtime_dirs.contains(&runtime_dir) {
            terminate_pid(pid);
        }
    }
}

/// Test-spawned flock servers visible on this machine.
///
/// LINUX ONLY, by construction: this walks `/proc`, and every caller's
/// behaviour degrades to a no-op elsewhere. That is deliberate but was silent —
/// on macOS `read_dir("/proc")` is `NotFound`, so this returned an empty list
/// and `terminate_servers_for_runtime_dirs` swept nothing, successfully. Any
/// server the harness did not explicitly register therefore leaked, and a
/// leaked process fails no test.
///
/// So the REGISTRY is the cross-platform guarantee, not this sweep: anything
/// that must be reaped has to go through `register_spawned_flock_pid`, which
/// is wired to the atexit / panic / ctrl-c hooks. This stays as a Linux-only
/// belt-and-braces pass for servers whose runtime dir outlived their owner.
fn iter_worktree_server_pids() -> std::io::Result<Vec<u32>> {
    let own_pid = std::process::id();
    let mut pids = Vec::new();

    let proc_entries = match fs::read_dir("/proc") {
        Ok(entries) => entries,
        // No /proc (macOS): nothing to enumerate. See the doc comment — the
        // registry, not this sweep, is what guarantees cleanup here.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };

    for entry in proc_entries {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(pid) = file_name.to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };

        if pid == own_pid {
            continue;
        }

        if is_test_flock_server_process(pid) {
            pids.push(pid);
        }
    }

    Ok(pids)
}

fn is_test_flock_server_process(pid: u32) -> bool {
    let Some(exe_path) = proc_link_target(pid, "exe") else {
        return false;
    };

    if !is_test_flock_binary(&exe_path) {
        return false;
    }

    let Ok(cmdline) = read_cmdline(pid) else {
        return false;
    };

    cmdline.iter().any(|arg| arg == "server")
}

fn proc_link_target(pid: u32, link: &str) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/{link}")).ok()
}

fn read_cmdline(pid: u32) -> std::io::Result<Vec<String>> {
    let cmdline = fs::read(format!("/proc/{pid}/cmdline"))?;
    Ok(cmdline
        .split(|byte| *byte == 0)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| String::from_utf8_lossy(chunk).to_string())
        .collect())
}

fn process_runtime_dir(pid: u32) -> std::io::Result<Option<PathBuf>> {
    let environ = fs::read(format!("/proc/{pid}/environ"))?;

    let mut socket_path: Option<PathBuf> = None;

    for entry in environ.split(|byte| *byte == 0) {
        if entry.is_empty() {
            continue;
        }

        let kv = String::from_utf8_lossy(entry);
        if let Some(value) = kv.strip_prefix("XDG_RUNTIME_DIR=") {
            return Ok(Some(PathBuf::from(value)));
        }

        if let Some(value) = kv.strip_prefix("FLOCK_SOCKET_PATH=") {
            socket_path = Some(PathBuf::from(value));
        }
    }

    Ok(socket_path.and_then(|path| path.parent().map(Path::to_path_buf)))
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

fn is_test_flock_binary(path: &Path) -> bool {
    path.ends_with("target/debug/flk") && path.starts_with(current_checkout_root())
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
}
