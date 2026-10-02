#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stdout/stderr for humans and scripts"
)]
use crate::api::schema::{LineageParams, Method, Request};

/// `flk lineage <target>` (#175 O1, US-4): print the fork ancestry chain of
/// a pane / worktree / branch, reconstructed from the durable event log so
/// it works across server restarts and after the panes are gone.
/// The one place that says what a target resolves from, and how a reused
/// branch name is disambiguated. Kept whole: a usage line alone would be a
/// downgrade of what this command already explained.
fn lineage_help_text() -> String {
    let mut out = String::new();
    use std::fmt::Write as _;
    let _ = writeln!(out, "usage: flk lineage <target> [--json]");
    let _ = writeln!(
        out,
        "  target: pane id, agent name, worktree path or basename, or branch name"
    );
    let _ = writeln!(
        out,
        "  ambiguous identities (reused branch names) resolve to the most recent fork"
    );
    out
}

pub(super) fn run_lineage_command(args: &[String]) -> std::io::Result<i32> {
    const USAGE: &str = "usage: flk lineage <target> [--json]";
    let Some(first) = args.first().map(|arg| arg.as_str()) else {
        eprintln!("{USAGE}");
        return Ok(2);
    };
    // #455: `cli::help`'s predicate, so this command's own richer usage (what a
    // target resolves from, and how reused branch names disambiguate) is
    // answered on the same terms as the table's — and is honoured anywhere in
    // the argument list, not only in first position.
    if super::help::asks_for_help(args) || first == "help" {
        // stdout, like `flk --help`: the request was honoured, so this is the
        // command's output and can be redirected or paged.
        print!("{}", lineage_help_text());
        return Ok(0);
    }
    let mut json = false;
    for arg in &args[1..] {
        match arg.as_str() {
            "--json" => json = true,
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    let response = super::send_request(&Request {
        id: "cli:lineage".into(),
        method: Method::AgentLineage(LineageParams {
            target: first.to_string(),
        }),
    })?;
    if json {
        return super::print_response(&response);
    }
    if response.get("error").is_some() {
        return super::print_response(&response);
    }
    let Some(chain) = response
        .get("result")
        .and_then(|result| result.get("chain"))
        .and_then(|chain| chain.as_array())
    else {
        return super::print_response(&response);
    };
    // Deepest (the target's own fork) first, root ancestor last.
    for (depth, edge) in chain.iter().enumerate() {
        let child = &edge["child"];
        let parent = &edge["parent"];
        println!(
            "{}{} [{}] {} <- {}  (branch {}, seeded: {})",
            "  ".repeat(depth),
            edge["run_id"].as_str().unwrap_or("?"),
            edge["agent"].as_str().unwrap_or("?"),
            child["pane_id"].as_str().unwrap_or("?"),
            parent["pane_id"].as_str().unwrap_or("?"),
            child["branch"].as_str().unwrap_or("?"),
            edge["seeded"],
        );
    }
    Ok(0)
}
