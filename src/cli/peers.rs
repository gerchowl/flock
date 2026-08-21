#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stdout/stderr for humans and scripts"
)]
use crate::api::schema::{EmptyParams, Method, PeersCheckoutPrepareParams, Request};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

pub(super) fn run_peers_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_peers_help();
        return Ok(2);
    };

    match subcommand {
        "summary" => peers_summary(&args[1..]),
        "checkout-prepare" => peers_checkout_prepare(&args[1..]),
        "logs" => peers_logs(&args[1..]),
        "relay" => peers_relay(&args[1..]),
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
        eprintln!("usage: flk peers checkout-prepare --workspace <id> [--push] [--json]");
        return Ok(2);
    };

    super::print_response(&super::send_request(&Request {
        id: "cli:peers:checkout_prepare".into(),
        method: Method::PeersCheckoutPrepare(PeersCheckoutPrepareParams { workspace_id, push }),
    })?)
}

/// Relay API requests from stdin to this node's socket, responses to stdout.
///
/// The remote half of a hub that holds ONE ssh connection to a peer instead of
/// spawning a fresh one per call. Measured cold handshake with multiplexing
/// disabled: 0.01s on the LAN but 0.42s to a Tailscale-remote peer (0.95s
/// cold). At that cost the handshake dominates — a few hundred bytes of
/// payload behind half a second of setup, paid again on every poll and every
/// message.
///
/// A multiplexer, NOT a byte pump: the API server is one-request-per-
/// connection, so each stdin line gets its own short-lived local socket. Those
/// are ~free (no handshake, no network); the ssh connection is the expensive
/// thing and it is what gets amortized.
///
/// Not a second server either. The running instance owns `api_tx` and all
/// state; this only carries bytes to it. Requests therefore arrive with no
/// local process ancestry — the parent is sshd, not a pane — so they attest to
/// no pane at all. That is correct rather than a gap: a relayed send takes its
/// name from the relay's asserted `from_agent`, never from ancestry (P3).
fn peers_relay(args: &[String]) -> std::io::Result<i32> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_peers_help();
        return Ok(0);
    }
    // CLI commands normally skip logging setup — they are one-shot and a log
    // line per `flk status` would be noise. The relay is the exception: it is
    // long-lived, it runs on the PEER where nobody is watching stdout, and its
    // failures (a refused subscription, a dead socket) are otherwise
    // indistinguishable from a node where nothing happened. Its own file, so
    // it never races the server's rotating writer.
    crate::logging::init_file_logging("flock-relay.log");
    let socket = crate::api::socket_path();
    let pushing = args.iter().any(|arg| arg == "--push");
    crate::logging::peer_relay_started(pushing);
    if pushing {
        start_summary_push(socket.clone());
    }
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        if stdin.lock().read_line(&mut line)? == 0 {
            return Ok(0);
        }
        let request = line.trim();
        if request.is_empty() {
            continue;
        }
        // A closed stdout means the hub hung up. That is the ordinary way
        // this process ends, not a failure worth a message nobody can read.
        match relay_one_request(&socket, request) {
            Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => return Ok(0),
            other => other?,
        }
    }
}

/// The refusal, if this request cannot be carried — split from the IO so the
/// decision is testable without a socket or a server.
fn relay_refusal(request: &str) -> Option<(String, &'static str, String)> {
    let parsed: serde_json::Value = serde_json::from_str(request).ok()?;
    let id = parsed
        .get("id")
        .and_then(|id| id.as_str())
        .unwrap_or_default()
        .to_string();
    let method = parsed.get("method").and_then(|method| method.as_str())?;
    STREAMING_METHODS.contains(&method).then(|| {
        (
            id,
            "unsupported_over_relay",
            format!("{method} streams and would block the relay; not carried over this transport"),
        )
    })
}

/// Emit a fresh summary up the pipe whenever this node's state changes.
///
/// Pushes a SUMMARY rather than raw events, for two reasons. The hub already
/// has exactly one path that applies a summary, so a push needs no second
/// application path to drift from it. And a summary is inherently coalesced —
/// it is a snapshot, so when changes arrive faster than the debounce only the
/// latest matters, and dropping the intermediates is free rather than lossy.
///
/// The debounce is TRAILING-edge, and that distinction is the whole of #4.
/// Leading-edge coalescing drops every event inside the window, which is
/// harmless while events keep coming — the next one carries what the dropped
/// ones would have said — and silently wrong the moment a burst ENDS inside
/// the window. That is not an edge case: it is what every state change looks
/// like. An agent goes working, then idle 300ms later; the first event pushes
/// a snapshot saying "working", the second is dropped, and no further event is
/// coming because the node just went quiet. The hub then shows "working" until
/// a poll happens to find the push slot empty. Deferring the event instead of
/// dropping it keeps the same at-most-one-push-per-window guarantee while
/// making the LAST state always the one that ships.
///
/// Distinguished from a response by carrying `push` instead of `id`, so the
/// hub can route it without guessing.
fn start_summary_push(socket: std::path::PathBuf) {
    std::thread::spawn(move || {
        let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&socket) else {
            return;
        };
        let subscribe = serde_json::json!({
            "id": "relay-push",
            "method": "events.subscribe",
            "params": { "subscriptions": PUSH_SUBSCRIPTIONS
                .iter()
                .map(|kind| serde_json::json!({ "type": kind }))
                .collect::<Vec<_>>() },
        });
        if writeln!(stream, "{subscribe}").is_err() {
            return;
        }
        let mut reader = std::io::BufReader::new(stream);
        // The server answers a bad subscription with an error line and closes.
        // Reading it here turns "pushes silently never happen" into a stated
        // reason — the failure mode that hid a rejected subscription until a
        // live run caught it.
        let mut ack = String::new();
        match reader.read_line(&mut ack) {
            // EOF with no answer at all: the server closed instead of
            // acknowledging. Reported rather than treated as success — logging
            // "subscription active" for a subscription that will never deliver
            // is the same silent-failure shape this ack check was added to
            // remove.
            Ok(0) => {
                crate::logging::peer_push_subscribe_failed("stream closed without an ack");
                return;
            }
            Ok(_) => {}
            Err(_) => return,
        }
        if ack.contains("\"error\"") {
            crate::logging::peer_push_subscribe_failed(ack.trim());
            return;
        }
        crate::logging::peer_push_subscribed(PUSH_SUBSCRIPTIONS.len());

        // The reader parks on the event stream; the pusher below wakes on the
        // debounce. Splitting them is what makes the trailing edge possible at
        // all — a single loop blocked in `lines()` cannot notice that a window
        // has expired with an event still owed.
        let debounce = PushDebounce::default();
        let reader_debounce = debounce.clone();
        std::thread::spawn(move || {
            for _ in reader.lines().map_while(Result::ok) {
                reader_debounce.note_event();
            }
            reader_debounce.note_closed();
        });

        loop {
            std::thread::sleep(SUMMARY_PUSH_DEBOUNCE);
            let Some(coalesced) = debounce.take_due() else {
                if debounce.is_finished() {
                    return;
                }
                continue;
            };
            let Ok(mut summary_stream) = std::os::unix::net::UnixStream::connect(&socket) else {
                continue;
            };
            let request = serde_json::json!({
                "id": "relay-push-summary",
                "method": "peers.summary",
                "params": {},
            });
            if writeln!(summary_stream, "{request}").is_err() {
                continue;
            }
            let mut line = String::new();
            if std::io::BufReader::new(summary_stream)
                .read_line(&mut line)
                .is_err()
            {
                continue;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            let Some(result) = value.get("result") else {
                continue;
            };
            let push = serde_json::json!({ "push": "peers.summary", "result": result });
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            if writeln!(out, "{push}").is_err() || out.flush().is_err() {
                return;
            }
            drop(out);
            crate::logging::peer_push_emitted(coalesced);
        }
    });
}

/// Trailing-edge debounce state, shared between the event reader and the
/// pusher (#4).
///
/// Split out of the thread bodies so the property that matters is testable
/// without sockets or sleeps: an event arriving inside a window is DEFERRED to
/// the end of it, never dropped. The leading-edge version this replaced could
/// only be wrong when a burst ended inside the window — which is what every
/// state change on a node that then goes quiet looks like.
#[derive(Clone, Default)]
struct PushDebounce {
    /// A push is owed.
    pending: Arc<AtomicBool>,
    /// The event stream ended; stop once nothing is owed.
    closed: Arc<AtomicBool>,
    /// Events folded into the owed push. Counted, not merely dropped: "5
    /// events became 1 push" is the evidence coalescing works, and its absence
    /// is the evidence it does not.
    ///
    /// Deliberately not synchronised with `pending`: a thread preempted between
    /// the two can attribute one event to the neighbouring push. That is ±1 on
    /// a diagnostic counter, and paying a lock on every event to make a log
    /// field exact would be a poor trade.
    coalesced: Arc<AtomicU64>,
}

impl PushDebounce {
    /// Reader side: this node changed.
    fn note_event(&self) {
        // Already owed a push? Then this event folds into it. That is the
        // coalescing — the difference from the leading-edge version is that
        // the fold is into a push that WILL happen, not one that already did.
        if self.pending.swap(true, Ordering::AcqRel) {
            self.coalesced.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Reader side: the event stream ended.
    fn note_closed(&self) {
        self.closed.store(true, Ordering::Release);
    }

    /// Pusher side: is a push owed now? Returns how many events folded into it.
    fn take_due(&self) -> Option<u64> {
        self.pending
            .swap(false, Ordering::AcqRel)
            .then(|| self.coalesced.swap(0, Ordering::Relaxed))
    }

    /// Pusher side: nothing more will ever be owed.
    ///
    /// Deliberately only meaningful AFTER `take_due` returned `None` — a final
    /// event that landed in the same window as the disconnect must still ship,
    /// and the outgoing pipe long outlives the subscription stream.
    fn is_finished(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// Fastest cadence at which state pushes leave a node. Not configurable: the
/// #224 review flagged per-kind cadence config as a footgun, and this is a
/// property of the payload rather than a preference — a snapshot coalesces,
/// so a faster rate would only resend what the next one already carries.
const SUMMARY_PUSH_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(1);

/// Events that mean "something on this node changed", so the hub is owed a
/// fresh summary.
///
/// Every one MUST be field-free. `pane.agent_status_changed` reads as the
/// obvious inclusion and is not usable: it requires a `pane_id`, so it scopes
/// to ONE pane where this needs the whole node. Including it makes the entire
/// `events.subscribe` invalid — the server rejects it, this thread exits, and
/// pushes silently never arrive, indistinguishable from a quiet node. That is
/// only caught at runtime, against a live server.
const PUSH_SUBSCRIPTIONS: [&str; 7] = [
    "workspace.created",
    "workspace.updated",
    "workspace.closed",
    "pane.created",
    "pane.closed",
    "pane.exited",
    "pane.agent_detected",
];

/// Wire names of the two methods that hold their connection open streaming.
///
/// Sequential relaying is right for the only caller today — a hub poll is one
/// request at a time — but a streaming method would hold the pipe forever and
/// starve every request behind it. Refuse loudly instead of hanging quietly.
/// Carrying these needs a demultiplexed channel, which is the push work, not
/// this seam.
const STREAMING_METHODS: [&str; 2] = ["events.subscribe", "pane.wait_for_output"];

fn relay_one_request(socket: &std::path::Path, request: &str) -> std::io::Result<()> {
    if let Some((id, code, message)) = relay_refusal(request) {
        return write_relay_error(&id, code, &message);
    }
    // Malformed input is the server's error to describe, not ours — pass it
    // through so the one implementation answers.
    let id = serde_json::from_str::<serde_json::Value>(request)
        .ok()
        .and_then(|parsed| {
            parsed
                .get("id")
                .and_then(|id| id.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();

    let mut stream = match std::os::unix::net::UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(err) => {
            return write_relay_error(
                &id,
                "no_local_server",
                &format!("no flk server on this node: {err}"),
            );
        }
    };
    stream.write_all(request.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    // Read to EOF rather than taking one line: the server closes the
    // connection when it is done, so this carries a single response without
    // assuming there is exactly one.
    let mut reader = std::io::BufReader::new(stream);
    let mut response = String::new();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    loop {
        response.clear();
        if reader.read_line(&mut response)? == 0 {
            break;
        }
        out.write_all(response.as_bytes())?;
    }
    out.flush()
}

fn write_relay_error(id: &str, code: &str, message: &str) -> std::io::Result<()> {
    let error = serde_json::json!({
        "id": id,
        "error": { "code": code, "message": message },
    });
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "{error}")?;
    out.flush()
}

fn print_peers_help() {
    eprintln!("usage: flk peers summary [--json]");
    eprintln!("       flk peers checkout-prepare --workspace <id> [--push] [--json]");
    eprintln!("       flk peers logs [--all] [--lines N] [--json]");
    eprintln!(
        "       flk peers relay [--push]  (stdin/stdout API relay, for `ssh <peer> flk peers relay`)"
    );
}

#[cfg(test)]
mod tests {
    use super::PushDebounce;

    /// #4, the whole bug in one assertion. A burst that ENDS inside the
    /// debounce window must still ship its final state.
    ///
    /// The leading-edge version pushed on the first event and dropped the
    /// rest, which is harmless while events keep arriving — the next one
    /// carries what the dropped ones would have said — and silently wrong the
    /// moment the node goes quiet. An agent goes working, then idle 300ms
    /// later, then nothing: the hub was told "working" and never corrected.
    #[test]
    fn the_last_event_of_a_burst_still_produces_a_push() {
        let debounce = PushDebounce::default();

        // A burst: three changes inside one window.
        debounce.note_event();
        debounce.note_event();
        debounce.note_event();

        // The window expires. One push, carrying all three.
        assert_eq!(
            debounce.take_due(),
            Some(2),
            "three events are one push, with two folded in"
        );

        // Nothing further owed while the node is quiet.
        assert_eq!(debounce.take_due(), None, "a quiet node pushes nothing");

        // The event that used to be lost: it lands after a push has already
        // gone out, and nothing else follows it.
        debounce.note_event();
        assert_eq!(
            debounce.take_due(),
            Some(0),
            "the trailing event of a burst is deferred, never dropped"
        );
    }

    /// A disconnect must not strand a push that was already owed — the
    /// outgoing pipe outlives the subscription stream, so the final state can
    /// still be delivered.
    #[test]
    fn a_close_racing_the_last_event_still_ships_it() {
        let debounce = PushDebounce::default();
        debounce.note_event();
        debounce.note_closed();

        assert_eq!(
            debounce.take_due(),
            Some(0),
            "an owed push survives the stream ending"
        );
        assert!(debounce.is_finished(), "and only then does the pusher stop");
    }

    /// Coalescing must reset per push, or the count becomes a running total
    /// and stops meaning "how many events this push represents".
    #[test]
    fn the_coalesced_count_is_per_push() {
        let debounce = PushDebounce::default();
        debounce.note_event();
        debounce.note_event();
        assert_eq!(debounce.take_due(), Some(1));

        debounce.note_event();
        debounce.note_event();
        debounce.note_event();
        assert_eq!(debounce.take_due(), Some(2), "not 3 — the count reset");
    }

    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn streaming_methods_are_refused_rather_than_left_to_hang() {
        // The relay is sequential, so a method that holds its connection open
        // would starve every request behind it. Silence would look like a slow
        // peer; the refusal names the reason.
        for method in STREAMING_METHODS {
            let request = format!(r#"{{"id":"r1","method":"{method}","params":{{}}}}"#);
            let (id, code, message) =
                relay_refusal(&request).expect("a streaming method is refused");
            assert_eq!(
                id, "r1",
                "the refusal is addressed to the request that caused it"
            );
            assert_eq!(code, "unsupported_over_relay");
            assert!(
                message.contains(method),
                "the refusal names the method: {message}"
            );
        }
    }

    #[test]
    fn ordinary_requests_are_carried_and_malformed_input_is_left_to_the_server() {
        assert!(
            relay_refusal(r#"{"id":"r1","method":"peers.summary","params":{}}"#).is_none(),
            "a non-streaming method is carried"
        );
        assert!(
            relay_refusal("not json at all").is_none(),
            "malformed input is not refused here — the server owns that error \
             message, and duplicating it would give two implementations to drift"
        );
        assert!(
            relay_refusal(r#"{"id":"r1"}"#).is_none(),
            "a request with no method is likewise the server's to reject"
        );
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
