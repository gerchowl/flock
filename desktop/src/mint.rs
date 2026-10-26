//! The zero-touch token (ADR-0038 §6, "The server on the same machine"): this
//! process mints an app-session token over `flock.sock` with
//! `client.app.mint`, holding the connection itself, because the token binds
//! to the minting process and a child's exit would revoke it. The token stays
//! in memory and is never logged; every refusal becomes a plain message.

use std::{fmt, io, path::PathBuf};

/// The remote path the refusals offer instead (#2293).
const REMOTE: &str = "Connect to it with a one-time code instead (the remote path, #2293).";

/// An app-session token: a bearer secret with operator authority while the app
/// runs. It has no `Display`, and its `Debug` shows nothing of it.
pub struct Token(String);

impl Token {
    /// The secret, for the one Tauri command that hands it to the webview.
    pub fn into_secret(self) -> String {
        self.0
    }
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
    Io(String),
    Refused {
        code: String,
        message: String,
    },
}

/// `flock.sock` where `flk` (release) puts it: `src/session.rs`
/// `active_api_socket_path` over `src/config/io.rs` `config_dir`.
pub fn socket_path(var: impl Fn(&str) -> Option<String>) -> Result<PathBuf, String> {
    if let Some(path) = var("FLOCK_SOCKET_PATH") {
        return Ok(path.into());
    }
    let dir = match (var("XDG_CONFIG_HOME"), var("HOME")) {
        (Some(config), _) => PathBuf::from(config).join("flock"),
        (None, Some(home)) => PathBuf::from(home).join(".config/flock"),
        (None, None) => return Err("neither XDG_CONFIG_HOME nor HOME is set".into()),
    };
    // flk ignores the default name and an invalid one (`session::active_name`).
    let named = var("FLOCK_SESSION").filter(|name| {
        !matches!(name.as_str(), "" | "default" | "." | "..")
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
    });
    Ok(match named {
        Some(name) => dir.join("sessions").join(name),
        None => dir,
    }
    .join("flock.sock"))
}

/// Linux only: a same-UID process may attach to the shell and read the token
/// unless Yama's `ptrace_scope` is at least 1 (ADR-0038 §6), so mint only then.
pub fn ptrace_guard(read: impl FnOnce() -> io::Result<String>) -> Result<(), String> {
    match read().map(|text| text.trim().parse::<u8>()) {
        Ok(Ok(scope)) if scope >= 1 => Ok(()),
        found => Err(format!(
            "Not minting a token: /proc/sys/kernel/yama/ptrace_scope {}, so any process of \
             yours could read it from this app. Set it to 1 or more. {REMOTE}",
            match found {
                Ok(Ok(_)) => "is 0".to_owned(),
                Ok(Err(_)) => "is not a number".to_owned(),
                Err(error) => format!("is unreadable ({error})"),
            }
        )),
    }
}

/// The plain message for a refusal code (#2493's `client.app.mint` refusals).
pub fn refusal_message(code: &str, message: &str) -> String {
    match code {
        "operator_only" => format!(
            "The flk server refused the mint: this app was started from inside a flk pane, or \
             not from your own session. Open it from the Dock, Finder or your launcher. {REMOTE}"
        ),
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
/// connection and starting a server through `start` when none runs.
pub fn mint_with(
    mut request: impl FnMut() -> Result<Token, Failure>,
    start: impl FnOnce() -> Result<(), String>,
) -> Result<Token, String> {
    let mut reply = request();
    if matches!(reply, Err(Failure::NoServer)) {
        start().map_err(|reason| {
            format!("No flk server is running, and starting one failed: {reason}")
        })?;
        reply = request();
    }
    if matches!(&reply, Err(Failure::Refused { code, .. }) if code == "peer_process_changed") {
        reply = request();
    }
    reply.map_err(|failure| match failure {
        Failure::NoServer => "No flk server is running, and the one started did not answer.".into(),
        Failure::Io(reason) => format!("Talking to the flk server failed: {reason}"),
        Failure::Refused { code, message } => refusal_message(&code, &message),
    })
}

/// The token in a `client.app.mint` reply line, or its refusal.
/// No error text quotes the line, which may hold the token.
pub fn parse_reply(line: &str) -> Result<Token, Failure> {
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
            Ok(Token(token.to_owned()))
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

    use super::{Failure, Token};

    /// `src/server/autodetect.rs`: how long `flk` waits for its daemon, and how often it looks.
    const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(5);
    const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(50);

    /// One mint from this process, starting a server when none runs.
    pub fn mint() -> Result<Token, String> {
        if cfg!(target_os = "linux") {
            super::ptrace_guard(|| std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope"))?;
        }
        let path = super::socket_path(|key| std::env::var(key).ok())?;
        super::mint_with(|| request(&path), || start_server(&path))
    }

    /// A fresh connection, one `client.app.mint` line and its reply line.
    pub(super) fn request(path: &Path) -> Result<Token, Failure> {
        let stream = UnixStream::connect(path).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => Failure::NoServer,
            _ => Failure::Io(format!("{}: {error}", path.display())),
        })?;
        let io = |error: io::Error| Failure::Io(error.to_string());
        (&stream)
            .write_all(b"{\"id\":\"mint\",\"method\":\"client.app.mint\",\"params\":{}}\n")
            .map_err(io)?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).map_err(io)?;
        super::parse_reply(&line)
    }

    /// `flk server` as `spawn_server_daemon` runs it: its own process group,
    /// no stdio, this environment. `FLOCK_DESKTOP_FLK` names the binary, else `flk` on PATH.
    // flk's `TracedCommand` funnel lives in flk; this crate never links it (#1601).
    #[allow(clippy::disallowed_methods)]
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
pub fn mint() -> Result<Token, String> {
    Err(format!("This platform has no flock.sock. {REMOTE}"))
}

#[cfg(test)]
mod tests;
