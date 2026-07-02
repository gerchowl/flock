#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stdout/stderr for humans and scripts"
)]
use crate::api::schema::{EmptyParams, Method, PeersCheckoutPrepareParams, Request};

pub(super) fn run_peers_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_peers_help();
        return Ok(2);
    };

    match subcommand {
        "summary" => peers_summary(&args[1..]),
        "checkout-prepare" => peers_checkout_prepare(&args[1..]),
        "logs" => peers_logs(&args[1..]),
        "help" | "--help" | "-h" => {
            print_peers_help();
            Ok(0)
        }
        _ => {
            print_peers_help();
            Ok(2)
        }
    }
}

/// Default and max tail size for `peers logs`. The cap bounds the over-SSH wire
/// payload when a hub fans out across the fleet (the live log file is ≤5 MiB).
const DEFAULT_LOG_LINES: u32 = 200;
const MAX_LOG_LINES: u32 = 5000;

/// Tail this node's session logs (`--json` for the structured envelope a hub
/// fetches over SSH), or with `--all` fan out across configured `[[peers]]` and
/// interleave every node's logs by timestamp into one stream (#67).
fn peers_logs(args: &[String]) -> std::io::Result<i32> {
    let LogsArgs { json, all, lines } = match parse_logs_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    let records = if all {
        fleet_log_records(lines)
    } else {
        crate::logging::tail_session_logs(lines as usize)
    };

    if json {
        print_logs_json(&records);
    } else {
        print_logs_human(&records, all);
    }
    Ok(0)
}

struct LogsArgs {
    json: bool,
    all: bool,
    lines: u32,
}

/// Parse `peers logs` flags. `--lines` is clamped to `1..=MAX_LOG_LINES`. Pure
/// so the flag handling is testable without touching logs or ssh.
fn parse_logs_args(args: &[String]) -> Result<LogsArgs, String> {
    let mut parsed = LogsArgs {
        json: false,
        all: false,
        lines: DEFAULT_LOG_LINES,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => parsed.json = true,
            "--all" => parsed.all = true,
            "--lines" | "-n" => {
                let value = iter
                    .next()
                    .ok_or_else(|| format!("{arg} requires a count"))?;
                let n: u32 = value
                    .parse()
                    .map_err(|_| format!("invalid --lines value: {value}"))?;
                parsed.lines = n.clamp(1, MAX_LOG_LINES);
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }
    Ok(parsed)
}

/// Local logs plus each peer's logs (fetched over SSH), tagged by host and
/// merged by timestamp. A peer that can't be reached is reported on stderr and
/// skipped — the merged view still shows every node that answered.
fn fleet_log_records(lines: u32) -> Vec<crate::logging::LogLine> {
    let local_host = crate::app::short_host_name();
    let mut records: Vec<crate::logging::LogLine> =
        crate::logging::tail_session_logs(lines as usize)
            .into_iter()
            .map(|mut record| {
                record.host = Some(local_host.clone());
                record
            })
            .collect();

    let peers = crate::config::Config::load().config.peers;
    let handles: Vec<_> = peers
        .into_iter()
        .map(|peer| {
            std::thread::spawn(move || {
                (
                    peer.name.clone(),
                    crate::peers::run_logs_command(&peer, lines),
                )
            })
        })
        .collect();
    for handle in handles {
        let (host, result) = match handle.join() {
            Ok(joined) => joined,
            Err(_) => {
                eprintln!("peer log fetch thread panicked");
                continue;
            }
        };
        match result {
            Ok(peer_records) => records.extend(peer_records.into_iter().map(|mut record| {
                record.host = Some(host.clone());
                record
            })),
            Err(err) => eprintln!("peer {host}: {err}"),
        }
    }

    crate::logging::merge_log_records(records, lines as usize)
}

/// The JSON envelope a hub fetches over SSH — the same shape `peers summary`
/// uses, so `crate::peers::run_logs_command` can parse it back.
fn print_logs_json(records: &[crate::logging::LogLine]) {
    let envelope = serde_json::json!({
        "id": "cli:peers:logs",
        "result": {
            "type": "peers_logs",
            "host": crate::app::short_host_name(),
            "lines": records,
        }
    });
    println!("{envelope}");
}

fn print_logs_human(records: &[crate::logging::LogLine], show_host: bool) {
    for record in records {
        let host = if show_host {
            format!("{} ", record.host.as_deref().unwrap_or("?"))
        } else {
            String::new()
        };
        println!(
            "{ts}  {level:<5}  {host}{target}: {message}",
            ts = record.ts,
            level = record.level,
            host = host,
            target = record.target,
            message = record.message,
        );
    }
}

/// This server's federated summary (workspaces + agent statuses). Peer
/// servers run this over SSH to fold our workspaces into their sidebars.
fn peers_summary(args: &[String]) -> std::io::Result<i32> {
    for arg in args {
        match arg.as_str() {
            // Output is always JSON; the flag is accepted for symmetry with
            // the other read-only subcommands.
            "--json" => {}
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:peers:summary".into(),
        method: Method::PeersSummary(EmptyParams {}),
    })?)
}

/// Prepare one of this server's workspaces for a cross-machine checkout (#125):
/// the hub invokes this over SSH, the local server resolves the repo + branch
/// and acts on its own git, then the hub fetches the branch from origin. With
/// `--push` the branch is pushed to origin; without it this is a read-only
/// probe (dirty / unpushed state) for the hub's pre-action confirmation.
fn peers_checkout_prepare(args: &[String]) -> std::io::Result<i32> {
    let mut workspace_id: Option<String> = None;
    let mut push = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--workspace" | "--workspace-id" => {
                let Some(value) = iter.next() else {
                    eprintln!("{arg} requires a workspace id");
                    return Ok(2);
                };
                workspace_id = Some(value.clone());
            }
            "--push" => push = true,
            // Output is always JSON; the flag is accepted for symmetry.
            "--json" => {}
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    let Some(workspace_id) = workspace_id else {
        eprintln!("usage: flock peers checkout-prepare --workspace <id> [--push] [--json]");
        return Ok(2);
    };

    super::print_response(&super::send_request(&Request {
        id: "cli:peers:checkout_prepare".into(),
        method: Method::PeersCheckoutPrepare(PeersCheckoutPrepareParams { workspace_id, push }),
    })?)
}

fn print_peers_help() {
    eprintln!("usage: flock peers summary [--json]");
    eprintln!("       flock peers checkout-prepare --workspace <id> [--push] [--json]");
    eprintln!("       flock peers logs [--all] [--lines N] [--json]");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_logs_args_defaults_and_flags() {
        let parsed = parse_logs_args(&args(&[])).unwrap();
        assert!(!parsed.json && !parsed.all);
        assert_eq!(parsed.lines, DEFAULT_LOG_LINES);

        let parsed = parse_logs_args(&args(&["--all", "--json", "--lines", "50"])).unwrap();
        assert!(parsed.json && parsed.all);
        assert_eq!(parsed.lines, 50);
    }

    #[test]
    fn parse_logs_args_clamps_lines_and_rejects_bad_input() {
        assert_eq!(
            parse_logs_args(&args(&["-n", "999999"])).unwrap().lines,
            MAX_LOG_LINES
        );
        assert_eq!(parse_logs_args(&args(&["-n", "0"])).unwrap().lines, 1);
        assert!(parse_logs_args(&args(&["--lines"])).is_err());
        assert!(parse_logs_args(&args(&["--lines", "abc"])).is_err());
        assert!(parse_logs_args(&args(&["--bogus"])).is_err());
    }
}
