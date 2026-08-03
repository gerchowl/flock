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
            "--json" => index += 1,
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }
    if workspace_id.is_some() && cwd.is_some() {
        eprintln!("usage: flk worktree list [--workspace ID | --cwd PATH] [--json]");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:worktree:list".into(),
        method: Method::WorktreeList(WorktreeListParams { workspace_id, cwd }),
    })?)
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

    let Some(workspace_id) = workspace_id else {
        eprintln!(
            "usage: flk worktree kill --workspace ID [--dry-run] [--force] [--keep-branch] [--json]"
        );
        return Ok(2);
    };

    // Transport only. The merge gate, the #121 protected-branch tiers and the
    // branch deletion all live in the server's `worktree.kill` — they used to
    // live HERE, which meant anything reaching the socket or MCP directly got
    // the destructive half without them.
    let response = super::send_request(&Request {
        id: "cli:worktree:kill".into(),
        method: Method::WorktreeKill(WorktreeKillParams {
            workspace_id: workspace_id.clone(),
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
            "dirty_worktree_requires_force" => 4,
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
    eprintln!("  flk worktree list [--workspace ID | --cwd PATH] [--json]");
    eprintln!(
        "  flk worktree create [--workspace ID | --cwd PATH] [--branch NAME] [--base REF] [--path PATH] [--label TEXT] [--focus] [--no-focus] [--json]"
    );
    eprintln!(
        "  flk worktree open [--workspace ID | --cwd PATH] (--path PATH | --branch NAME) [--label TEXT] [--focus] [--no-focus] [--json]"
    );
    eprintln!("  flk worktree remove --workspace ID [--force] [--json]");
    eprintln!("  flk worktree kill --workspace ID [--dry-run] [--force] [--keep-branch] [--json]");
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
