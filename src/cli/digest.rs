#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stdout/stderr for humans and scripts"
)]
//! `flk digest` CLI (#175 phase 5 / S3 commit 2).
//!
//! Renders a self-contained HTML digest from the durable event log via the
//! `digest.render` socket verb. Prints the `file://` path of the written
//! file on success (or the raw JSON envelope in `--json` mode).

use std::time::{SystemTime, UNIX_EPOCH};

use crate::api::schema::{DigestRenderParams, Method, Request};

pub(super) fn run_digest_command(args: &[String]) -> std::io::Result<i32> {
    if let Some(first) = args.first().map(|arg| arg.as_str()) {
        if matches!(first, "help" | "--help" | "-h") {
            print_digest_help();
            return Ok(0);
        }
    }
    let mut json = false;
    let mut path: Option<String> = None;
    let mut since_secs: Option<u64> = None;
    let mut idx = 0;
    while idx < args.len() {
        let arg = args[idx].as_str();
        match arg {
            "--json" => {
                json = true;
                idx += 1;
            }
            "--path" => {
                let Some(value) = args.get(idx + 1) else {
                    eprintln!("missing value for --path");
                    return Ok(2);
                };
                path = Some(value.clone());
                idx += 2;
            }
            "--since" => {
                let Some(value) = args.get(idx + 1) else {
                    eprintln!("missing value for --since");
                    return Ok(2);
                };
                let Some(secs) = parse_duration_secs(value) else {
                    eprintln!("invalid --since value: {value} (try 24h, 30m, 3600s)");
                    return Ok(2);
                };
                since_secs = Some(secs);
                idx += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                print_digest_help();
                return Ok(2);
            }
        }
    }

    let since_ms = since_secs.map(|secs| {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0);
        now_ms.saturating_sub(secs * 1_000)
    });

    let response = super::send_request(&Request {
        id: "cli:digest".into(),
        method: Method::DigestRender(DigestRenderParams {
            since_ms,
            path_template: None,
            path,
        }),
    })?;

    if json {
        return super::print_response(&response);
    }
    if response.get("error").is_some() {
        return super::print_response(&response);
    }
    let Some(result) = response.get("result") else {
        return super::print_response(&response);
    };
    let path = result
        .get("path")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if path.is_empty() {
        return super::print_response(&response);
    }
    let events = result
        .get("events_considered")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let date = result
        .get("generated_for_date")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    println!("file://{path}");
    eprintln!("digest for {date}: {events} event(s) considered");
    Ok(0)
}

/// Parses `24h`, `30m`, `3600s`, or `3600` (bare = seconds). Returns None
/// on anything else — no regex, no shell.
fn parse_duration_secs(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (digits, suffix) = trimmed.split_at(
        trimmed
            .find(|ch: char| !ch.is_ascii_digit())
            .unwrap_or(trimmed.len()),
    );
    let value: u64 = digits.parse().ok()?;
    match suffix {
        "" | "s" => Some(value),
        "m" => value.checked_mul(60),
        "h" => value.checked_mul(3_600),
        "d" => value.checked_mul(86_400),
        _ => None,
    }
}

fn print_digest_help() {
    eprintln!("flk digest [--since <duration>] [--path FILE] [--json]");
    eprintln!("  --since <dur>  keep only events younger than <dur>; supports 30m/24h/7d/3600s");
    eprintln!("  --path FILE    write to FILE instead of data_dir()/digest/<YYYY-MM-DD>.html");
    eprintln!("  --json         print the raw JSON envelope instead of the file path");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_understands_common_suffixes() {
        assert_eq!(parse_duration_secs("30"), Some(30));
        assert_eq!(parse_duration_secs("30s"), Some(30));
        assert_eq!(parse_duration_secs("30m"), Some(1_800));
        assert_eq!(parse_duration_secs("24h"), Some(86_400));
        assert_eq!(parse_duration_secs("7d"), Some(604_800));
        assert_eq!(parse_duration_secs("abc"), None);
        assert_eq!(parse_duration_secs("24hoursago"), None);
        assert_eq!(parse_duration_secs(""), None);
    }
}
