#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stdout/stderr for humans and scripts"
)]
//! `flk delegate list` (#960, ADR-0027 §2): the server's architect ->
//! delegate edges, grouped per architect. It is a projection of the
//! `delegate.list` verb and nothing else, so it and MCP `flock_delegate_list`
//! return the same rows.

use std::collections::BTreeMap;
use std::io;

use serde_json::Value;

use crate::api::schema::{DelegateListParams, DelegateRow, Method, Request};

use super::exit;

pub(super) const LIST_USAGE: &str = concat!(
    "flk delegate list [--mine | --architect <id> | --all] [--branch <b>] [--json]\n",
    "  the delegates an architect started, from the server's edge index; --mine (the\n",
    "  default) is the agent running this, --all is every architect.\n",
    "  --branch returns the edge that owns that branch"
);

/// The request a `flk delegate list` command line asks for, and `--json`.
pub(crate) fn parse_list_args(args: &[String]) -> Result<(DelegateListParams, bool), String> {
    let mut params = DelegateListParams {
        architect: "self".into(),
        branch: None,
        repo: None,
    };
    let mut json = super::output::json();
    let mut scope: Option<&str> = None;
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        let mut value = || {
            index += 1;
            args.get(index)
                .cloned()
                .ok_or_else(|| format!("missing value for {flag}"))
        };
        match flag {
            "--json" => json = true,
            "--branch" => params.branch = Some(value()?),
            "--mine" | "--all" | "--architect" => {
                if let Some(previous) = scope.replace(flag) {
                    return Err(format!("{flag} and {previous} cannot be combined"));
                }
                params.architect = match flag {
                    "--mine" => "self".into(),
                    "--all" => "all".into(),
                    _ => value()?,
                };
            }
            other => return Err(format!("unknown option: {other}")),
        }
        index += 1;
    }
    Ok((params, json))
}

pub(super) fn delegate_list(args: &[String]) -> io::Result<i32> {
    let (params, json) = match parse_list_args(args) {
        Ok(parsed) => parsed,
        Err(reason) => {
            eprintln!("{reason}\nusage: {LIST_USAGE}");
            return Ok(exit::USAGE);
        }
    };
    let response = super::send_request(&Request {
        id: "cli:delegate:list".into(),
        method: Method::DelegateList(params),
    })?;
    if response.get("error").is_some() {
        return super::print_response(&response);
    }
    let result = json_output(&response);
    if json {
        println!("{result}");
        return Ok(exit::SUCCESS);
    }
    let rows = rows(&result).map_err(io::Error::other)?;
    if rows.is_empty() {
        eprintln!("no open delegate edges");
    }
    print!("{}", render(&rows));
    Ok(exit::SUCCESS)
}

/// What `--json` prints: the verb's result as is, which is also exactly what
/// MCP `flock_delegate_list` hands back.
pub(crate) fn json_output(response: &Value) -> Value {
    response.get("result").cloned().unwrap_or(Value::Null)
}

/// The rows of a `delegate.list` result, as the CLI renders them.
pub(crate) fn rows(result: &Value) -> serde_json::Result<Vec<DelegateRow>> {
    serde_json::from_value(result["delegates"].clone())
}

/// One block per architect, in id order.
pub(crate) fn render(rows: &[DelegateRow]) -> String {
    let mut by_architect: BTreeMap<&str, Vec<&DelegateRow>> = BTreeMap::new();
    for row in rows {
        by_architect.entry(&row.architect).or_default().push(row);
    }
    let mut out = String::new();
    for (architect, rows) in by_architect {
        let orphaned = rows.iter().any(|row| row.state == "orphaned");
        let tag = if orphaned {
            " (orphaned: architect gone)"
        } else {
            ""
        };
        out.push_str(&format!("architect {architect}{tag}\n"));
        for row in rows {
            out.push_str(&format!("  {}\n", row_line(row)));
        }
    }
    out
}

/// The rows a session-start table shows before it points at the full list.
/// Small enough that the table fits Codex's default `additionalContextLimit`.
pub(crate) const SESSION_TABLE_ROWS: usize = 10;

/// The delegate table an architect gets back after a compaction or resume
/// (#962, ADR-0027 §4): its own edges, at most `max_rows` of them, then a
/// tail naming how many more there are. `None` when it has no edges, so a
/// session with nothing to recover gets no extra context at all.
pub(crate) fn session_table(rows: &[DelegateRow], max_rows: usize) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    let mut out = format!(
        "Your open delegates ({} edge{}, from `flk delegate list --mine`):\n",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    );
    for row in rows.iter().take(max_rows) {
        out.push_str(&format!("  {}\n", row_line(row)));
    }
    if rows.len() > max_rows {
        out.push_str(&format!(
            "  +{} more: flk delegate list --mine\n",
            rows.len() - max_rows
        ));
    }
    Some(out)
}

fn row_line(row: &DelegateRow) -> String {
    let mut fields = vec![
        row.name.clone().unwrap_or_else(|| row.delegate.clone()),
        row.state.clone(),
    ];
    if let Some(status) = row.agent_status {
        let status = serde_json::to_value(status)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_default();
        let age = row.status_age_secs.map(age).unwrap_or_default();
        fields.push(format!("{status} {age}").trim_end().to_string());
    }
    fields.extend(row.harness.clone());
    fields.extend(row.branch.clone());
    fields.extend(
        row.pr
            .as_ref()
            .map(|pr| format!("PR #{} {}", pr.number, pr.state)),
    );
    fields.push(format!("edge {}", age(row.edge_age_secs)));
    fields.extend(row.label.clone());
    fields.join("  ")
}

fn age(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{AgentStatus, DelegationVia, PrInfo};

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    fn row(architect: &str, delegate: &str, state: &str) -> DelegateRow {
        DelegateRow {
            architect: architect.into(),
            delegate: delegate.into(),
            name: None,
            via: DelegationVia::Start,
            state: state.into(),
            label: None,
            harness: None,
            agent_status: None,
            status_age_secs: None,
            edge_age_secs: 90,
            workspace_id: "w1".into(),
            pane_id: None,
            worktree: None,
            branch: None,
            repo: None,
            pr: None,
            questions: None,
        }
    }

    #[test]
    fn the_scope_flags_pick_the_architect_and_refuse_to_combine() {
        let architect = |words: &[&str]| parse_list_args(&args(words)).map(|(p, _)| p.architect);
        assert_eq!(architect(&[]), Ok("self".into()));
        assert_eq!(architect(&["--mine"]), Ok("self".into()));
        assert_eq!(architect(&["--all"]), Ok("all".into()));
        assert_eq!(architect(&["--architect", "agent_a"]), Ok("agent_a".into()));
        assert!(architect(&["--all", "--mine"]).is_err());
        assert!(architect(&["--architect"]).is_err());
        assert!(architect(&["--bogus"]).is_err());
        let (params, json) = parse_list_args(&args(&["--branch", "feat/x", "--json"])).unwrap();
        assert_eq!(params.branch.as_deref(), Some("feat/x"));
        assert!(json);
    }

    #[test]
    fn render_draws_one_block_per_architect_and_marks_an_orphaned_one() {
        let mut worker = row("agent_lead", "agent_sub", "live");
        worker.name = Some("bunny-614".into());
        worker.agent_status = Some(AgentStatus::Idle);
        worker.status_age_secs = Some(30);
        worker.branch = Some("feat/614".into());
        worker.pr = Some(PrInfo {
            number: 624,
            state: "closed".into(),
        });
        worker.state = "closed, not merged".into();
        let rows = [
            worker,
            row("agent_sub", "agent_leaf", "gone"),
            row("agent_other", "agent_b", "orphaned"),
        ];
        assert_eq!(
            render(&rows),
            "architect agent_lead\n\
             \x20 bunny-614  closed, not merged  idle 30s  feat/614  PR #624 closed  edge 1m\n\
             architect agent_other (orphaned: architect gone)\n\
             \x20 agent_b  orphaned  edge 1m\n\
             architect agent_sub\n\
             \x20 agent_leaf  gone  edge 1m\n"
        );
    }

    #[test]
    fn the_session_table_is_absent_without_edges_and_bounded_with_a_tail() {
        assert_eq!(session_table(&[], 3), None);
        let two = [row("a", "d1", "live"), row("a", "d2", "gone")];
        assert_eq!(
            session_table(&two, 3).as_deref(),
            Some(
                "Your open delegates (2 edges, from `flk delegate list --mine`):\n\
                 \x20 d1  live  edge 1m\n\
                 \x20 d2  gone  edge 1m\n"
            )
        );
        let five: Vec<_> = (1..=5)
            .map(|n| row("a", &format!("d{n}"), "live"))
            .collect();
        let table = session_table(&five, 3).unwrap();
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 1 + 3 + 1, "{table}");
        assert_eq!(lines[3], "  d3  live  edge 1m");
        assert_eq!(lines[4], "  +2 more: flk delegate list --mine");
    }
}
