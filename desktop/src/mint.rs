//! The zero-touch token (ADR-0038 §6, "The server on the same machine"): this
//! process mints an app-session token over `flock.sock` with
//! `client.app.mint`, holding the connection itself, because the token binds
//! to the minting process and a child's exit would revoke it. The token stays
//! in this process (`bridge.rs`) and is never logged; every refusal becomes a
//! plain message saying why, to which `bridge::rejection` adds the remote path.

use std::{
    fmt, io,
    path::{Path, PathBuf},
    time::Duration,
};

/// The remote path every refusal offers instead (#2293).
pub const REMOTE: &str = "Connect to it with a one-time code instead (the remote path, #2293).";

/// How long one open may take, all of it: the mint, its one retry and the
/// `/v1/ws` upgrade. ADR-0038 §3's connect deadline, 10 s. `bridge::Bridge::open`
/// is the one place it starts.
pub const DEADLINE: Duration = Duration::from_secs(10);

/// Why an open ran out of [`DEADLINE`] before the server answered the mint.
pub const NO_ANSWER: &str = "The flk server did not answer within 10 s.";

/// An app-session token: a bearer secret with operator authority while the app
/// runs. It has no `Display`, and its `Debug` shows nothing of it.
pub struct Token(String);

impl Token {
    /// The secret, for the bridge's `hello` and nothing that leaves this process.
    pub fn secret(&self) -> &str {
        &self.0
    }
}

/// A mint's token and the remaining lifetime the server reported at issuance
/// (`issued.info.expires_in_ms`), when it reported one.
#[derive(Debug)]
pub struct Minted {
    pub token: Token,
    pub expires_in_ms: Option<u64>,
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// Why one mint request failed.
#[derive(Debug)]
pub enum Failure {
    /// No socket, or nobody listening on it.
    NoServer,
    /// The server did not answer within what was left of [`DEADLINE`].
    Timeout,
    Io(String),
    Refused {
        code: String,
        message: String,
    },
}

/// `flock.sock` where `flk` (release) puts it: `src/session.rs`
/// `active_api_socket_path` over `src/config/io.rs` `config_dir`, which falls
/// back to `/tmp/flock` with neither `XDG_CONFIG_HOME` nor `HOME`.
pub fn socket_path(var: impl Fn(&str) -> Option<String>) -> PathBuf {
    if let Some(path) = var("FLOCK_SOCKET_PATH") {
        return path.into();
    }
    let dir = match (var("XDG_CONFIG_HOME"), var("HOME")) {
        (Some(config), _) => PathBuf::from(config).join("flock"),
        (None, Some(home)) => PathBuf::from(home).join(".config/flock"),
        (None, None) => PathBuf::from("/tmp/flock"),
    };
    // flk ignores the default name and an invalid one (`session::active_name`
    // over `validate_name`: at most 64 bytes of `[A-Za-z0-9._-]`, not `.` or `..`).
    let named = var("FLOCK_SESSION").filter(|name| {
        !matches!(name.as_str(), "" | "default" | "." | "..")
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
    });
    match named {
        Some(name) => dir.join("sessions").join(name),
        None => dir,
    }
    .join("flock.sock")
}

/// The server's unix-socket `/v1/ws` beside `api`: `flock.sock` gives
/// `flock-ws.sock`, an override `custom.sock` gives `custom-ws.sock`
/// (`src/web/local_listener.rs` `socket_path`).
pub fn ws_socket_path(api: &Path) -> PathBuf {
    let mut stem = api.file_stem().unwrap_or("flock".as_ref()).to_os_string();
    stem.push("-ws.sock");
    api.with_file_name(stem)
}

/// Linux only: a same-UID process may attach to the shell and read the token
/// unless Yama's `ptrace_scope` is at least 1 (ADR-0038 §6), so mint only then.
pub fn ptrace_guard(read: impl FnOnce() -> io::Result<String>) -> Result<(), String> {
    match read().map(|text| text.trim().parse::<u8>()) {
        Ok(Ok(scope)) if scope >= 1 => Ok(()),
        found => Err(format!(
            "Not minting a token: /proc/sys/kernel/yama/ptrace_scope {}, so any process of \
             yours could read it from this app. Set it to 1 or more.",
            match found {
                Ok(Ok(_)) => "is 0".to_owned(),
                Ok(Err(_)) => "is not a number".to_owned(),
                Err(error) => format!("is unreadable ({error})"),
            }
        )),
    }
}

/// Why the server refused (#2493's `client.app.mint` refusals), in plain words.
pub fn refusal_message(code: &str, message: &str) -> String {
    match code {
        "operator_only" => "The flk server refused the mint: this app was started from inside \
            a flk pane, or not from your own session. Open it from the Dock, Finder or your \
            launcher."
            .to_owned(),
        "peer_process_changed" => "The flk server could not bind the token to this app's process \
            (it changed while minting, twice). Restart the app."
            .to_owned(),
        "session_busy" => "The flk server holds too many app sessions or is busy. Close another \
            flock window, or restart the app later."
            .to_owned(),
        "invalid_params" => format!(
            "The flk server refused the mint request ({message}). The app \
            and the server may be different versions."
        ),
        _ => format!("The flk server could not mint a token ({code}: {message})."),
    }
}

/// Mints through `request`, retrying `peer_process_changed` once on a fresh
/// connection and starting a server through `start` when none runs. Each
/// request gets what `left` says remains of the open's one budget, and none
/// runs once it is spent.
pub fn mint_with(
    mut request: impl FnMut(Duration) -> Result<Minted, Failure>,
    start: impl FnOnce() -> Result<(), String>,
    left: impl Fn() -> Duration,
) -> Result<Minted, String> {
    let mut ask = || match left() {
        spent if spent.is_zero() => Err(Failure::Timeout),
        left => request(left),
    };
    let mut reply = ask();
    if matches!(reply, Err(Failure::NoServer)) {
        start().map_err(|reason| {
            format!("No flk server is running, and starting one failed: {reason}")
        })?;
        reply = ask();
    }
    if matches!(&reply, Err(Failure::Refused { code, .. }) if code == "peer_process_changed") {
        reply = ask();
    }
    reply.map_err(|failure| match failure {
        Failure::NoServer => "No flk server is running, and the one started did not answer.".into(),
        Failure::Timeout => NO_ANSWER.into(),
        Failure::Io(reason) => format!("Talking to the flk server failed: {reason}"),
        Failure::Refused { code, message } => refusal_message(&code, &message),
    })
}

/// The token and its `expires_in_ms` in a `client.app.mint` reply line, or its
/// refusal. No error text quotes the line, which may hold the token.
pub fn parse_reply(line: &str) -> Result<Minted, Failure> {
    let reply: serde_json::Value =
        serde_json::from_str(line).map_err(|_| Failure::Io("the reply is not JSON".into()))?;
    if let Some(error) = reply.get("error") {
        let text = |key| error[key].as_str().unwrap_or_default().to_owned();
        return Err(Failure::Refused {
            code: text("code"),
            message: text("message"),
        });
    }
    match (
        &reply["result"]["type"],
        reply["result"]["issued"]["token"].as_str(),
    ) {
        (serde_json::Value::String(kind), Some(token))
            if kind == "client_token_issued" && !token.is_empty() =>
        {
            Ok(Minted {
                token: Token(token.to_owned()),
                expires_in_ms: reply["result"]["issued"]["info"]["expires_in_ms"].as_u64(),
            })
        }
        _ => Err(Failure::Io("the reply carries no issued token".into())),
    }
}

#[cfg(unix)]
pub use unix::mint;

#[cfg(unix)]
mod unix {
    use std::{
        io::{self, BufRead, BufReader, Write},
        os::unix::{net::UnixStream, process::CommandExt},
        path::Path,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };

    use super::{Failure, Minted};

    /// `src/server/autodetect.rs`: how long `flk` waits for its daemon, and how often it looks.
    const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(5);
    const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(50);

    /// One mint from this process, answered by `until` (the open's
    /// budget), starting a server when none runs.
    pub fn mint(until: Instant) -> Result<Minted, String> {
        if cfg!(target_os = "linux") {
            super::ptrace_guard(|| std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope"))?;
        }
        let path = super::socket_path(|key| std::env::var(key).ok());
        let left = || until.saturating_duration_since(Instant::now());
        super::mint_with(|left| request(&path, left), || start_server(&path), left)
    }

    /// A fresh connection, one `client.app.mint` line and its reply line,
    /// each way within `deadline`.
    pub(super) fn request(path: &Path, deadline: Duration) -> Result<Minted, Failure> {
        let stream = UnixStream::connect(path).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => Failure::NoServer,
            _ => Failure::Io(format!("{}: {error}", path.display())),
        })?;
        let io = |error: io::Error| match error.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Failure::Timeout,
            _ => Failure::Io(error.to_string()),
        };
        stream.set_read_timeout(Some(deadline)).map_err(io)?;
        stream.set_write_timeout(Some(deadline)).map_err(io)?;
        (&stream)
            .write_all(b"{\"id\":\"mint\",\"method\":\"client.app.mint\",\"params\":{}}\n")
            .map_err(io)?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).map_err(io)?;
        super::parse_reply(&line)
    }

    /// `flk server` as `spawn_server_daemon` runs it: its own process group,
    /// no stdio, this environment. `FLOCK_DESKTOP_FLK` names the binary, else `flk` on PATH.
    fn start_server(path: &Path) -> Result<(), String> {
        let flk = std::env::var_os("FLOCK_DESKTOP_FLK").unwrap_or_else(|| "flk".into());
        let mut child = Command::new(&flk)
            .arg("server")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("{}: {error}", Path::new(&flk).display()))?;
        let deadline = Instant::now() + SERVER_READY_TIMEOUT;
        while Instant::now() < deadline {
            if UnixStream::connect(path).is_ok() {
                return Ok(());
            }
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!("`flk server` exited ({status}); its log says why"));
            }
            std::thread::sleep(SOCKET_POLL_INTERVAL);
        }
        Err(format!(
            "`flk server` did not listen on {} within 5 s",
            path.display()
        ))
    }
}

#[cfg(not(unix))]
pub fn mint(_until: std::time::Instant) -> Result<Minted, String> {
    Err("This platform has no flock.sock.".into())
}

#[cfg(test)]
mod tests;
