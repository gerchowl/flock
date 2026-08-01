#![expect(
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stderr for humans and scripts"
)]
//! `flk fleet` CLI (#175 S3 commit 3, US-9).
//!
//! Three verbs mirroring the socket:
//! - `flk fleet pause [--reason TEXT]` — halt the SCHEDULER and MESSAGE
//!   DELIVERY (persistent, survives restarts). Does NOT gate human input.
//! - `flk fleet resume` — start ticking again on the next scheduled tick.
//! - `flk fleet status` — read the persisted pause switch.

use crate::api::schema::{EmptyParams, FleetPauseParams, Method, Request};

pub(super) fn run_fleet_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_fleet_help();
        return Ok(2);
    };
    match subcommand {
        "pause" => fleet_pause(&args[1..]),
        "resume" => fleet_resume(&args[1..]),
        "status" => fleet_status(&args[1..]),
        "help" | "--help" | "-h" => {
            print_fleet_help();
            Ok(0)
        }
        _ => {
            print_fleet_help();
            Ok(2)
        }
    }
}

fn fleet_pause(args: &[String]) -> std::io::Result<i32> {
    let mut reason: Option<String> = None;
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--reason" => {
                let Some(value) = args.get(idx + 1) else {
                    eprintln!("missing value for --reason");
                    return Ok(2);
                };
                reason = Some(value.clone());
                idx += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }
    super::print_response(&super::send_request(&Request {
        id: "cli:fleet:pause".into(),
        method: Method::FleetPause(FleetPauseParams { reason }),
    })?)
}

fn fleet_resume(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: flk fleet resume");
        return Ok(2);
    }
    super::print_response(&super::send_request(&Request {
        id: "cli:fleet:resume".into(),
        method: Method::FleetResume(EmptyParams::default()),
    })?)
}

fn fleet_status(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: flk fleet status");
        return Ok(2);
    }
    super::print_response(&super::send_request(&Request {
        id: "cli:fleet:status".into(),
        method: Method::FleetStatus(EmptyParams::default()),
    })?)
}

fn print_fleet_help() {
    eprintln!("flk fleet commands (US-9 fleet pause):");
    eprintln!("  flk fleet pause [--reason TEXT]  # halt the scheduler + message delivery");
    eprintln!("  flk fleet resume                 # start ticking again on the next tick");
    eprintln!("  flk fleet status                 # read the persisted pause switch");
    eprintln!();
    eprintln!(
        "Pause halts the SCHEDULER and MESSAGE DELIVERY, not human agency:\n\
         keystrokes, `agent.send`, and human-invoked `flk agent fork` /\n\
         `flk msg send` still work. On resume the scheduler starts ticking\n\
         again — missed cron fires collapse to at most one fire."
    );
}
