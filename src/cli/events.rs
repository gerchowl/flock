#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stdout/stderr for humans and scripts"
)]
//! `flk events read` / `flk events wait` (ADR-0028 §5, #1029): the client
//! side of `events.read`. Both are one server-side long-poll per call, never
//! a client loop over `agent get`. `flk wait park` (#1031) registers a wait
//! with the same filter and returns at once.

use std::time::{Duration, Instant};

use serde_json::Value;

use super::exit;
use crate::api::client::ApiClient;
use crate::api::schema::{EventKind, EventsReadParams, Method, Request, WaitParkParams};

pub(super) const READ_USAGE: &str =
    "flk events read [--after CURSOR] [--kinds K1,K2] [--subject ID]... [--limit N] [--json]";
pub(super) const WAIT_USAGE: &str =
    "flk events wait [--kinds K1,K2] [--subject ID]... [--after CURSOR] --timeout DUR";
pub(super) const PARK_USAGE: &str =
    "flk wait park [--kinds K1,K2] [--subject ID]... --deadline DUR [--json]";

/// Slack on top of the server-side hold before the client stops reading.
const READ_GRACE: Duration = Duration::from_secs(10);

pub(super) fn run_events_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(String::as_str) {
        Some("read") => read(&args[1..]),
        Some("wait") => wait(&args[1..]),
        Some("help") => {
            println!("usage: {READ_USAGE}\n       {WAIT_USAGE}");
            Ok(exit::SUCCESS)
        }
        _ => {
            eprintln!("usage: {READ_USAGE}\n       {WAIT_USAGE}");
            Ok(exit::USAGE)
        }
    }
}

#[derive(Default)]
struct Flags {
    params: EventsReadParams,
    has_after: bool,
    timeout_ms: Option<u64>,
    json: bool,
}

/// A durable kind by its wire name; `message.queued` is read as
/// `message_queued`.
fn parse_kind(raw: &str) -> Result<EventKind, String> {
    [raw.to_owned(), raw.replace('.', "_")]
        .iter()
        .find_map(|name| serde_json::from_value(Value::String(name.clone())).ok())
        .ok_or_else(|| format!("unknown event kind: {raw}"))
}

fn parse_flags(args: &[String], allowed: &[&str]) -> Result<Flags, String> {
    let mut flags = Flags {
        json: super::output::json(),
        ..Flags::default()
    };
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        if !allowed.contains(&flag) {
            return Err(format!("unknown option: {flag}"));
        }
        if flag == "--json" {
            flags.json = true;
            index += 1;
            continue;
        }
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("{flag} needs a value"))?;
        let number = || {
            value
                .parse::<u64>()
                .map_err(|_| format!("invalid value for {flag}: {value}"))
        };
        match flag {
            "--after" => {
                flags.params.after = number()?;
                flags.has_after = true;
            }
            "--kinds" => {
                for kind in value.split(',').filter(|kind| !kind.is_empty()) {
                    flags.params.kinds.push(parse_kind(kind)?);
                }
            }
            "--subject" => flags.params.subjects.push(value.clone()),
            "--limit" => {
                flags.params.limit =
                    Some(u32::try_from(number()?).map_err(|_| format!("{flag} is too large"))?);
            }
            _ => {
                flags.timeout_ms = Some(
                    crate::detect::provider_limit::parse_duration(value)
                        .map_err(|reason| format!("invalid value for {flag}: {reason}"))?,
                );
            }
        }
        index += 2;
    }
    Ok(flags)
}

/// `flk wait park`: park the calling agent until the deadline, so it can
/// end its turn. State-changing, so an older server refuses it with exit 78
/// before it is sent (#868).
pub(super) fn park(args: &[String]) -> std::io::Result<i32> {
    let flags =
        parse_flags(args, &["--kinds", "--subject", "--deadline", "--json"]).and_then(|flags| {
            match flags.timeout_ms {
                Some(0) => Err("--deadline must be above 0".to_owned()),
                Some(_) => Ok(flags),
                None => Err("missing required --deadline".to_owned()),
            }
        });
    let flags = match flags {
        Ok(flags) => flags,
        Err(reason) => {
            eprintln!("{reason}\nusage: {PARK_USAGE}");
            return Ok(exit::USAGE);
        }
    };
    super::print_response(&super::send_request(&Request {
        id: "cli:wait:park".into(),
        method: Method::WaitPark(WaitParkParams {
            kinds: flags.params.kinds,
            subjects: flags.params.subjects,
            deadline_ms: flags.timeout_ms.unwrap_or_default(),
        }),
    })?)
}

fn events_read(params: EventsReadParams, timeout: Duration) -> std::io::Result<Value> {
    ApiClient::local()
        .request_value_with_timeout(
            &Request {
                id: "cli:events:read".into(),
                method: Method::EventsRead(params),
            },
            timeout,
        )
        .map_err(super::api_client_error_to_io)
}

fn report_gap(result: &Value) {
    if let Some(gap) = result.get("gap") {
        eprintln!(
            "gap: events {}..={} are no longer retained",
            gap["from"], gap["to"]
        );
    }
}

fn read(args: &[String]) -> std::io::Result<i32> {
    let flags = match parse_flags(
        args,
        &["--after", "--kinds", "--subject", "--limit", "--json"],
    ) {
        Ok(flags) => flags,
        Err(reason) => {
            eprintln!("{reason}\nusage: {READ_USAGE}");
            return Ok(exit::USAGE);
        }
    };
    let response = events_read(flags.params, READ_GRACE)?;
    let Some(result) = response.get("result") else {
        return super::print_response(&response);
    };
    if flags.json {
        println!("{result}");
        return Ok(exit::SUCCESS);
    }
    report_gap(result);
    for event in result["events"].as_array().into_iter().flatten() {
        println!("{event}");
    }
    eprintln!("next_after: {}", result["next_after"]);
    Ok(exit::SUCCESS)
}

/// The newest durable sequence, so a wait without `--after` sees only what
/// happens from now on. The server returns it as `data.head` when it refuses
/// a cursor past it. A server from before that field names it only at the
/// end of the message.
fn head_cursor() -> std::io::Result<Result<u64, Value>> {
    let response = events_read(
        EventsReadParams {
            after: u64::MAX,
            ..Default::default()
        },
        READ_GRACE,
    )?;
    Ok(cursor_ahead_head(&response).ok_or(response))
}

pub(super) fn cursor_ahead_head(response: &Value) -> Option<u64> {
    let error = &response["error"];
    if error["code"] != "cursor_ahead" {
        return None;
    }
    error["data"]["head"]
        .as_u64()
        .or_else(|| error["message"].as_str()?.rsplit(' ').next()?.parse().ok())
}

fn wait(args: &[String]) -> std::io::Result<i32> {
    let flags =
        parse_flags(args, &["--after", "--kinds", "--subject", "--timeout"]).and_then(|flags| {
            match flags.timeout_ms {
                Some(_) => Ok(flags),
                None => Err("missing required --timeout".to_owned()),
            }
        });
    let Flags {
        mut params,
        has_after,
        timeout_ms,
        ..
    } = match flags {
        Ok(flags) => flags,
        Err(reason) => {
            eprintln!("{reason}\nusage: {WAIT_USAGE}");
            return Ok(exit::USAGE);
        }
    };
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.unwrap_or_default());
    if !has_after {
        params.after = match head_cursor()? {
            Ok(head) => head,
            Err(response) => return super::print_response(&response),
        };
    }
    params.limit = Some(1);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        params.wait_ms = Some(left.as_millis().try_into().unwrap_or(u64::MAX));
        let started = Instant::now();
        let response = events_read(params.clone(), left + READ_GRACE)?;
        let Some(result) = response.get("result") else {
            return super::print_response(&response);
        };
        report_gap(result);
        if let Some(event) = result["events"].get(0) {
            println!("{event}");
            return Ok(exit::SUCCESS);
        }
        params.after = result["next_after"].as_u64().unwrap_or(params.after);
        if Instant::now() >= deadline {
            eprintln!(
                "timed out waiting for an event after cursor {}",
                params.after
            );
            return Ok(exit::TIMEOUT);
        }
        // A server configured to hold no reads answers at once: do not spin.
        if started.elapsed() < Duration::from_millis(100) {
            std::thread::sleep(Duration::from_millis(250).min(left));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn flags_parse_kinds_subjects_and_durations() {
        let flags = parse_flags(
            &args(&[
                "--kinds",
                "message.queued,pane_exited",
                "--subject",
                "a",
                "--subject",
                "b",
                "--after",
                "7",
                "--timeout",
                "5s",
            ]),
            &["--after", "--kinds", "--subject", "--timeout"],
        )
        .unwrap();
        assert_eq!(
            flags.params.kinds,
            [EventKind::MessageQueued, EventKind::PaneExited]
        );
        assert_eq!(flags.params.subjects, ["a", "b"]);
        assert_eq!((flags.params.after, flags.has_after), (7, true));
        assert_eq!(flags.timeout_ms, Some(5_000));
    }

    /// #1029: the head comes from `data.head`; the message is read only
    /// when an older server sent no such field.
    #[test]
    fn the_head_prefers_the_structured_field() {
        let refusal = |data: Value| {
            serde_json::json!({"error": {"code": "cursor_ahead", "data": data,
                "message": "cursor 9 is past this server's newest event 3; read from 0 or 3"}})
        };
        assert_eq!(
            cursor_ahead_head(&refusal(serde_json::json!({"head": 7}))),
            Some(7)
        );
        assert_eq!(cursor_ahead_head(&refusal(Value::Null)), Some(3));
        let other = serde_json::json!({"error": {"code": "invalid_params", "message": "x 3"}});
        assert_eq!(cursor_ahead_head(&other), None);
    }

    #[test]
    fn flags_refuse_unknown_kinds_and_options() {
        let allowed = ["--kinds", "--limit"];
        assert!(parse_flags(&args(&["--kinds", "agent.nope"]), &allowed).is_err());
        assert!(parse_flags(&args(&["--timeout", "5s"]), &allowed).is_err());
        assert!(parse_flags(&args(&["--limit"]), &allowed).is_err());
    }
}
