#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stdout/stderr for humans and scripts"
)]
use serde::Serialize;

use crate::api;
use crate::api::client::{ApiClient, ApiClientError};

pub(super) fn run_status_command(args: &[String]) -> std::io::Result<i32> {
    let Some((scope, json)) = parse_status_args(args) else {
        return Ok(2);
    };

    match scope {
        StatusScope::Full => print_full_status(json),
        StatusScope::Server => print_server_status(json),
        StatusScope::Client => {
            print_client_status(json)?;
            Ok(0)
        }
        StatusScope::Help => {
            // stdout, like `flk --help`: the request was honoured, so this is
            // the command's output and can be redirected or paged. The same text
            // printed after a FAILED parse still goes to stderr below.
            print!("{}", status_help_text());
            Ok(0)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusScope {
    Full,
    Server,
    Client,
    Help,
}

fn parse_status_args(args: &[String]) -> Option<(StatusScope, bool)> {
    // #455: a help request is a help request wherever it sits, and this used to
    // be a first-position arm that printed the right help and then failed
    // anyway whenever anything followed it — so `flk status --json --help`
    // answered with correct text and exit 2.
    //
    // The predicate is `cli::help`'s, shared rather than reimplemented: a third
    // copy of this rule is a third copy to drift. `help` stays accepted as a
    // first-position word because `status` has never taken a literal-text
    // argument that could collide with it.
    if super::help::asks_for_help(args) || matches!(args.first().map(String::as_str), Some("help"))
    {
        return Some((StatusScope::Help, false));
    }

    match args.first().map(|arg| arg.as_str()) {
        None => Some((StatusScope::Full, false)),
        Some("--json") if args.len() == 1 => Some((StatusScope::Full, true)),
        Some("server") => {
            parse_status_scope_args(args, StatusScope::Server, "flk status server [--json]")
        }
        Some("client") => {
            parse_status_scope_args(args, StatusScope::Client, "flk status client [--json]")
        }
        Some(_) => {
            print_status_help();
            None
        }
    }
}

fn parse_status_scope_args(
    args: &[String],
    scope: StatusScope,
    usage: &str,
) -> Option<(StatusScope, bool)> {
    match args.get(1).map(|arg| arg.as_str()) {
        None => Some((scope, false)),
        Some("--json") if args.len() == 2 => Some((scope, true)),
        _ => {
            eprintln!("usage: {usage}");
            None
        }
    }
}

/// Live server identity as seen from the client.
///
/// `pub(crate)` because `crate::report` builds its provenance block from the
/// same read rather than issuing its own status call — one seam for "what is
/// the server running", not one per caller (#233).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ServerRuntimeStatus {
    Running {
        version: Option<String>,
        protocol: Option<u32>,
        capabilities: Option<crate::api::schema::ServerCapabilities>,
        session_health: Option<crate::platform::SessionHealth>,
        api_listener: Option<crate::api::schema::ApiListenerHealth>,
    },
    NotRunning,
}

fn print_full_status(json: bool) -> std::io::Result<i32> {
    let server = read_server_runtime_status()?;

    if json {
        print_json(&FullStatusJson {
            client: client_status_json(),
            server: server_status_json(&server),
            update: update_status_json(&server),
        })?;
        return Ok(0);
    }

    println!("client:");
    println!("  version: {}", crate::build_info::version());
    println!(
        "  channel: {}",
        crate::config::Config::load().config.update.channel.as_str()
    );
    println!("  protocol: {}", crate::protocol::PROTOCOL_VERSION);
    println!();
    println!("server:");
    print_server_status_body(&server, "  ");
    println!();
    println!("update:");
    println!("  restart_needed: {}", restart_needed_label(&server));

    Ok(0)
}

fn print_server_status(json: bool) -> std::io::Result<i32> {
    let server = read_server_runtime_status()?;
    if json {
        print_json(&server_status_json(&server))?;
        return Ok(0);
    }
    print_server_status_body(&server, "");
    Ok(0)
}

fn print_client_status(json: bool) -> std::io::Result<()> {
    if json {
        print_json(&client_status_json())?;
        return Ok(());
    }

    println!("version: {}", crate::build_info::version());
    println!(
        "channel: {}",
        crate::config::Config::load().config.update.channel.as_str()
    );
    println!("protocol: {}", crate::protocol::PROTOCOL_VERSION);
    println!("binary: {}", current_exe_label());
    Ok(())
}

fn print_server_status_body(server: &ServerRuntimeStatus, indent: &str) {
    match server {
        ServerRuntimeStatus::Running {
            version,
            protocol,
            api_listener,
            ..
        } => {
            println!("{indent}status: running");
            println!("{indent}version: {}", option_label(version.as_deref()));
            println!("{indent}protocol: {}", protocol_label(*protocol));
            println!("{indent}compatible: {}", compatibility_label(*protocol));
            println!("{indent}socket: {}", api::socket_path().display());
            if let Some(health) = api_listener {
                println!("{indent}api accept errors: {}", health.accept_errors);
                if let Some(err) = &health.last_accept_error {
                    println!("{indent}last api accept error: {err}");
                }
            }
        }
        ServerRuntimeStatus::NotRunning => {
            println!("{indent}status: not running");
            println!("{indent}socket: {}", api::socket_path().display());
        }
    }

    // #426. Printed after the identity block and before anything else, because
    // every line above it is reassuring: version, protocol, socket, all healthy
    // while every pane is unable to resolve a name. The one line that explains
    // why belongs where a reader's eye lands before they conclude all is well.
    if session_broken(server) {
        println!(
            "{indent}session: {}",
            crate::health::session_warning::STATUS_LINE
        );
        // Indented per line, not once for the whole string. The warning is
        // prose that can wrap, and a second line starting at column 0 reads as
        // a new top-level key rather than the tail of this one.
        for line in crate::health::session_warning::BANNER.lines() {
            println!("{indent}warning: {line}");
        }
    }
}

/// #426: whether the running server reports a lost user session.
///
/// `None` — a server older than this field, or no server at all — is not
/// broken. The alternative reading, "unknown", would warn on every upgraded
/// client talking to an un-upgraded server, which is how a warning stops being
/// read.
fn session_broken(server: &ServerRuntimeStatus) -> bool {
    matches!(
        server,
        ServerRuntimeStatus::Running {
            session_health: Some(crate::platform::SessionHealth::Broken),
            ..
        }
    )
}

pub(crate) fn read_server_runtime_status() -> std::io::Result<ServerRuntimeStatus> {
    match ApiClient::local().status() {
        Ok(status) => Ok(ServerRuntimeStatus::Running {
            version: status.version,
            protocol: status.protocol,
            capabilities: status.capabilities,
            session_health: status.session_health,
            api_listener: status.api_listener,
        }),
        Err(ApiClientError::Io(err)) if server_not_running_error(&err) => {
            Ok(ServerRuntimeStatus::NotRunning)
        }
        Err(err) => Err(api_client_error_to_io(err)),
    }
}

fn server_not_running_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

fn api_client_error_to_io(err: ApiClientError) -> std::io::Error {
    match err {
        ApiClientError::Io(err) => err,
        err => std::io::Error::other(err),
    }
}

fn option_label(value: Option<&str>) -> &str {
    value.unwrap_or("unknown")
}

fn protocol_label(protocol: Option<u32>) -> String {
    protocol
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn compatibility_label(protocol: Option<u32>) -> &'static str {
    match protocol {
        Some(protocol) if protocol == crate::protocol::PROTOCOL_VERSION => "yes",
        Some(_) => "no",
        None => "unknown",
    }
}

fn restart_needed_label(server: &ServerRuntimeStatus) -> &'static str {
    match server {
        ServerRuntimeStatus::Running { version, .. } => match version.as_deref() {
            Some(version) if version == crate::build_info::version() => "no",
            Some(_) => "yes",
            None => "unknown",
        },
        ServerRuntimeStatus::NotRunning => "no",
    }
}

#[derive(Serialize)]
struct FullStatusJson {
    client: ClientStatusJson,
    server: ServerStatusJson,
    update: UpdateStatusJson,
}

#[derive(Serialize)]
struct ClientStatusJson {
    version: String,
    channel: &'static str,
    protocol: u32,
    binary: String,
    session: Option<String>,
}

#[derive(Serialize)]
struct ServerStatusJson {
    status: &'static str,
    running: bool,
    version: Option<String>,
    protocol: Option<u32>,
    capabilities: Option<ServerCapabilitiesJson>,
    compatible: Option<bool>,
    socket: String,
    session: Option<String>,
    restart_needed: Option<bool>,
    /// #426: `broken` only when the server says so. A missing field is `None`,
    /// not `"broken"` and not `"healthy"` — an older server genuinely did not
    /// report this, and collapsing that into a value would assert something
    /// nobody knows.
    session_health: Option<&'static str>,
    api_listener: Option<crate::api::schema::ApiListenerHealth>,
}

#[derive(Serialize)]
struct ServerCapabilitiesJson {
    live_handoff: bool,
}

#[derive(Serialize)]
struct UpdateStatusJson {
    restart_needed: Option<bool>,
}

fn client_status_json() -> ClientStatusJson {
    ClientStatusJson {
        version: crate::build_info::version(),
        channel: crate::config::Config::load().config.update.channel.as_str(),
        protocol: crate::protocol::PROTOCOL_VERSION,
        binary: current_exe_label(),
        session: crate::session::active_name(),
    }
}

fn server_status_json(server: &ServerRuntimeStatus) -> ServerStatusJson {
    match server {
        ServerRuntimeStatus::Running {
            version,
            protocol,
            capabilities,
            session_health,
            api_listener,
        } => ServerStatusJson {
            status: "running",
            running: true,
            version: version.clone(),
            protocol: *protocol,
            capabilities: capabilities
                .as_ref()
                .map(|capabilities| ServerCapabilitiesJson {
                    live_handoff: capabilities.live_handoff,
                }),
            compatible: protocol.map(|value| value == crate::protocol::PROTOCOL_VERSION),
            socket: api::socket_path().display().to_string(),
            session: crate::session::active_name(),
            restart_needed: restart_needed_bool(server),
            session_health: session_health_label(*session_health),
            api_listener: api_listener.clone(),
        },
        ServerRuntimeStatus::NotRunning => ServerStatusJson {
            status: "not_running",
            running: false,
            version: None,
            protocol: None,
            capabilities: None,
            compatible: None,
            socket: api::socket_path().display().to_string(),
            session: crate::session::active_name(),
            restart_needed: Some(false),
            session_health: None,
            api_listener: None,
        },
    }
}

pub(crate) fn session_health_label(
    health: Option<crate::platform::SessionHealth>,
) -> Option<&'static str> {
    match health? {
        crate::platform::SessionHealth::Healthy => Some("healthy"),
        crate::platform::SessionHealth::Broken => Some(crate::health::session_warning::STATUS_LINE),
    }
}

fn update_status_json(server: &ServerRuntimeStatus) -> UpdateStatusJson {
    UpdateStatusJson {
        restart_needed: restart_needed_bool(server),
    }
}

pub(crate) fn restart_needed_bool(server: &ServerRuntimeStatus) -> Option<bool> {
    match server {
        ServerRuntimeStatus::Running { version, .. } => match version.as_deref() {
            Some(version) if version == crate::build_info::version() => Some(false),
            Some(_) => Some(true),
            None => None,
        },
        ServerRuntimeStatus::NotRunning => Some(false),
    }
}

fn print_json(value: &impl Serialize) -> std::io::Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

pub(crate) fn current_exe_label() -> String {
    std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|err| format!("unknown ({err})"))
}

/// Usage after a bad argument: stderr, because it accompanies a failure.
fn print_status_help() {
    eprint!("{}", status_help_text());
}

fn status_help_text() -> String {
    let mut out = String::new();
    use std::fmt::Write as _;
    let _ = writeln!(out, "flk status commands:");
    let _ = writeln!(
        out,
        "  flk status [--json]         show local client and running server status"
    );
    let _ = writeln!(
        out,
        "  flk status server [--json]  show running server status"
    );
    let _ = writeln!(
        out,
        "  flk status client [--json]  show local client binary status"
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_status_json_retains_accept_error_diagnostics() {
        let server = ServerRuntimeStatus::Running {
            version: None,
            protocol: None,
            capabilities: None,
            session_health: None,
            api_listener: Some(crate::api::schema::ApiListenerHealth {
                accept_errors: 2,
                last_accept_error: Some("injected accept failure".into()),
            }),
        };
        let json = serde_json::to_value(server_status_json(&server)).unwrap();
        assert_eq!(json["api_listener"]["accept_errors"], 2);
        assert_eq!(
            json["api_listener"]["last_accept_error"],
            "injected accept failure"
        );
    }
}
