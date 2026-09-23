#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stdout/stderr for humans and scripts"
)]
use crate::api::schema::{
    Method, Request, WorktreeCreateParams, WorktreeKillParams, WorktreeListParams,
    WorktreeOpenParams, WorktreeRemoveParams,
};

pub(super) fn run_worktree_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_worktree_help();
        return Ok(2);
    };

    match subcommand {
        "list" => worktree_list(&args[1..]),
        "create" => worktree_create(&args[1..]),
        "open" => worktree_open(&args[1..]),
        "remove" => worktree_remove(&args[1..]),
        "kill" => worktree_kill(&args[1..]),
        "quarantine-list" => worktree_quarantine_list(&args[1..]),
        "unquarantine" => worktree_unquarantine(&args[1..]),
        "help" | "--help" | "-h" => {
            print_worktree_help();
            Ok(0)
        }
        _ => {
            print_worktree_help();
            Ok(2)
        }
    }
}

/// #175 S2 read-only listing: enumerate every quarantined worktree under
/// the current session's data dir. Prints one path per line for scripts.
fn worktree_quarantine_list(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: flk worktree quarantine-list");
        return Ok(2);
    }
    match crate::worktree::list_quarantined_worktrees() {
        Ok(paths) => {
            for path in paths {
                println!("{}", path.display());
            }
            Ok(0)
        }
        Err(err) => {
            eprintln!("{err}");
            Ok(1)
        }
    }
}

/// #175 S2: `flk worktree unquarantine <path> <destination>` — move a
/// quarantined checkout back onto the operator's chosen path. Never
/// deletes anything; strict `git worktree move`.
fn worktree_unquarantine(args: &[String]) -> std::io::Result<i32> {
    if args.len() != 2 {
        eprintln!("usage: flk worktree unquarantine <quarantined-path> <destination>");
        return Ok(2);
    }
    let src = std::path::PathBuf::from(&args[0]);
    let dst = std::path::PathBuf::from(&args[1]);
    match crate::worktree::unquarantine_worktree(&src, &dst) {
        Ok(()) => {
            println!("moved {} -> {}", src.display(), dst.display());
            Ok(0)
        }
        Err(err) => {
            eprintln!("{err}");
            Ok(1)
        }
    }
}

fn worktree_list(args: &[String]) -> std::io::Result<i32> {
    let mut workspace_id = None;
    let mut cwd = None;
    let mut json = false;
    let mut scan = false;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--workspace" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --workspace");
                    return Ok(2);
                };
                workspace_id = Some(super::normalize_workspace_id(value));
                index += 2;
            }
            "--cwd" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --cwd");
                    return Ok(2);
                };
                cwd = Some(normalize_path_arg(value)?);
                index += 2;
            }
            "--scan" => {
                scan = true;
                index += 1;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }
    if workspace_id.is_some() && cwd.is_some() {
        eprintln!("usage: flk worktree list [--workspace ID | --cwd PATH] [--scan] [--json]");
        return Ok(2);
    }

    let response = super::send_request(&Request {
        id: "cli:worktree:list".into(),
        method: Method::WorktreeList(WorktreeListParams {
            workspace_id,
            cwd,
            scan,
        }),
    })?;
    if json {
        return super::print_response(&response);
    }
    if response.get("error").is_some() {
        eprintln!("{}", serde_json::to_string(&response).unwrap_or_default());
        return Ok(1);
    }
    print_worktree_table(&response, scan);
    Ok(0)
}

/// Render the list the way an operator reads it: oldest work first, one line
/// per checkout, with the gate's verdict when it was asked for (#396).
///
/// The server already ordered the rows; this prints them in the order they
/// arrived rather than re-sorting, so the CLI and the TUI picker cannot
/// disagree about which checkout is the stalest.
fn print_worktree_table(response: &serde_json::Value, scan: bool) {
    let rows = response
        .pointer("/result/worktrees")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        println!("no worktrees");
        return;
    }

    let now = crate::worktree::current_unix_time();
    let lines: Vec<WorktreeRow> = rows.iter().map(WorktreeRow::from_json).collect();
    let age_width = lines
        .iter()
        .map(|row| row.age(now).chars().count())
        .max()
        .unwrap_or(3)
        .max(3);
    let branch_width = lines
        .iter()
        .map(|row| row.branch.chars().count())
        .max()
        .unwrap_or(6)
        .max(6);
    let where_width = lines
        .iter()
        .map(|row| row.location.chars().count())
        .max()
        .unwrap_or(5)
        .max(5);
    let verdict_width = lines
        .iter()
        .map(|row| row.verdict.chars().count())
        .max()
        .unwrap_or(7)
        .max(7);

    let (header_age, header_branch, header_where) = ("AGE", "BRANCH", "WHERE");
    if scan {
        println!(
            "{header_age:>age_width$}  {header_branch:<branch_width$}  {header_where:<where_width$}  {:<verdict_width$}  PATH",
            "VERDICT"
        );
    } else {
        println!(
            "{header_age:>age_width$}  {header_branch:<branch_width$}  {header_where:<where_width$}  PATH"
        );
    }
    for row in &lines {
        let age = row.age(now);
        let branch = &row.branch;
        let location = &row.location;
        let path = &row.path;
        if scan {
            let verdict = &row.verdict;
            println!(
                "{age:>age_width$}  {branch:<branch_width$}  {location:<where_width$}  {verdict:<verdict_width$}  {path}"
            );
            if let Some(evidence) = &row.evidence {
                println!("{:age_width$}  \u{21b3} {evidence}", "");
            }
        } else {
            println!(
                "{age:>age_width$}  {branch:<branch_width$}  {location:<where_width$}  {path}"
            );
        }
    }
    println!();
    if scan {
        println!("merged rows are safe to delete: flk worktree kill --path PATH");
    } else {
        println!("AGE is the last commit on the branch. For the merge verdict, rerun with --scan.");
    }
}

/// One `worktree.list` row, reduced to what the table prints.
struct WorktreeRow {
    last_commit_at: Option<i64>,
    branch: String,
    location: String,
    verdict: String,
    evidence: Option<String>,
    path: String,
}

impl WorktreeRow {
    fn from_json(row: &serde_json::Value) -> Self {
        let text = |value: Option<&serde_json::Value>| {
            value
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let branch = text(row.get("branch"));
        let open = text(row.get("open_workspace_id"));
        let linked = row
            .get("is_linked_worktree")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let verdict = row.get("kill_verdict");
        Self {
            last_commit_at: row.get("last_commit_at").and_then(|value| value.as_i64()),
            branch: if branch.is_empty() {
                "(detached)".to_string()
            } else {
                branch
            },
            location: if !linked {
                "main".to_string()
            } else if open.is_empty() {
                "-".to_string()
            } else {
                open
            },
            verdict: match verdict {
                None => "-".to_string(),
                Some(verdict) => {
                    let flag = |key: &str| {
                        verdict
                            .get(key)
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false)
                    };
                    // A gate that ran out of clock is UNKNOWN, not unmerged —
                    // the safe degradation must not print as a judgement.
                    if flag("timed_out") {
                        "unknown".to_string()
                    } else if flag("protected") {
                        format!("{} (protected)", text(verdict.get("verdict")))
                    } else {
                        text(verdict.get("verdict"))
                    }
                }
            },
            evidence: verdict
                .and_then(|verdict| verdict.get("evidence"))
                .and_then(|value| value.as_str())
                .map(str::to_string),
            path: text(row.get("path")),
        }
    }

    /// Compact age, or `?` when git could not date the branch — never a
    /// number nothing measured.
    fn age(&self, now: i64) -> String {
        self.last_commit_at
            .map(|at| crate::worktree::relative_age_label(now, at))
            .unwrap_or_else(|| "?".to_string())
    }
}

fn worktree_create(args: &[String]) -> std::io::Result<i32> {
    let mut workspace_id = None;
    let mut cwd = None;
    let mut branch = None;
    let mut base = None;
    let mut path = None;
    let mut label = None;
    let mut focus = false;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--workspace" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --workspace");
                    return Ok(2);
                };
                workspace_id = Some(super::normalize_workspace_id(value));
                index += 2;
            }
            "--cwd" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --cwd");
                    return Ok(2);
                };
                cwd = Some(normalize_path_arg(value)?);
                index += 2;
            }
            "--branch" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --branch");
                    return Ok(2);
                };
                branch = Some(value.clone());
                index += 2;
            }
            "--base" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --base");
                    return Ok(2);
                };
                base = Some(value.clone());
                index += 2;
            }
            "--path" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --path");
                    return Ok(2);
                };
                path = Some(normalize_path_arg(value)?);
                index += 2;
            }
            "--label" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --label");
                    return Ok(2);
                };
                label = Some(value.clone());
                index += 2;
            }
            "--focus" => {
                focus = true;
                index += 1;
            }
            "--no-focus" => {
                focus = false;
                index += 1;
            }
            "--json" => index += 1,
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }
    if workspace_id.is_some() && cwd.is_some() {
        eprintln!(
            "usage: flk worktree create [--workspace ID | --cwd PATH] [--branch NAME] [--base REF] [--path PATH] [--label TEXT] [--focus] [--no-focus] [--json]"
        );
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:worktree:create".into(),
        method: Method::WorktreeCreate(WorktreeCreateParams {
            workspace_id,
            cwd,
            branch,
            base,
            path,
            label,
            focus,
        }),
    })?)
}

fn worktree_open(args: &[String]) -> std::io::Result<i32> {
    let mut workspace_id = None;
    let mut cwd = None;
    let mut path = None;
    let mut branch = None;
    let mut label = None;
    let mut focus = false;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--workspace" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --workspace");
                    return Ok(2);
                };
                workspace_id = Some(super::normalize_workspace_id(value));
                index += 2;
            }
            "--cwd" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --cwd");
                    return Ok(2);
                };
                cwd = Some(normalize_path_arg(value)?);
                index += 2;
            }
            "--path" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --path");
                    return Ok(2);
                };
                path = Some(normalize_path_arg(value)?);
                index += 2;
            }
            "--branch" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --branch");
                    return Ok(2);
                };
                branch = Some(value.clone());
                index += 2;
            }
            "--label" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --label");
                    return Ok(2);
                };
                label = Some(value.clone());
                index += 2;
            }
            "--focus" => {
                focus = true;
                index += 1;
            }
            "--no-focus" => {
                focus = false;
                index += 1;
            }
            "--json" => index += 1,
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }
    if workspace_id.is_some() && cwd.is_some() {
        eprintln!(
            "usage: flk worktree open [--workspace ID | --cwd PATH] (--path PATH | --branch NAME) [--label TEXT] [--focus] [--no-focus] [--json]"
        );
        return Ok(2);
    }
    if path.is_some() == branch.is_some() {
        eprintln!(
            "usage: flk worktree open [--workspace ID | --cwd PATH] (--path PATH | --branch NAME) [--label TEXT] [--focus] [--no-focus] [--json]"
        );
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:worktree:open".into(),
        method: Method::WorktreeOpen(WorktreeOpenParams {
            workspace_id,
            cwd,
            path,
            branch,
            label,
            focus,
        }),
    })?)
}

fn worktree_remove(args: &[String]) -> std::io::Result<i32> {
    let mut workspace_id = None;
    let mut force = false;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--workspace" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --workspace");
                    return Ok(2);
                };
                workspace_id = Some(super::normalize_workspace_id(value));
                index += 2;
            }
            "--force" => {
                force = true;
                index += 1;
            }
            "--json" => index += 1,
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    let Some(workspace_id) = workspace_id else {
        eprintln!("usage: flk worktree remove --workspace ID [--force] [--json]");
        return Ok(2);
    };

    super::print_response(&super::send_request(&Request {
        id: "cli:worktree:remove".into(),
        method: Method::WorktreeRemove(WorktreeRemoveParams {
            workspace_id,
            force,
        }),
    })?)
}

/// Kill a linked worktree workspace through the same merge gate as the TUI's
/// "Kill worktree & branch": evidence required before the local branch dies.
/// The gate functions are the single source of truth shared with the TUI.
fn worktree_kill(args: &[String]) -> std::io::Result<i32> {
    let mut workspace_id = None;
    let mut path = None;
    let mut dry_run = false;
    let mut force = false;
    let mut keep_branch = false;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--workspace" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --workspace");
                    return Ok(2);
                };
                workspace_id = Some(super::normalize_workspace_id(value));
                index += 2;
            }
            "--path" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --path");
                    return Ok(2);
                };
                path = Some(normalize_path_arg(value)?);
                index += 2;
            }
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            "--force" => {
                force = true;
                index += 1;
            }
            "--keep-branch" => {
                keep_branch = true;
                index += 1;
            }
            "--json" => index += 1,
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    if workspace_id.is_some() == path.is_some() {
        eprintln!(
            "usage: flk worktree kill (--workspace ID | --path PATH) [--dry-run] [--force] [--keep-branch] [--json]"
        );
        return Ok(2);
    }

    // Transport only. The merge gate, the #121 protected-branch tiers and the
    // branch deletion all live in the server's `worktree.kill` — they used to
    // live HERE, which meant anything reaching the socket or MCP directly got
    // the destructive half without them.
    let response = super::send_request(&Request {
        id: "cli:worktree:kill".into(),
        method: Method::WorktreeKill(WorktreeKillParams {
            workspace_id,
            path,
            force,
            keep_branch,
            dry_run,
        }),
    })?;

    if let Some(error) = response.get("error") {
        println!("{response}");
        let code = error.get("code").and_then(|v| v.as_str()).unwrap_or("");
        return Ok(match code {
            "not_linked_worktree" | "workspace_not_found" => 2,
            // 4 is the retryable class: the same call with --force clears it.
            "dirty_worktree_requires_force" | "submodule_worktree_requires_force" => 4,
            _ => 1,
        });
    }

    let result = response.pointer("/result").cloned().unwrap_or_default();
    println!("{result}");
    let merged = result
        .get("merged")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if dry_run {
        // Unchanged contract: 0 when the gate would pass, 3 when it would not.
        return Ok(if merged { 0 } else { 3 });
    }
    Ok(0)
}

fn print_worktree_help() {
    eprintln!("flk worktree commands:");
    eprintln!("  flk worktree list [--workspace ID | --cwd PATH] [--scan] [--json]");
    eprintln!(
        "  flk worktree create [--workspace ID | --cwd PATH] [--branch NAME] [--base REF] [--path PATH] [--label TEXT] [--focus] [--no-focus] [--json]"
    );
    eprintln!(
        "  flk worktree open [--workspace ID | --cwd PATH] (--path PATH | --branch NAME) [--label TEXT] [--focus] [--no-focus] [--json]"
    );
    eprintln!("  flk worktree remove --workspace ID [--force] [--json]");
    eprintln!(
        "  flk worktree kill (--workspace ID | --path PATH) [--dry-run] [--force] [--keep-branch] [--json]"
    );
    eprintln!("  flk worktree quarantine-list");
    eprintln!("  flk worktree unquarantine <quarantined-path> <destination>");
}

fn normalize_path_arg(value: &str) -> std::io::Result<String> {
    let path = crate::worktree::expand_tilde_path(value);
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(absolute.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::WorktreeRow;

    fn row(json: serde_json::Value) -> WorktreeRow {
        WorktreeRow::from_json(&json)
    }

    #[test]
    fn a_row_with_no_verdict_reads_as_unscanned_not_as_unmerged() {
        // #396's honesty requirement, at the surface an operator reads: a
        // plain list has asked nobody anything, and must not render a column
        // that looks like an answer.
        let row = row(serde_json::json!({
            "path": "/w/flock/feature",
            "branch": "feature/x",
            "is_linked_worktree": true,
        }));
        assert_eq!(row.verdict, "-");
        assert_eq!(row.evidence, None);
        assert_eq!(row.location, "-");
        assert_eq!(row.age(1_000), "?", "an undated row shows no number");
    }

    #[test]
    fn a_timed_out_gate_reads_as_unknown_not_as_not_merged() {
        // The gate degrades to `not_merged` when it runs out of clock, which
        // is the safe action but the wrong SENTENCE — it sends the operator to
        // go check a PR when the truth is that nothing was asked.
        let row = row(serde_json::json!({
            "path": "/w/flock/feature",
            "branch": "feature/x",
            "is_linked_worktree": true,
            "kill_verdict": {"verdict": "not_merged", "protected": false, "timed_out": true},
        }));
        assert_eq!(row.verdict, "unknown");
    }

    #[test]
    fn a_protected_branch_says_so_next_to_its_verdict() {
        // The gate calls the default branch merged, trivially. Printing that
        // alone would read as "safe to delete" for the one branch that never
        // is (#121).
        let row = row(serde_json::json!({
            "path": "/w/flock",
            "branch": "main",
            "is_linked_worktree": false,
            "open_workspace_id": "w_1",
            "last_commit_at": 1_000,
            "kill_verdict": {
                "verdict": "merged",
                "evidence": "contained in origin/main",
                "protected": true,
                "timed_out": false,
            },
        }));
        assert_eq!(row.verdict, "merged (protected)");
        assert_eq!(row.evidence.as_deref(), Some("contained in origin/main"));
        assert_eq!(row.location, "main");
        assert_eq!(row.age(1_000 + 40 * 86_400), "5w");
    }

    #[test]
    fn an_open_checkout_names_the_workspace_holding_it() {
        let row = row(serde_json::json!({
            "path": "/w/flock/feature",
            "branch": "feature/x",
            "is_linked_worktree": true,
            "open_workspace_id": "w_3",
        }));
        assert_eq!(row.location, "w_3");
    }

    #[test]
    fn a_detached_checkout_is_named_rather_than_left_blank() {
        let row = row(serde_json::json!({
            "path": "/w/flock/detached",
            "is_linked_worktree": true,
        }));
        assert_eq!(row.branch, "(detached)");
    }
}
