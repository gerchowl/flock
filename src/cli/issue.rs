#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stdout/stderr for humans and scripts"
)]
//! `flk issue` — drop an issue into any repo the token can reach (#371).
//!
//! Thin adapter, the same shape as `flk report` (ADR-0010 decision 1): it
//! gathers inputs, hands them to the pure `crate::github::drop`, and decides
//! what to do with the result. The rules live in `crate::github`; the TUI
//! entry point is a second adapter over the same functions rather than a
//! second implementation.
//!
//! **Nothing is filed without an explicit `--file-it`.** The default composes
//! and prints. That follows `flk report`'s posture, and matters more here
//! because this path actually writes: a dry run that accidentally files an
//! issue into someone else's repository cannot be taken back.

use crate::github::drop::{self, Draft, DropError};
use crate::github::graphql::GraphQlErrorKind;
use crate::github::issues::{self, NewIssue, RepoMeta};
use crate::github::repos::{self, Provenance};

pub(super) fn run_issue_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(|arg| arg.as_str()) {
        Some("repos") => run_repos(&args[1..]),
        Some("drop") => run_drop(&args[1..]),
        Some("help" | "--help" | "-h") | None => {
            print_usage();
            Ok(i32::from(args.is_empty()) * 2)
        }
        Some(other) => {
            eprintln!("unknown subcommand {other:?}");
            print_usage();
            Ok(2)
        }
    }
}

fn print_usage() {
    eprintln!("usage: flk issue <command>");
    eprintln!();
    eprintln!("  repos                     list every repo this token can file into");
    eprintln!("  drop --repo owner/name    compose an issue for that repo");
    eprintln!();
    eprintln!("drop options:");
    eprintln!("  --repo owner/name  destination (required)");
    eprintln!("  --title <text>     issue title (required)");
    eprintln!("  --body-file <path> body markdown; omit to open $EDITOR on a skeleton");
    eprintln!("  --label <name>     add a label; repeatable");
    eprintln!("  --type <name>      issue type, when the org configures them");
    eprintln!("  --file-it          actually file it (default: compose and print)");
    eprintln!();
    eprintln!("nothing is filed without --file-it.");
}

/// Render a transport failure with the remedy that actually applies.
///
/// `Forbidden` is kept distinct from `Auth` deliberately: the operator's fix
/// for one is "your token cannot write here", and for the other "log in
/// again". Collapsing them sends someone to re-authenticate a token that was
/// never the problem.
fn explain(kind: GraphQlErrorKind) -> String {
    match kind {
        GraphQlErrorKind::NoToken => {
            "no GitHub token — set GH_TOKEN or run `gh auth login`".to_string()
        }
        GraphQlErrorKind::Transport => "could not reach api.github.com".to_string(),
        GraphQlErrorKind::RateLimited => "GitHub rate-limited this token".to_string(),
        GraphQlErrorKind::Auth => {
            "GitHub rejected the token — run `gh auth login` to refresh it".to_string()
        }
        GraphQlErrorKind::Forbidden => {
            "the token authenticated but cannot write here — it needs the `repo` scope \
             (classic) or `Issues: read & write` (fine-grained) on this repository"
                .to_string()
        }
        GraphQlErrorKind::GraphQl => "GitHub refused the query".to_string(),
    }
}

/// Repositories this machine already knows about, for ranking.
///
/// Best-effort: the directory is a shortcut, so failing to resolve local
/// context degrades the ordering and never the reachability.
fn local_repos() -> Vec<String> {
    crate::report::url::baked_repo().into_iter().collect()
}

fn run_repos(args: &[String]) -> std::io::Result<i32> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return Ok(0);
    }
    let entries = match repos::fetch_directory() {
        Ok(entries) => entries,
        Err(kind) => {
            eprintln!("error: {}", explain(kind));
            return Ok(1);
        }
    };
    let ranked = repos::rank(&entries, &local_repos(), &[]);
    for repo in &ranked {
        let tier = match repo.provenance {
            Provenance::LocalCheckout => "local",
            Provenance::SeenInFleet => "fleet",
            Provenance::Reachable => "",
        };
        println!("{:<6} {}", tier, repo.name_with_owner);
    }
    println!();
    println!("{} filable repositories.", ranked.len());
    Ok(0)
}

#[derive(Debug, Default)]
struct DropOptions {
    repo: Option<String>,
    title: Option<String>,
    body_file: Option<String>,
    labels: Vec<String>,
    issue_type: Option<String>,
    file_it: bool,
}

fn parse_drop_options(args: &[String]) -> Result<DropOptions, String> {
    let mut options = DropOptions::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let take_value = |name: &str| -> Result<String, String> {
            args.get(index + 1)
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg {
            "--repo" => {
                options.repo = Some(take_value("--repo")?);
                index += 2;
            }
            "--title" => {
                options.title = Some(take_value("--title")?);
                index += 2;
            }
            "--body-file" => {
                options.body_file = Some(take_value("--body-file")?);
                index += 2;
            }
            "--label" => {
                options.labels.push(take_value("--label")?);
                index += 2;
            }
            "--type" => {
                options.issue_type = Some(take_value("--type")?);
                index += 2;
            }
            "--file-it" => {
                options.file_it = true;
                index += 1;
            }
            other => return Err(format!("unknown option {other:?}")),
        }
    }
    Ok(options)
}

/// Resolve the label names the operator asked for against the repo's real
/// labels, reporting the ones that do not exist rather than dropping them.
///
/// A silently ignored `--label` produces an issue that is missing the routing
/// the operator believed they had applied.
fn resolve_labels(meta: &RepoMeta, wanted: &[String]) -> Result<Vec<String>, String> {
    let mut ids = Vec::new();
    let mut missing = Vec::new();
    for name in wanted {
        match meta
            .labels
            .iter()
            .find(|label| label.name.eq_ignore_ascii_case(name))
        {
            Some(label) => ids.push(label.id.clone()),
            None => missing.push(name.clone()),
        }
    }
    if missing.is_empty() {
        Ok(ids)
    } else {
        Err(format!(
            "no such label on this repo: {}",
            missing.join(", ")
        ))
    }
}

fn resolve_issue_type(meta: &RepoMeta, wanted: Option<&String>) -> Result<Option<String>, String> {
    let Some(name) = wanted else {
        return Ok(None);
    };
    match meta.issue_types.as_ref() {
        // `null`, not `[]` — the organisation configures no types at all.
        None => Err("this repository's organisation has no issue types configured".to_string()),
        Some(types) => types
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(name))
            .map(|t| Some(t.id.clone()))
            .ok_or_else(|| {
                format!(
                    "no such issue type: {name} (available: {})",
                    types
                        .iter()
                        .map(|t| t.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }),
    }
}

/// Open `$EDITOR` on a skeleton and read back what was written.
///
/// The same device as `flk config edit` (`src/cli.rs`) and `flk report
/// template`: the operator composes in their own editor, which already has
/// wrapping, selection, paste and their own keybindings — none of which a
/// hand-rolled field in a terminal overlay would have.
fn compose_in_editor(repo: &str, title: &str, meta: Option<&RepoMeta>) -> std::io::Result<String> {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("flk-issue-{}.md", std::process::id()));
    std::fs::write(&path, drop::skeleton(repo, title, meta))?;

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let status = crate::process::TracedCommand::new("sh", "issue_drop_editor")
        .args([
            "-c",
            &format!("{editor} \"$1\""),
            "sh",
            &path.to_string_lossy(),
        ])
        .status_traced()?;
    if !status.success() {
        // Leave the file: the operator may have written something worth
        // recovering, and deleting it is the one outcome they cannot undo.
        eprintln!("editor exited non-zero; draft kept at {}", path.display());
        return Ok(String::new());
    }
    let contents = std::fs::read_to_string(&path)?;
    let _ = std::fs::remove_file(&path);
    Ok(contents)
}

/// Ask before writing to someone's tracker. Anything but an explicit yes is no.
fn confirm_filing(repo: &str) -> std::io::Result<bool> {
    use std::io::Write;
    print!("file this into {repo}? [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer)? == 0 {
        // EOF: no answer is not consent.
        return Ok(false);
    }
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn run_drop(args: &[String]) -> std::io::Result<i32> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return Ok(0);
    }
    let options = match parse_drop_options(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("error: {message}");
            return Ok(2);
        }
    };

    let repo = match drop::normalize_destination(options.repo.as_deref().unwrap_or_default()) {
        Ok(repo) => repo,
        Err(err) => {
            eprintln!("error: {err}");
            return Ok(2);
        }
    };
    let Some(title) = options.title.filter(|t| !t.trim().is_empty()) else {
        eprintln!("error: {}", DropError::EmptyTitle);
        return Ok(2);
    };

    let (owner, name) = repo.split_once('/').expect("normalized as owner/name");
    let meta = match issues::fetch_meta(owner, name) {
        Ok(meta) => meta,
        Err(kind) => {
            eprintln!("error: {}", explain(kind));
            return Ok(1);
        }
    };

    let label_ids = match resolve_labels(&meta, &options.labels) {
        Ok(ids) => ids,
        Err(message) => {
            eprintln!("error: {message}");
            return Ok(2);
        }
    };
    let issue_type_id = match resolve_issue_type(&meta, options.issue_type.as_ref()) {
        Ok(id) => id,
        Err(message) => {
            eprintln!("error: {message}");
            return Ok(2);
        }
    };

    let raw_body = match options.body_file.as_ref() {
        Some(path) => std::fs::read_to_string(path)?,
        None => compose_in_editor(&repo, &title, Some(&meta))?,
    };
    let body = drop::parse_body(&raw_body);

    let draft = Draft {
        repo: repo.clone(),
        title: title.clone(),
        body: body.clone(),
        label_names: options.labels.clone(),
        issue_type: options.issue_type.clone(),
    };
    if let Err(err) = drop::validate(&draft) {
        eprintln!("error: {err}");
        return Ok(2);
    }

    println!("destination: {repo}");
    println!("title:       {title}");
    if !options.labels.is_empty() {
        println!("labels:      {}", options.labels.join(", "));
    }
    if let Some(kind) = options.issue_type.as_ref() {
        println!("type:        {kind}");
    }
    println!();
    println!("{body}");
    println!();

    for advisory in drop::advisories(Some(&meta), None) {
        eprintln!("note: {}", advisory.message());
    }

    if !options.file_it {
        println!("nothing has been filed. re-run with --file-it to create it.");
        return Ok(0);
    }

    // ADR-0010's preview-before-send invariant, kept on the write path. The
    // dialog hand-off passes --file-it, so without this the operator's editor
    // closing would file immediately and the preview above would scroll past
    // unread. Only asked when there is a human to ask: a scripted run passed
    // --file-it deliberately and has no terminal to answer on.
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) && !confirm_filing(&repo)? {
        println!("not filed.");
        return Ok(0);
    }

    match issues::create_issue(&NewIssue {
        repo_id: meta.repo_id,
        title,
        body,
        label_ids,
        issue_type_id,
    }) {
        Ok(filed) => {
            println!("filed #{}: {}", filed.number, filed.url);
            Ok(0)
        }
        Err(kind) => {
            eprintln!("error: {}", explain(kind));
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::issues::{IssueType, Label, TemplatePosture};

    fn meta() -> RepoMeta {
        RepoMeta {
            repo_id: "R_1".into(),
            labels: vec![
                Label {
                    id: "L_bug".into(),
                    name: "bug".into(),
                },
                Label {
                    id: "L_enh".into(),
                    name: "enhancement".into(),
                },
            ],
            issue_types: Some(vec![IssueType {
                id: "T_bug".into(),
                name: "Bug".into(),
            }]),
            templates: TemplatePosture::None,
        }
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn filing_requires_an_explicit_flag() {
        let options = parse_drop_options(&args(&["--repo", "o/n", "--title", "t"])).unwrap();
        assert!(
            !options.file_it,
            "a bare drop must not write to someone's tracker"
        );
        let explicit =
            parse_drop_options(&args(&["--repo", "o/n", "--title", "t", "--file-it"])).unwrap();
        assert!(explicit.file_it);
    }

    #[test]
    fn labels_are_repeatable_and_options_round_trip() {
        let options = parse_drop_options(&args(&[
            "--repo",
            "o/n",
            "--title",
            "a title",
            "--label",
            "bug",
            "--label",
            "enhancement",
            "--type",
            "Bug",
            "--body-file",
            "b.md",
        ]))
        .unwrap();
        assert_eq!(options.repo.as_deref(), Some("o/n"));
        assert_eq!(options.title.as_deref(), Some("a title"));
        assert_eq!(options.labels, ["bug", "enhancement"]);
        assert_eq!(options.issue_type.as_deref(), Some("Bug"));
        assert_eq!(options.body_file.as_deref(), Some("b.md"));
    }

    #[test]
    fn a_flag_without_its_value_is_an_error_not_a_panic() {
        assert!(parse_drop_options(&args(&["--repo"])).is_err());
        assert!(parse_drop_options(&args(&["--title"])).is_err());
        assert!(parse_drop_options(&args(&["--nonsense"])).is_err());
    }

    #[test]
    fn an_unknown_label_is_reported_rather_than_silently_dropped() {
        // Silently ignoring it files an issue missing the routing the operator
        // believed they had applied.
        let err = resolve_labels(&meta(), &["bug".into(), "nope".into()]).unwrap_err();
        assert!(err.contains("nope"), "{err}");
        assert!(!err.contains("bug"), "the valid one is not blamed: {err}");
    }

    #[test]
    fn labels_resolve_to_ids_case_insensitively() {
        let ids = resolve_labels(&meta(), &["BUG".into()]).unwrap();
        assert_eq!(ids, ["L_bug"]);
        assert_eq!(resolve_labels(&meta(), &[]).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn an_issue_type_on_a_repo_with_none_configured_says_so() {
        let mut without = meta();
        without.issue_types = None;
        let err = resolve_issue_type(&without, Some(&"Bug".to_string())).unwrap_err();
        assert!(err.contains("no issue types"), "{err}");
        // Asking for none is fine on such a repo.
        assert_eq!(resolve_issue_type(&without, None).unwrap(), None);
    }

    #[test]
    fn an_unknown_issue_type_lists_the_real_ones() {
        let err = resolve_issue_type(&meta(), Some(&"Nope".to_string())).unwrap_err();
        assert!(err.contains("Nope"), "{err}");
        assert!(err.contains("Bug"), "the available types are named: {err}");
        assert_eq!(
            resolve_issue_type(&meta(), Some(&"bug".to_string())).unwrap(),
            Some("T_bug".to_string())
        );
    }

    #[test]
    fn a_scope_failure_does_not_tell_the_operator_to_log_in_again() {
        let forbidden = explain(GraphQlErrorKind::Forbidden);
        assert!(forbidden.contains("scope"), "{forbidden}");
        assert!(
            !forbidden.contains("gh auth login"),
            "re-authenticating fixes nothing here: {forbidden}"
        );
        assert!(explain(GraphQlErrorKind::Auth).contains("gh auth login"));
    }
}
