#![expect(
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stderr usage/errors for humans and scripts"
)]
//! `flk checks` CLI surface (#175 phase 4, C4 tail).
//!
//! Three verbs, mirroring the `checks.*` methods over the socket:
//! - `flk checks list` — enumerate the runner's script checks + built-in
//!   states.
//! - `flk checks ack <name>` — silence a check's next fire.
//! - `flk checks run <name>` — force a script check due right now.

use crate::api::schema::{ChecksNamedTarget, EmptyParams, Method, Request};

pub(super) fn run_checks_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_checks_help();
        return Ok(2);
    };
    match subcommand {
        "list" => checks_list(&args[1..]),
        "ack" => checks_ack(&args[1..]),
        "run" => checks_run(&args[1..]),
        "help" | "--help" | "-h" => {
            print_checks_help();
            Ok(0)
        }
        _ => {
            print_checks_help();
            Ok(2)
        }
    }
}

fn checks_list(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: flk checks list");
        return Ok(2);
    }
    super::print_response(&super::send_request(&Request {
        id: "cli:checks:list".into(),
        method: Method::ChecksList(EmptyParams::default()),
    })?)
}

fn checks_ack(args: &[String]) -> std::io::Result<i32> {
    let Some(name) = args.first() else {
        eprintln!("usage: flk checks ack <name>");
        return Ok(2);
    };
    if args.len() != 1 {
        eprintln!("usage: flk checks ack <name>");
        return Ok(2);
    }
    super::print_response(&super::send_request(&Request {
        id: "cli:checks:ack".into(),
        method: Method::ChecksAck(ChecksNamedTarget { name: name.clone() }),
    })?)
}

fn checks_run(args: &[String]) -> std::io::Result<i32> {
    let Some(name) = args.first() else {
        eprintln!("usage: flk checks run <name>");
        return Ok(2);
    };
    if args.len() != 1 {
        eprintln!("usage: flk checks run <name>");
        return Ok(2);
    }
    super::print_response(&super::send_request(&Request {
        id: "cli:checks:run".into(),
        method: Method::ChecksRun(ChecksNamedTarget { name: name.clone() }),
    })?)
}

fn print_checks_help() {
    eprintln!("flk checks commands:");
    eprintln!("  flk checks list                # enumerate runner + built-in state");
    eprintln!("  flk checks ack <name>          # silence the next fire");
    eprintln!("  flk checks run <name>          # force a script check due now");
}
