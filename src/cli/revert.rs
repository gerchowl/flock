#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stdout/stderr for humans and scripts"
)]
//! `flk revert-run` CLI (#175 phase 5 / S3 commit 4, ops).
//!
//! Sends `revert.run` over the socket and pretty-prints the per-repo
//! result. Because the server owns the repo list (workspaces +
//! worktrees) this cannot reasonably be a client-side loop.

use crate::api::schema::{Method, Request, RevertRunParams};

pub(super) fn run_revert_run_command(args: &[String]) -> std::io::Result<i32> {
    let Some(first) = args.first().map(|arg| arg.as_str()) else {
        print_help();
        return Ok(2);
    };
    if matches!(first, "help" | "--help" | "-h") {
        print_help();
        return Ok(0);
    }
    let run_id = first.to_string();
    let mut json = false;
    let mut dry_run = false;
    for arg in &args[1..] {
        match arg.as_str() {
            "--json" => json = true,
            "--dry-run" => dry_run = true,
            other => {
                eprintln!("unknown option: {other}");
                print_help();
                return Ok(2);
            }
        }
    }
    let response = super::send_request(&Request {
        id: "cli:revert-run".into(),
        method: Method::RevertRun(RevertRunParams { run_id, dry_run }),
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
    let Some(results) = result.get("results").and_then(|v| v.as_array()) else {
        return super::print_response(&response);
    };
    let run_id = result.get("run_id").and_then(|v| v.as_str()).unwrap_or("?");
    let dry_run = result
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    println!(
        "revert-run {run_id}{}",
        if dry_run { " (dry-run)" } else { "" }
    );
    for row in results {
        let repo = row.get("repo_path").and_then(|v| v.as_str()).unwrap_or("?");
        let outcome = row.get("outcome").and_then(|v| v.as_str()).unwrap_or("?");
        let branch = row
            .get("revert_branch")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let commits = row
            .get("reverted_commits")
            .and_then(|v| v.as_array())
            .map(|arr| arr.len())
            .unwrap_or(0);
        println!("  {repo}: {outcome} · {commits} commit(s) · {branch}");
        if let Some(detail) = row.get("detail").and_then(|v| v.as_str()) {
            println!("      {detail}");
        }
        if let Some(pr) = row.get("pr_command").and_then(|v| v.as_str()) {
            println!("      $ {pr}");
        }
    }
    Ok(0)
}

fn print_help() {
    eprintln!("flk revert-run <run-id> [--dry-run] [--json]");
    eprintln!("  <run-id>    the id to revert (matches `^Agent-Run: <id>$` trailers)");
    eprintln!("  --dry-run   list matches + would-be branches; write nothing");
    eprintln!("  --json      print the raw JSON envelope");
}
