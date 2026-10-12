#![expect(
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stderr usage for humans and scripts"
)]
//! `flk owner bind|unbind|list|show` (ADR-0035 §1, #1196).
//!
//! `bind` and `unbind` are operator verbs: the server refuses an attested
//! agent caller with `operator_only`, which exits [`exit::OPERATOR_ONLY`]
//! (13). Both are gated (#868), so against a server without `owner.*` they
//! exit 78 before sending. After a bind, the client reports which layer-one
//! points of ADR-0035 §6.3 are in place for the repo, read from GitHub
//! through `gh` and never configured here.

use std::time::Duration;

use serde_json::{json, Value};

use super::exit;
use crate::api::schema::{
    EmptyParams, Method, OwnerBindParams, OwnerHarness, OwnerRepoParams, Request,
};

pub(super) const BIND_USAGE: &str = "flk owner bind <owner/repo> --checkout PATH --harness <claude|codex|opencode> [--model MODEL] [--profile NAME]...";
pub(super) const UNBIND_USAGE: &str = "flk owner unbind <owner/repo>";
pub(super) const LIST_USAGE: &str = "flk owner list";
pub(super) const SHOW_USAGE: &str = "flk owner show <owner/repo>";

/// How long one `gh api` read may take before its fact reads `unknown`.
const GH_READ_TIMEOUT: Duration = Duration::from_secs(20);

pub(super) fn run_owner_command(args: &[String]) -> std::io::Result<i32> {
    let rest = args.get(1..).unwrap_or_default();
    match (args.first().map(String::as_str), rest) {
        (Some("bind"), rest) => bind(rest),
        (Some("unbind"), [repo]) => send("unbind", Method::OwnerUnbind(repo_params(repo))),
        (Some("list"), []) => send("list", Method::OwnerList(EmptyParams::default())),
        (Some("show"), [repo]) => send("show", Method::OwnerShow(repo_params(repo))),
        (Some("help" | "--help" | "-h"), _) => {
            print_usage();
            Ok(exit::SUCCESS)
        }
        _ => {
            print_usage();
            Ok(exit::USAGE)
        }
    }
}

fn print_usage() {
    for usage in [BIND_USAGE, UNBIND_USAGE, LIST_USAGE, SHOW_USAGE] {
        eprintln!("usage: {usage}");
    }
}

fn repo_params(repo: &str) -> OwnerRepoParams {
    OwnerRepoParams { repo: repo.into() }
}

fn bind(args: &[String]) -> std::io::Result<i32> {
    let params = match parse_bind(args) {
        Ok(params) => params,
        Err(problem) => {
            eprintln!("{problem}\nusage: {BIND_USAGE}");
            return Ok(exit::USAGE);
        }
    };
    let repo = params.repo.clone();
    let mut response = request("bind", Method::OwnerBind(params))?;
    if response.get("error").is_none() {
        response["result"]["layer_one"] = json!(layer_one_facts(&repo, &mut gh_api));
    }
    print(&response)
}

fn parse_bind(args: &[String]) -> Result<OwnerBindParams, String> {
    let (repo, flags) = args.split_first().ok_or("missing <owner/repo>")?;
    let (mut checkout, mut harness, mut model, mut profiles) = (None, None, None, Vec::new());
    let mut flags = flags.iter();
    while let Some(flag) = flags.next() {
        let value = flags
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--checkout" => {
                checkout = Some(
                    std::path::absolute(value)
                        .map_err(|err| format!("--checkout {value:?}: {err}"))?,
                );
            }
            "--harness" => {
                harness = Some(
                    serde_json::from_value::<OwnerHarness>(json!(value))
                        .map_err(|_| format!("unknown harness {value:?}"))?,
                );
            }
            "--model" => model = Some(value.clone()),
            "--profile" => profiles.push(value.clone()),
            other => return Err(format!("unknown option: {other}")),
        }
    }
    Ok(OwnerBindParams {
        repo: repo.clone(),
        checkout: checkout.ok_or("missing --checkout")?.display().to_string(),
        harness: harness.ok_or("missing --harness")?,
        model,
        profiles,
    })
}

fn request(verb: &str, method: Method) -> std::io::Result<Value> {
    super::send_request(&Request {
        id: format!("cli:owner:{verb}"),
        method,
    })
}

fn send(verb: &str, method: Method) -> std::io::Result<i32> {
    print(&request(verb, method)?)
}

fn print(response: &Value) -> std::io::Result<i32> {
    match response["error"]["code"].as_str() {
        Some(code) => Ok(super::output::error(response, error_exit(code))),
        None => super::print_response(response),
    }
}

/// The owner verbs' exit for an error code: an operator verb refused for
/// its caller exits 13 (ADR-0029 §2, ADR-0014 boundary) and an unbound repo
/// exits 4, both never retryable without a change.
fn error_exit(code: &str) -> i32 {
    match code {
        "operator_only" => exit::OPERATOR_ONLY,
        "not_found" => exit::GONE,
        code => super::error_code_exit(code, false),
    }
}

/// A failed `gh api` read: the HTTP status when GitHub answered one.
struct GhFailure {
    status: Option<u16>,
    detail: String,
}

fn gh_api(path: &str) -> Result<Value, GhFailure> {
    let output = crate::process::TracedCommand::new("gh", "owner_bind")
        .args(["api", path])
        .output_traced_with_timeout(GH_READ_TIMEOUT)
        .map_err(|err| GhFailure {
            status: None,
            detail: format!("gh: {err}"),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let status = stderr
            .split("(HTTP ")
            .nth(1)
            .and_then(|rest| rest.get(..3))
            .and_then(|code| code.parse().ok());
        return Err(GhFailure {
            status,
            detail: stderr,
        });
    }
    serde_json::from_slice(&output.stdout).map_err(|err| GhFailure {
        status: None,
        detail: format!("gh api {path}: {err}"),
    })
}

/// ADR-0035 §6.3's five layer-one points for `repo`, as read-only facts:
/// `in_place`, `missing`, `unavailable` (GitHub refuses the feature on this
/// plan, so the merge gate is forced to `ask`), `unknown` (the read failed),
/// or `not_built` (the flk side does not exist yet; the slice is named).
fn layer_one_facts(
    repo: &str,
    api: &mut dyn FnMut(&str) -> Result<Value, GhFailure>,
) -> Vec<Value> {
    let fact = |point: &str, state: &str, detail: String| json!({"point": point, "state": state, "detail": detail});
    let mut facts = vec![
        fact(
            "agent_user",
            "not_built",
            "agents run as the operator's uid, so every guarantee below is cooperative only (O19a, #1208)".into(),
        ),
        fact(
            "gate_app",
            "not_built",
            "no flk-gate App posts flk/land-gate yet (O19c, #1210)".into(),
        ),
    ];
    facts.push(match api(&format!("repos/{repo}")) {
        Err(failure) => fact("required_gate_check", "unknown", failure.detail),
        Ok(meta) => {
            let branch = meta["default_branch"].as_str().unwrap_or("main");
            let (state, detail) = required_gate_check(repo, branch, api);
            fact("required_gate_check", state, detail)
        }
    });
    facts.push(match api(&format!("repos/{repo}/environments/release")) {
        Ok(env)
            if env["protection_rules"].as_array().is_some_and(|rules| {
                rules
                    .iter()
                    .any(|rule| rule["type"] == "required_reviewers")
            }) =>
        {
            fact(
                "release_environment",
                "in_place",
                "environment `release` requires a reviewer".into(),
            )
        }
        Ok(_) => fact(
            "release_environment",
            "missing",
            "environment `release` has no required reviewer".into(),
        ),
        Err(GhFailure {
            status: Some(404), ..
        }) => fact(
            "release_environment",
            "missing",
            "no environment `release`".into(),
        ),
        Err(failure) => fact("release_environment", "unknown", failure.detail),
    });
    facts.push(fact(
        "bypass_detection",
        "not_built",
        "a merge without a flk/land-gate success is not detected yet (O3a #1197, O19c #1210)"
            .into(),
    ));
    facts
}

/// Whether `branch` requires `flk/land-gate`, through classic branch
/// protection or a ruleset.
fn required_gate_check(
    repo: &str,
    branch: &str,
    api: &mut dyn FnMut(&str) -> Result<Value, GhFailure>,
) -> (&'static str, String) {
    const GATE: &str = "flk/land-gate";
    let classic = api(&format!(
        "repos/{repo}/branches/{branch}/protection/required_status_checks"
    ));
    let rules = api(&format!("repos/{repo}/rules/branches/{branch}"));
    let in_classic = classic.as_ref().is_ok_and(|checks| {
        checks["contexts"]
            .as_array()
            .is_some_and(|c| c.iter().any(|c| c == GATE))
            || checks["checks"]
                .as_array()
                .is_some_and(|c| c.iter().any(|c| c["context"] == GATE))
    });
    let in_rules = rules.as_ref().is_ok_and(|rules| {
        rules.as_array().is_some_and(|rules| {
            rules.iter().any(|rule| {
                rule["type"] == "required_status_checks"
                    && rule["parameters"]["required_status_checks"]
                        .as_array()
                        .is_some_and(|c| c.iter().any(|c| c["context"] == GATE))
            })
        })
    });
    match (&classic, &rules) {
        _ if in_classic || in_rules => ("in_place", format!("`{branch}` requires {GATE}")),
        (
            Err(GhFailure {
                status: Some(403), ..
            }),
            _,
        ) => (
            "unavailable",
            "branch protection is unavailable on this plan, so the merge gate stays `ask`".into(),
        ),
        (
            Ok(_)
            | Err(GhFailure {
                status: Some(404), ..
            }),
            Ok(_),
        ) => ("missing", format!("`{branch}` does not require {GATE}")),
        (Err(failure), _) | (_, Err(failure)) => ("unknown", failure.detail.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn bind_parses_every_flag_and_absolutizes_the_checkout() {
        let params = parse_bind(&args(&[
            "gerchowl/flock",
            "--checkout",
            "/srv/flock",
            "--harness",
            "codex",
            "--model",
            "m",
            "--profile",
            "a",
            "--profile",
            "b",
        ]))
        .unwrap();
        assert_eq!(params.repo, "gerchowl/flock");
        assert_eq!(params.checkout, "/srv/flock");
        assert_eq!(params.harness, OwnerHarness::Codex);
        assert_eq!(params.model.as_deref(), Some("m"));
        assert_eq!(params.profiles, ["a", "b"]);

        let relative =
            parse_bind(&args(&["a/b", "--checkout", "wt", "--harness", "claude"])).unwrap();
        assert!(std::path::Path::new(&relative.checkout).is_absolute());
        for broken in [
            &["a/b", "--harness", "claude"][..],
            &["a/b", "--checkout", "/x"],
            &["a/b", "--checkout", "/x", "--harness", "vim"],
            &["a/b", "--checkout"],
            &[
                "a/b",
                "--checkout",
                "/x",
                "--harness",
                "claude",
                "--force",
                "1",
            ],
        ] {
            assert!(parse_bind(&args(broken)).is_err(), "{broken:?}");
        }
    }

    /// ADR-0035 §1 acceptance: an attested agent calling `bind` exits 13.
    #[test]
    fn an_operator_only_refusal_exits_13_and_an_unbound_repo_exits_4() {
        assert_eq!(error_exit("operator_only"), 13);
        assert_eq!(error_exit("not_found"), exit::GONE);
        assert_eq!(error_exit("invalid_params"), exit::FAILURE);
        assert_eq!(error_exit("server_version_gap"), exit::PROTOCOL_GAP);
    }

    fn states(facts: &[Value]) -> Vec<(String, String)> {
        facts
            .iter()
            .map(|fact| {
                (
                    fact["point"].as_str().unwrap().to_string(),
                    fact["state"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    fn http(status: u16) -> GhFailure {
        GhFailure {
            status: Some(status),
            detail: format!("gh: HTTP {status}"),
        }
    }

    #[test]
    fn layer_one_reads_the_gate_check_and_the_release_environment() {
        let mut api = |path: &str| match path {
            "repos/x/y" => Ok(json!({"default_branch": "dev"})),
            "repos/x/y/branches/dev/protection/required_status_checks" => {
                Ok(json!({"contexts": ["check"], "checks": [{"context": "check"}]}))
            }
            "repos/x/y/rules/branches/dev" => Ok(json!([{"type": "required_status_checks",
                "parameters": {"required_status_checks": [{"context": "flk/land-gate"}]}}])),
            "repos/x/y/environments/release" => {
                Ok(json!({"protection_rules": [{"type": "required_reviewers"}]}))
            }
            other => panic!("unexpected read {other}"),
        };
        let facts = layer_one_facts("x/y", &mut api);
        let want = [
            ("agent_user", "not_built"),
            ("gate_app", "not_built"),
            ("required_gate_check", "in_place"),
            ("release_environment", "in_place"),
            ("bypass_detection", "not_built"),
        ];
        assert_eq!(
            states(&facts),
            want.map(|(p, s)| (p.to_string(), s.to_string()))
        );
    }

    /// A private repo on a plan without protection answers 403: the gate
    /// cannot be configured there, which is a fact, not a failed read.
    #[test]
    fn layer_one_tells_a_missing_gate_from_an_unavailable_one_and_a_failed_read() {
        let case = |classic: Result<Value, u16>, rules: Result<Value, u16>| {
            let mut api = |path: &str| match path {
                "repos/x/y" => Ok(json!({"default_branch": "main"})),
                p if p.ends_with("required_status_checks") => classic.clone().map_err(http),
                p if p.contains("/rules/") => rules.clone().map_err(http),
                _ => Err(http(404)),
            };
            states(&layer_one_facts("x/y", &mut api))[2..4].to_vec()
        };
        let pair = |gate: &str, env: &str| {
            vec![
                ("required_gate_check".to_string(), gate.to_string()),
                ("release_environment".to_string(), env.to_string()),
            ]
        };
        assert_eq!(
            case(Err(403), Ok(json!([]))),
            pair("unavailable", "missing")
        );
        assert_eq!(case(Err(404), Ok(json!([]))), pair("missing", "missing"));
        assert_eq!(
            case(Ok(json!({"contexts": []})), Ok(json!([]))),
            pair("missing", "missing")
        );
        assert_eq!(case(Err(500), Ok(json!([]))), pair("unknown", "missing"));
        assert_eq!(
            case(Ok(json!({"contexts": []})), Err(502)),
            pair("unknown", "missing")
        );
    }
}
