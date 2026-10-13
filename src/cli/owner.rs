#![expect(
    clippy::print_stderr,
    reason = "CLI output surface: usage and errors on stderr; replies go through print_response"
)]
//! `flk owner bind|unbind|list|show` (ADR-0035 §1, #1231): the operator's
//! verbs over the `owner.*` socket methods. `flk owner policy show|adopt`
//! (ADR-0035 §6.2, #1195) is documented at [`policy`].
//!
//! `bind` and `unbind` are operator verbs. The server refuses an agent,
//! relayed or unattested caller with `operator_only`, which exits 13
//! (`exit::POLICY_REFUSED`, ADR-0029 §2). That check is cooperative only until
//! agents run as their own Unix user (O19a, #1208, #1177): a child an agent
//! detaches with `setsid` still attests as the operator. `unbind` and `show`
//! of a repo with no binding exit `GONE` (4). Every verb prints the server's
//! JSON reply, and under `--json` an error is the ADR-0029 §4 envelope.

use super::exit;
use crate::api::schema::{
    EmptyParams, Method, OwnerBindParams, OwnerHarness, OwnerPolicyParams, OwnerRepoParams, Request,
};

pub(super) const BIND_USAGE: &str = "flk owner bind <owner/repo> --checkout PATH --harness claude|codex|opencode [--model NAME] [--profile NAME]... [--json]";
pub(super) const UNBIND_USAGE: &str = "flk owner unbind <owner/repo> [--json]";
pub(super) const LIST_USAGE: &str = "flk owner list [--json]";
pub(super) const SHOW_USAGE: &str = "flk owner show <owner/repo> [--json]";
pub(super) const POLICY_USAGE: &str =
    "flk owner policy show [--repo PATH] | flk owner policy adopt <VERSION|none> [--repo PATH]";

pub(super) fn run_owner_command(args: &[String]) -> std::io::Result<i32> {
    let verb = args.first().map(String::as_str);
    let rest = args.get(1..).unwrap_or_default();
    let method = match verb {
        Some("policy") => return policy(rest),
        Some("bind") => parse_bind(rest).map(Method::OwnerBind),
        Some("unbind") => repo_operand(rest, UNBIND_USAGE).map(Method::OwnerUnbind),
        Some("list") if rest.is_empty() => Ok(Method::OwnerList(EmptyParams {})),
        Some("list") => Err(format!("usage: {LIST_USAGE}")),
        Some("show") => repo_operand(rest, SHOW_USAGE).map(Method::OwnerShow),
        // Answered on stderr, as `flk owner policy`'s dispatcher did (#1195).
        Some("help" | "--help" | "-h") if rest.is_empty() => {
            eprintln!("{}", usage());
            return Ok(exit::SUCCESS);
        }
        _ => Err(usage()),
    };
    let method = match method {
        Ok(method) => method,
        Err(problem) => {
            eprintln!("{problem}");
            return Ok(exit::USAGE);
        }
    };
    let response = super::send_request(&Request {
        id: format!("cli:owner:{}", verb.unwrap_or_default()),
        method,
    })?;
    if response["error"]["code"] == "not_found" {
        return Ok(super::output::error(&response, exit::GONE));
    }
    super::print_response(&response)
}

fn usage() -> String {
    format!(
        "usage: {BIND_USAGE}\n       {UNBIND_USAGE}\n       {LIST_USAGE}\n       {SHOW_USAGE}\n       {POLICY_USAGE}"
    )
}

/// The one `<owner/repo>` operand of `unbind` and `show`.
fn repo_operand(args: &[String], usage: &str) -> Result<OwnerRepoParams, String> {
    match args {
        [repo] if !repo.starts_with('-') => Ok(OwnerRepoParams { repo: repo.clone() }),
        _ => Err(format!("usage: {usage}")),
    }
}

/// `bind`'s operand and flags. A relative `--checkout` is resolved against
/// the current directory, which is on the serving node: the CLI only talks to
/// its local server. The server checks the slug and that the path is a git
/// checkout.
fn parse_bind(args: &[String]) -> Result<OwnerBindParams, String> {
    let usage = || format!("usage: {BIND_USAGE}");
    let (repo, mut flags) = match args.split_first() {
        Some((repo, flags)) if !repo.starts_with('-') => (repo.clone(), flags.iter()),
        _ => return Err(usage()),
    };
    let (mut checkout, mut harness, mut model, mut profiles) = (None, None, None, Vec::new());
    while let Some(flag) = flags.next() {
        // A value that is itself a flag means the value was left out: `bind r
        // --checkout --harness claude` must not bind a checkout named
        // `--harness`.
        let value = flags
            .next()
            .filter(|value| !value.starts_with("--"))
            .ok_or_else(|| format!("{flag} needs a value\n{}", usage()))?;
        match flag.as_str() {
            "--checkout" => checkout = Some(value.clone()),
            "--harness" => harness = Some(parse_harness(value)?),
            "--model" => model = Some(value.clone()),
            "--profile" => profiles.push(value.clone()),
            other => return Err(format!("unknown option: {other}\n{}", usage())),
        }
    }
    let checkout = checkout.ok_or_else(|| format!("--checkout is required\n{}", usage()))?;
    let checkout =
        std::path::absolute(&checkout).map_err(|err| format!("--checkout {checkout:?}: {err}"))?;
    Ok(OwnerBindParams {
        repo,
        checkout: checkout.display().to_string(),
        harness: harness.ok_or_else(|| format!("--harness is required\n{}", usage()))?,
        model,
        profiles,
    })
}

/// `flk owner policy show|adopt` (ADR-0035 §6.2, #1195). `show` reports the
/// repo's owner policy as committed on its default branch, the version the
/// operator adopted, and whether a newer one is pending. `adopt` is the
/// operator verb that makes the current version apply. It is state-changing
/// for the #868 gate, so against a server without it it exits 78 before
/// sending, and a refusal of an agent caller exits 13 (`operator_only`,
/// `exit::POLICY_REFUSED`). That refusal is cooperative only until O19a
/// (#1208, #1177): see ADR-0035 §6.3 point 6.
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

fn parse_harness(raw: &str) -> Result<OwnerHarness, String> {
    serde_json::from_value(serde_json::Value::String(raw.to_string()))
        .map_err(|_| format!("invalid value for --harness: {raw} (claude|codex|opencode)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn bind_reads_every_flag_and_keeps_an_absolute_checkout() {
        let params = parse_bind(&args(&[
            "example/widgets",
            "--checkout",
            "/srv/widgets",
            "--harness",
            "codex",
            "--model",
            "m",
            "--profile",
            "main",
            "--profile",
            "spare",
        ]))
        .unwrap();
        assert_eq!(
            params,
            OwnerBindParams {
                repo: "example/widgets".into(),
                checkout: "/srv/widgets".into(),
                harness: OwnerHarness::Codex,
                model: Some("m".into()),
                profiles: vec!["main".into(), "spare".into()],
            }
        );
    }

    /// The server refuses a relative path, so the CLI sends it absolute.
    #[test]
    fn bind_sends_a_relative_checkout_as_an_absolute_path() {
        let params = parse_bind(&args(&[
            "example/widgets",
            "--checkout",
            "widgets",
            "--harness",
            "claude",
        ]))
        .unwrap();
        let checkout = std::path::Path::new(&params.checkout);
        assert!(checkout.is_absolute(), "{checkout:?}");
        assert!(checkout.ends_with("widgets"), "{checkout:?}");
    }

    #[test]
    fn bind_without_a_repo_checkout_or_known_harness_is_a_usage_error() {
        for bad in [
            &["--checkout", "/srv/w", "--harness", "claude"][..],
            &["example/widgets", "--harness", "claude"],
            &["example/widgets", "--checkout", "/srv/w"],
            &[
                "example/widgets",
                "--checkout",
                "/srv/w",
                "--harness",
                "gpt",
            ],
            &["example/widgets", "--checkout"],
            // Without the check this binds a checkout named `--profile`.
            &[
                "example/widgets",
                "--checkout",
                "--profile",
                "--harness",
                "claude",
            ],
        ] {
            assert!(parse_bind(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn unbind_and_show_take_exactly_one_repo() {
        assert_eq!(
            repo_operand(&args(&["example/widgets"]), SHOW_USAGE),
            Ok(OwnerRepoParams {
                repo: "example/widgets".into()
            })
        );
        for bad in [&[][..], &["a/b", "c/d"], &["--repo"]] {
            assert!(repo_operand(&args(bad), SHOW_USAGE).is_err(), "{bad:?}");
        }
    }
}
