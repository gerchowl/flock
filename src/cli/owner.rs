#![expect(
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stderr usage/errors for humans and scripts"
)]
//! `flk owner policy show|adopt` (ADR-0035 §6.2, #1195).
//!
//! `show` reports the repo's owner policy as committed on its default branch,
//! the version the operator adopted, and whether a newer one is pending.
//! `adopt` is the operator verb that makes the current version apply. It is
//! state-changing for the #868 gate, so against a server without it it exits
//! 78 before sending, and a refusal of an agent caller exits 13
//! (`operator_only`, `exit::POLICY_REFUSED`). That refusal is cooperative only
//! until O19a (#1208, #1177): see ADR-0035 §6.3 point 6.

use crate::api::schema::{Method, OwnerPolicyParams, Request};

pub(super) const POLICY_USAGE: &str =
    "flk owner policy show [--repo PATH] | flk owner policy adopt <VERSION|none> [--repo PATH]";

pub(super) fn run_owner_command(args: &[String]) -> std::io::Result<i32> {
    match args {
        [group, rest @ ..] if group == "policy" => policy(rest),
        [help] if matches!(help.as_str(), "help" | "--help" | "-h") => {
            eprintln!("usage: {POLICY_USAGE}");
            Ok(super::exit::SUCCESS)
        }
        _ => {
            eprintln!("usage: {POLICY_USAGE}");
            Ok(super::exit::USAGE)
        }
    }
}

fn policy(args: &[String]) -> std::io::Result<i32> {
    let (verb, version, rest) = match args {
        [verb, rest @ ..] if verb == "show" => (verb, None, rest),
        [verb, version, rest @ ..] if verb == "adopt" && !version.starts_with("--") => {
            let version = (version != "none").then(|| version.clone());
            (verb, version, rest)
        }
        _ => {
            eprintln!("usage: {POLICY_USAGE}");
            return Ok(super::exit::USAGE);
        }
    };
    let repo = match rest {
        [] => std::env::current_dir()?,
        [flag, path] if flag == "--repo" => std::path::absolute(path)?,
        _ => {
            eprintln!("usage: {POLICY_USAGE}");
            return Ok(super::exit::USAGE);
        }
    };
    let params = OwnerPolicyParams {
        repo: repo.to_string_lossy().into_owned(),
        policy_version: version,
    };
    let (id, method) = if verb == "show" {
        ("cli:owner:policy:show", Method::OwnerPolicyShow(params))
    } else {
        ("cli:owner:policy:adopt", Method::OwnerPolicyAdopt(params))
    };
    let response = super::send_request(&Request {
        id: id.into(),
        method,
    })?;
    if verb == "show" {
        super::print_response(&response)
    } else {
        super::print_send_response(&response)
    }
}
