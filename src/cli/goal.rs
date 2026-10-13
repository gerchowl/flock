//! `flk goal` (ADR-0030 §1, G1b #1155): arm, read, pause, resume and stop an
//! agent's goal through the `goal.*` socket methods.
//!
//! `--agent TARGET` names the agent and defaults to `self`, the agent whose
//! pane runs the command. The writes are refused locally with exit 78 against
//! an older server (#868, `Method::needs_current_protocol`), and `status`
//! reaches it and gets exit 78 through `normalize`. `--json` prints the
//! response as the server sent it (ADR-0029 §3, §4).

use super::exit;
use crate::api::schema::{
    GoalArmParams, GoalPauseParams, GoalStopParams, GoalTargetParams, Method, Request,
};

const USAGE: &str = "\
usage:
  flk goal arm <goal> [--check CMD]... [--agent TARGET] [--json]
  flk goal status [--agent TARGET] [--json]
  flk goal pause [--minutes N] [--agent TARGET] [--json]
  flk goal resume [--agent TARGET] [--json]
  flk goal stop [--reason TEXT] [--agent TARGET] [--json]

--agent defaults to self, the agent whose pane runs the command. The operator
may change any agent's goal, an agent only its own. A pause without --minutes
lasts until resume.";

pub(super) fn run_goal_command(args: &[String]) -> std::io::Result<i32> {
    let Some(verb) = args.first().map(String::as_str) else {
        eprintln!("{USAGE}");
        return Ok(exit::USAGE);
    };
    if matches!(verb, "help" | "--help" | "-h") {
        println!("{USAGE}");
        return Ok(exit::SUCCESS);
    }
    let (method, json) = match parse(verb, &args[1..]) {
        Ok(parsed) => parsed,
        Err(problem) => {
            eprintln!("{problem}\n{USAGE}");
            return Ok(exit::USAGE);
        }
    };
    let id = format!("cli:goal:{verb}");
    let response = super::send_request(&Request { id, method })?;
    if json || response.get("error").is_some() {
        return super::print_response(&response);
    }
    print_goal(verb, &response["result"]);
    Ok(exit::SUCCESS)
}

/// The request a verb's arguments make, and whether `--json` was asked for.
fn parse(verb: &str, args: &[String]) -> Result<(Method, bool), String> {
    let (mut goal, mut checks, mut target) = (None, Vec::new(), String::new());
    let (mut minutes, mut reason, mut json) = (None, None, super::output::json());
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let mut value = || {
            rest.next()
                .cloned()
                .ok_or(format!("missing value for {arg}"))
        };
        match (verb, arg.as_str()) {
            (_, "--json") => json = true,
            (_, "--agent") => target = value()?,
            ("arm", "--check") => checks.push(value()?),
            ("pause", "--minutes") => {
                let text = value()?;
                let parsed = text
                    .parse()
                    .map_err(|_| format!("--minutes takes whole minutes, not {text:?}"));
                minutes = Some(parsed?);
            }
            ("stop", "--reason") => reason = Some(value()?),
            ("arm", text) if goal.is_none() && !text.starts_with("--") => {
                goal = Some(text.to_string())
            }
            (_, other) => return Err(format!("unexpected argument: {other}")),
        }
    }
    let method = match verb {
        "arm" => {
            let goal = goal.ok_or("flk goal arm needs the goal text")?;
            Method::GoalArm(GoalArmParams {
                target,
                goal,
                checks,
            })
        }
        "status" => Method::GoalStatus(GoalTargetParams { target }),
        "pause" => Method::GoalPause(GoalPauseParams { target, minutes }),
        "resume" => Method::GoalResume(GoalTargetParams { target }),
        "stop" => Method::GoalStop(GoalStopParams { target, reason }),
        other => return Err(format!("unknown goal verb: {other}")),
    };
    Ok((method, json))
}

/// The human view of a `goal` result: one line for a write, the record for
/// `status`.
fn print_goal(verb: &str, result: &serde_json::Value) {
    let agent = result["agent_id"].as_str().unwrap_or("?");
    let goal = &result["goal"];
    let text = goal["goal"].as_str().unwrap_or("");
    let state = match (&goal["met"], &goal["paused"]["until_ms"]) {
        (serde_json::Value::Bool(true), _) => "met".to_string(),
        _ if goal["paused"].is_null() => "armed".to_string(),
        (_, serde_json::Value::Null) => "paused until resumed".to_string(),
        (_, until_ms) => format!("paused until {until_ms} (Unix ms)"),
    };
    if verb != "status" {
        println!("goal {verb}: {agent}: {text} ({state})");
        return;
    }
    println!("agent:  {agent}\ngoal:   {text}\nstate:  {state}");
    for check in goal["checks"].as_array().into_iter().flatten() {
        println!("check:  {}", check.as_str().unwrap_or(""));
    }
    if let Some(status) = goal["status"].as_str() {
        println!("status: {status}");
    }
    for subtask in goal["subtasks"].as_array().into_iter().flatten() {
        let mark = if subtask["done"] == true { "x" } else { " " };
        println!(
            "[{mark}] {} {}",
            subtask["id"].as_str().unwrap_or(""),
            subtask["text"].as_str().unwrap_or("")
        );
    }
    if let Some(verdict) = goal.get("last_verdict") {
        println!(
            "last verdict: {} (rung {})",
            verdict["decision"].as_str().unwrap_or("?"),
            verdict["rung"]
        );
    }
}
