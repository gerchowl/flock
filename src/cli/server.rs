#![expect(
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stdout/stderr for humans and scripts"
)]
use crate::api::schema::{EmptyParams, Method, Request, ServerLiveHandoffParams};

pub(super) fn run_server_command(args: &[String]) -> std::io::Result<Option<i32>> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        return Ok(None);
    };

    match subcommand {
        "stop" => server_stop(&args[1..]).map(Some),
        "live-handoff" => server_live_handoff(&args[1..]).map(Some),
        "--handoff-import" => Ok(None),
        "reload-config" => server_reload_config(&args[1..]).map(Some),
        "help" | "--help" | "-h" => {
            print_server_help();
            Ok(Some(0))
        }
        _ => {
            print_server_help();
            Ok(Some(2))
        }
    }
}

fn server_stop(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: flk server stop");
        return Ok(super::exit::USAGE);
    }

    super::send_ok_request(Method::ServerStop(EmptyParams::default()))
}

fn server_reload_config(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: flk server reload-config");
        return Ok(super::exit::USAGE);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:server:reload-config".into(),
        method: Method::ServerReloadConfig(EmptyParams::default()),
    })?)
}

fn server_live_handoff(args: &[String]) -> std::io::Result<i32> {
    let Some(mut params) = parse_live_handoff_params(args) else {
        eprintln!(
            "usage: flk server live-handoff [--import-exe <path>] [--expected-protocol <n>] [--expected-version <version>] [--skip-undrained]"
        );
        return Ok(super::exit::USAGE);
    };

    let socket_path = crate::api::socket_path();
    let before = crate::api::read_runtime_status_at(&socket_path, HANDOFF_STATUS_TIMEOUT)
        .ok()
        .flatten()
        .and_then(|status| status.version);
    let client_version = crate::build_info::version();
    // #958: with no --import-exe the server re-executes its own binary, which
    // on a versioned install (a Nix store path) is the old version forever.
    // The binary the operator just upgraded to is this one.
    if let Some(exe) = params.import_exe.as_deref() {
        // The server resolves a relative path against its own cwd, not ours.
        let resolved =
            crate::server::executable::resolve(Some(std::path::Path::new(exe)))?.into_path();
        if params.expected_version.is_none() {
            params.expected_version = probe_executable_version(&resolved);
        }
        params.import_exe = Some(resolved.display().to_string());
    } else {
        let own_exe = own_executable()?;
        if let Some(refusal) = refuse_downgrade(
            before.as_deref(),
            &client_version,
            Some(&own_exe),
            &socket_path,
        ) {
            eprintln!("{refusal}");
            return Ok(super::exit::FAILURE);
        }
        fill_import_target_from_client(&mut params, own_exe);
    }
    if let Some(refusal) = refuse_stale_import_target(
        before.as_deref(),
        params.expected_version.as_deref(),
        &client_version,
    ) {
        eprintln!("{refusal}");
        return Ok(super::exit::FAILURE);
    }
    let target_version = params.expected_version.clone();

    let response = super::send_request(&Request {
        id: "cli:server:live-handoff".into(),
        method: Method::ServerLiveHandoff(params),
    })?;
    if response.get("error").is_some() {
        // #426: the lost-session refusal is the one failure whose message is
        // written to be read by a person, and it is the one where raw JSON is
        // actively unhelpful — the operator needs to be told that the obvious
        // next move cannot work and what does. Everything else keeps the
        // machine-readable shape.
        if let Some(message) = error_message_for_human(&response) {
            eprintln!("{message}");
            return Ok(super::exit::FAILURE);
        }
        if let Some(guidance) = old_server_drain_guidance(&response) {
            eprintln!("{guidance}");
            return Ok(super::exit::FAILURE);
        }
        let rendered = serde_json::to_string(&response).unwrap_or_else(|err| {
            format!(
                "{{\"error\":{{\"code\":\"render_failed\",\"message\":\"failed to render error response: {err}\"}}}}"
            )
        });
        eprintln!("{rendered}");
        return Ok(super::exit::FAILURE);
    }

    for pane in skipped_undrained(&response) {
        eprintln!("{pane}");
    }
    let after = wait_for_replacement_version(&socket_path, target_version.as_deref());
    let log = crate::session::data_dir().join("flock-server.log");
    match judge_handoff(
        before.as_deref(),
        after.as_deref(),
        target_version.as_deref(),
        &client_version,
    ) {
        Ok(summary) => {
            eprintln!("{summary}; server log: {}", log.display());
            Ok(super::exit::SUCCESS)
        }
        Err(problem) => {
            eprintln!("{problem}\nserver log: {}", log.display());
            Ok(super::exit::FAILURE)
        }
    }
}

const HANDOFF_STATUS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const HANDOFF_VERIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// This CLI's own binary, resolved through symlinks so the handoff targets the
/// store path the profile points at right now, not a link that a later
/// profile switch could repoint mid-handoff.
fn own_executable() -> std::io::Result<std::path::PathBuf> {
    crate::server::executable::resolve(None).map(|exe| exe.into_path())
}

/// Point the handoff at the invoking client's binary and pin its identity, so
/// the importer refuses unless it really is this build. An explicit
/// `--expected-*` flag is kept. The caller must resolve its binary first,
/// refusing an unresolved target before sending a handoff request.
fn fill_import_target_from_client(
    params: &mut ServerLiveHandoffParams,
    own_exe: std::path::PathBuf,
) {
    params.import_exe = Some(own_exe.display().to_string());
    params
        .expected_version
        .get_or_insert_with(crate::build_info::version);
    params
        .expected_protocol
        .get_or_insert(crate::protocol::PROTOCOL_VERSION);
}

/// The version an `--import-exe` binary reports, from its `--version` line.
fn probe_executable_version(exe: &std::path::Path) -> Option<String> {
    let output = crate::process::TracedCommand::new(exe, "server")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output_traced_with_timeout(HANDOFF_STATUS_TIMEOUT)
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_version_line(&String::from_utf8_lossy(&output.stdout))
}

fn parse_version_line(stdout: &str) -> Option<String> {
    let version = stdout.lines().next()?.trim().strip_prefix("flk ")?.trim();
    (!version.is_empty()).then(|| version.to_string())
}

/// Refuse a handoff that would only re-run the server's own version while this
/// client is a different one: that is the #958 shape, an "upgrade" that keeps
/// the old build, and it used to report success.
fn refuse_stale_import_target(
    server_version: Option<&str>,
    target_version: Option<&str>,
    client_version: &str,
) -> Option<String> {
    let server = server_version?;
    let target = target_version?;
    if target != server || target == client_version {
        return None;
    }
    Some(format!(
        "refusing live handoff: the import binary is flk {target}, the version the server already runs, but this client is flk {client_version}.\n{}",
        upgrade_hint()
    ))
}

/// The `major.minor.patch` a version string starts with, ignoring a channel
/// suffix such as `-latest.abc`.
fn base_version(version: &str) -> Option<(u32, u32, u32)> {
    let base = version.strip_prefix('v').unwrap_or(version);
    let base = base.split(['-', '+']).next()?;
    let mut parts = base.split('.');
    let parsed = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(parsed)
}

/// Refuse a default handoff from a client older than the running server: the
/// default target is this client's binary, so a stale `flk` earlier on PATH
/// would quietly downgrade every pane. `--import-exe` stays the explicit way.
fn refuse_downgrade(
    server_version: Option<&str>,
    client_version: &str,
    client_exe: Option<&std::path::Path>,
    socket_path: &std::path::Path,
) -> Option<String> {
    let server = server_version?;
    if base_version(client_version)? >= base_version(server)? {
        return None;
    }
    let client_path = client_exe
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "unknown path".to_string());
    Some(format!(
        "refusing live handoff: this client is flk {client_version} ({client_path}), older than the server's flk {server} (on {}).\nA handoff would downgrade the server to this client's binary. Run the newer flk, or pass --import-exe <path> to choose the binary explicitly.",
        socket_path.display()
    ))
}

fn upgrade_hint() -> String {
    "To hand off to this client's binary, run `flk server live-handoff` without --import-exe, or pass it explicitly: `flk server live-handoff --import-exe \"$(readlink -f \"$(command -v flk)\")\"`.".to_string()
}

/// Poll the replacement server until it reports `target`, or report whatever
/// version answered last once the budget runs out.
fn wait_for_replacement_version(
    socket_path: &std::path::Path,
    target: Option<&str>,
) -> Option<String> {
    let deadline = std::time::Instant::now() + HANDOFF_VERIFY_TIMEOUT;
    let mut last = None;
    loop {
        if let Ok(Some(status)) =
            crate::api::read_runtime_status_at(socket_path, HANDOFF_STATUS_TIMEOUT)
        {
            last = status.version;
            if target.is_none() || last.as_deref() == target {
                return last;
            }
        }
        if std::time::Instant::now() >= deadline {
            return last;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// The handoff's report: both versions on success, and a failure, not a
/// success line, when the replacement is not the build that was asked for.
fn judge_handoff(
    before: Option<&str>,
    after: Option<&str>,
    target: Option<&str>,
    client_version: &str,
) -> Result<String, String> {
    let before_label = before.unwrap_or("unknown");
    let Some(after) = after else {
        return Err(
            "live handoff was accepted, but no server answered on the API socket afterwards"
                .to_string(),
        );
    };
    if let Some(target) = target {
        if after != target {
            return Err(format!(
                "live handoff ran, but the server reports flk {after}, not the requested flk {target} (was flk {before_label})"
            ));
        }
    } else if before == Some(after) && after != client_version {
        return Err(format!(
            "live handoff re-executed the same server version: the server is still flk {after}, but this client is flk {client_version}.\n{}",
            upgrade_hint()
        ));
    }
    Ok(format!(
        "live handoff complete: server flk {before_label} -> flk {after}"
    ))
}

/// The prose message for the errors whose message was written for a person, or
/// `None` to keep the JSON rendering every other failure uses.
///
/// Keyed on the code rather than matched by text, so an unrelated failure can
/// never start being pretty-printed because its message happens to read like
/// one of these.
fn error_message_for_human(response: &serde_json::Value) -> Option<&str> {
    let error = response.get("error")?;
    match error.get("code")?.as_str()? {
        "session_broken" | "handoff_undrained" => error.get("message")?.as_str(),
        _ => None,
    }
}

/// The drain error as a server older than #1011 sends it: no pane named and no
/// `--skip-undrained` to honour, because the OLD server is the one that drains.
/// This tells the operator how to get off it without a restart that loses every
/// pane.
fn old_server_drain_guidance(response: &serde_json::Value) -> Option<String> {
    let error = response.get("error")?;
    if error.get("code")?.as_str()? != "handoff_failed" {
        return None;
    }
    let message = error.get("message")?.as_str()?;
    if !message.contains("timed out draining PTY writes before handoff") {
        return None;
    }
    let data_dir = crate::session::data_dir();
    Some(format!(
        "{message}\n\
         The running server predates per-pane drain reporting, so it cannot name the \
         stuck panes or honour --skip-undrained. To get off it without a restart:\n\
         \x20 1. Find the stuck panes: grep 'write returned zero bytes' {log} and \
         note each \"pane\":N (an internal number).\n\
         \x20 2. Map N to its public id: jq -r --arg n N '.workspaces[] | \
         select(.public_pane_numbers[$n]) | \"\\(.id):p\\(.public_pane_numbers[$n])\"' \
         {session} (pane numbers 1-9 read as-is; 10 is pA, 11 pB and so on).\n\
         \x20 3. Close those panes (in the TUI, or flk pane close <wX:pN>), then retry \
         flk server live-handoff. From this release on, a stuck pane is named in the \
         error and --skip-undrained hands off past it.",
        log = data_dir.join("flock-server.log").display(),
        session = data_dir.join("session.json").display(),
    ))
}

/// One line per pane whose pending input `--skip-undrained` dropped.
fn skipped_undrained(response: &serde_json::Value) -> Vec<String> {
    let Some(panes) = response
        .get("result")
        .and_then(|result| result.get("skipped_undrained"))
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };
    panes
        .iter()
        .map(|pane| {
            let id = pane
                .get("pane_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let bytes = pane
                .get("pending_bytes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            format!("skipped undrained pane {id}: dropped {bytes} pending bytes")
        })
        .collect()
}

fn parse_live_handoff_params(args: &[String]) -> Option<ServerLiveHandoffParams> {
    let mut params = ServerLiveHandoffParams::default();
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        if arg == "--skip-undrained" {
            params.skip_undrained = true;
            idx += 1;
            continue;
        }
        let (flag, value) = if let Some((flag, value)) = arg.split_once('=') {
            (flag, Some(value.to_string()))
        } else {
            let value = args.get(idx + 1).cloned();
            idx += 1;
            (arg.as_str(), value)
        };
        let value = value?;
        match flag {
            "--import-exe" => params.import_exe = Some(value),
            "--expected-protocol" => {
                params.expected_protocol = Some(value.parse().ok()?);
            }
            "--expected-version" => params.expected_version = Some(value),
            _ => return None,
        }
        idx += 1;
    }
    Some(params)
}

fn print_server_help() {
    eprintln!("flk server commands:");
    eprintln!("  flk server                run as headless server");
    eprintln!("  flk server stop           stop the running server via the API socket");
    eprintln!("  flk server live-handoff   hand off live panes to a new local server");
    eprintln!("  flk server reload-config  reload config.toml in the running server");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_handoff_params_parse_remote_update_fields() {
        let args = vec![
            "--import-exe".to_string(),
            "/home/me/.local/bin/flk".to_string(),
            "--expected-protocol=9".to_string(),
            "--expected-version".to_string(),
            "0.6.2".to_string(),
        ];

        let params = parse_live_handoff_params(&args).expect("params");

        assert_eq!(
            params.import_exe.as_deref(),
            Some("/home/me/.local/bin/flk")
        );
        assert_eq!(params.expected_protocol, Some(9));
        assert_eq!(params.expected_version.as_deref(), Some("0.6.2"));
    }

    #[test]
    fn live_handoff_params_parse_skip_undrained_flag() {
        let args = vec![
            "--skip-undrained".to_string(),
            "--expected-version".to_string(),
            "1.2.0".to_string(),
        ];

        let params = parse_live_handoff_params(&args).expect("params");

        assert!(params.skip_undrained);
        assert_eq!(params.expected_version.as_deref(), Some("1.2.0"));
        assert!(
            !parse_live_handoff_params(&[])
                .expect("params")
                .skip_undrained
        );
    }

    #[test]
    fn undrained_refusal_prints_as_prose() {
        let response = serde_json::json!({
            "id": "cli:server:live-handoff",
            "error": {
                "code": "handoff_undrained",
                "message": "timed out draining PTY writes before handoff: w1:p2 (3 bytes pending)."
            }
        });

        assert_eq!(
            error_message_for_human(&response),
            Some("timed out draining PTY writes before handoff: w1:p2 (3 bytes pending).")
        );
        assert!(old_server_drain_guidance(&response).is_none());
    }

    #[test]
    fn old_server_drain_error_gets_recovery_guidance() {
        let response = serde_json::json!({
            "id": "cli:server:live-handoff",
            "error": {
                "code": "handoff_failed",
                "message": "timed out draining PTY writes before handoff"
            }
        });

        let guidance = old_server_drain_guidance(&response).expect("guidance");

        assert!(guidance.starts_with("timed out draining PTY writes before handoff\n"));
        assert!(guidance.contains("write returned zero bytes"));
        assert!(guidance.contains("public_pane_numbers"));
        assert!(guidance.contains("flk pane close"));
        let other = serde_json::json!({
            "error": {"code": "handoff_failed", "message": "handoff import refused: protocol"}
        });
        assert!(old_server_drain_guidance(&other).is_none());
    }

    #[test]
    fn skipped_undrained_lists_each_dropped_pane() {
        let response = serde_json::json!({
            "id": "cli:server:live-handoff",
            "result": {
                "type": "live_handoff",
                "skipped_undrained": [
                    {"pane_id": "w1:p2", "pending_bytes": 3},
                    {"pane_id": "w2:p6", "pid": 7, "pending_bytes": 17}
                ]
            }
        });

        assert_eq!(
            skipped_undrained(&response),
            vec![
                "skipped undrained pane w1:p2: dropped 3 pending bytes".to_string(),
                "skipped undrained pane w2:p6: dropped 17 pending bytes".to_string(),
            ]
        );
        let ok = serde_json::json!({"result": {"type": "ok"}});
        assert!(skipped_undrained(&ok).is_empty());
    }

    #[test]
    fn default_handoff_targets_the_client_binary_and_pins_its_identity() {
        let mut params = ServerLiveHandoffParams::default();

        fill_import_target_from_client(
            &mut params,
            std::path::PathBuf::from("/nix/store/new-flock/bin/flk"),
        );

        assert_eq!(
            params.import_exe.as_deref(),
            Some("/nix/store/new-flock/bin/flk")
        );
        assert_eq!(params.expected_version, Some(crate::build_info::version()));
        assert_eq!(
            params.expected_protocol,
            Some(crate::protocol::PROTOCOL_VERSION)
        );
    }

    #[test]
    fn default_handoff_keeps_an_explicit_expected_version() {
        let mut params = ServerLiveHandoffParams {
            expected_version: Some("9.9.9".to_string()),
            ..ServerLiveHandoffParams::default()
        };

        fill_import_target_from_client(
            &mut params,
            std::path::PathBuf::from("/nix/store/new-flock/bin/flk"),
        );

        assert_eq!(params.expected_version.as_deref(), Some("9.9.9"));
    }

    #[test]
    fn own_executable_resolves_symlinks_to_the_versioned_path() {
        let exe = own_executable().expect("test binary path");

        assert_eq!(std::fs::canonicalize(&exe).ok(), Some(exe));
    }

    #[test]
    fn version_line_parses_the_flk_banner() {
        assert_eq!(
            parse_version_line("flk 1.2.0-latest.abc\n").as_deref(),
            Some("1.2.0-latest.abc")
        );
        assert_eq!(parse_version_line("something else\n"), None);
        assert_eq!(parse_version_line(""), None);
    }

    #[test]
    fn refuses_an_import_target_that_is_the_old_server_version() {
        let refusal = refuse_stale_import_target(Some("0.10.0"), Some("0.10.0"), "1.0.0")
            .expect("stale target refused");

        assert!(refusal.contains("flk 0.10.0"), "{refusal}");
        assert!(refusal.contains("flk 1.0.0"), "{refusal}");
        assert!(refusal.contains("--import-exe"), "{refusal}");
    }

    #[test]
    fn allows_upgrades_and_same_version_restarts() {
        assert_eq!(
            refuse_stale_import_target(Some("0.10.0"), Some("1.0.0"), "1.0.0"),
            None
        );
        assert_eq!(
            refuse_stale_import_target(Some("1.0.0"), Some("1.0.0"), "1.0.0"),
            None
        );
        assert_eq!(
            refuse_stale_import_target(None, Some("0.10.0"), "1.0.0"),
            None
        );
        assert_eq!(
            refuse_stale_import_target(Some("0.10.0"), None, "1.0.0"),
            None
        );
    }

    #[test]
    fn judge_reports_both_versions_on_an_upgrade() {
        assert_eq!(
            judge_handoff(Some("0.10.0"), Some("1.0.0"), Some("1.0.0"), "1.0.0"),
            Ok("live handoff complete: server flk 0.10.0 -> flk 1.0.0".to_string())
        );
    }

    #[test]
    fn judge_fails_when_the_replacement_is_not_the_target() {
        let problem = judge_handoff(Some("0.10.0"), Some("0.10.0"), Some("1.0.0"), "1.0.0")
            .expect_err("wrong replacement version");

        assert!(problem.contains("not the requested flk 1.0.0"), "{problem}");
    }

    #[test]
    fn judge_fails_a_same_version_reexec_when_the_client_is_newer() {
        let problem = judge_handoff(Some("0.10.0"), Some("0.10.0"), None, "1.0.0")
            .expect_err("#958 shape is not success");

        assert!(problem.contains("same server version"), "{problem}");
        assert!(problem.contains("--import-exe"), "{problem}");
    }

    #[test]
    fn judge_fails_when_no_server_answers_afterwards() {
        assert!(judge_handoff(Some("1.0.0"), None, Some("1.0.0"), "1.0.0").is_err());
    }

    #[test]
    fn base_version_ignores_channel_suffixes() {
        assert_eq!(base_version("1.2.0"), Some((1, 2, 0)));
        assert_eq!(base_version("v1.2.0-latest.abc"), Some((1, 2, 0)));
        assert_eq!(base_version("1.2"), None);
        assert_eq!(base_version("1.2.0.4"), None);
        assert_eq!(base_version("unknown"), None);
    }

    #[test]
    fn refuses_a_default_handoff_from_an_older_client() {
        let refusal = refuse_downgrade(
            Some("1.3.0"),
            "1.2.0",
            Some(std::path::Path::new("/home/me/.cargo/bin/flk")),
            std::path::Path::new("/run/flock.sock"),
        )
        .expect("downgrade refused");

        assert!(refusal.contains("flk 1.2.0"), "{refusal}");
        assert!(refusal.contains("flk 1.3.0"), "{refusal}");
        assert!(refusal.contains("/home/me/.cargo/bin/flk"), "{refusal}");
        assert!(refusal.contains("--import-exe"), "{refusal}");
    }

    #[test]
    fn allows_a_default_handoff_from_an_equal_or_newer_client() {
        let socket = std::path::Path::new("/run/flock.sock");
        assert_eq!(refuse_downgrade(Some("1.2.0"), "1.3.0", None, socket), None);
        assert_eq!(
            refuse_downgrade(Some("1.2.0"), "1.2.0-latest.abc", None, socket),
            None
        );
        assert_eq!(refuse_downgrade(None, "1.2.0", None, socket), None);
        assert_eq!(refuse_downgrade(Some("odd"), "1.2.0", None, socket), None);
    }
}
