//! Operator CLI for inactive ADR-0052 binding preparation.
#![expect(clippy::print_stderr, reason = "CLI usage surface")]

use crate::api::schema::{Method, ProfileEnableParams, Request};
use crate::structured::profiles::bindings::{
    transactions::{BindingSpec, PlanRequest, RevokeRequest},
    Scope,
};

/// Verb help for enablement, which remains unavailable until production readiness.
pub(super) const ENABLE_USAGE: &str = "flk profile enable --plan <id> --expect <digest> (unavailable until production readiness is complete)";

/// Group usage for data-only preparation and the inactive enable transaction.
const USAGE: &str = "flk profile plan bind --repo <id> --nodes <ids> --agents <ids> --account <alias> --bundle <digests> --credential-ref <ref> --generation <n> [--grants <ids>] [--binding <id>] | bind|enable --plan <id> --expect <digest> | revoke --binding <id> --reason <text> | status [--format json]";

fn parse(args: &[String]) -> Option<Method> {
    let (verb, tail) = args.split_first()?;
    let (verb, tail) = if verb == "plan" {
        let (action, tail) = tail.split_first()?;
        if action != "bind" {
            return None;
        }
        ("plan_bind", tail)
    } else {
        (verb.as_str(), tail)
    };
    let mut flags = std::collections::BTreeMap::new();
    let mut pairs = tail.chunks_exact(2);
    for pair in &mut pairs {
        if pair[1].is_empty()
            || pair[1].starts_with('-')
            || flags.insert(pair[0].as_str(), pair[1].as_str()).is_some()
        {
            return None;
        }
    }
    if !pairs.remainder().is_empty() {
        return None;
    }
    let method = match verb {
        "status" => Method::ProfileStatus(Default::default()),
        "enable" | "bind" => {
            let params = ProfileEnableParams {
                plan: flags.remove("--plan")?.into(),
                expect: flags.remove("--expect")?.into(),
            };
            if verb == "enable" {
                Method::ProfileEnable(params)
            } else {
                Method::ProfileBind(params)
            }
        }
        "revoke" => Method::ProfileRevoke(RevokeRequest {
            binding: flags.remove("--binding")?.into(),
            reason: flags.remove("--reason")?.into(),
        }),
        "plan_bind" => Method::ProfilePlan(PlanRequest::Bind(BindingSpec {
            binding_id: flags.remove("--binding").map(String::from),
            target: Scope {
                repo_id: flags.remove("--repo")?.into(),
                node_ids: list(flags.remove("--nodes")?)?,
                agent_ids: list(flags.remove("--agents")?)?,
            },
            account_alias: flags.remove("--account")?.into(),
            credential_ref: flags.remove("--credential-ref")?.into(),
            credential_generation: flags
                .remove("--generation")?
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)?,
            bundle_digests: list(flags.remove("--bundle")?)?,
            grants: match flags.remove("--grants") {
                Some(value) => list(value)?,
                None => Vec::new(),
            },
        })),
        _ => return None,
    };
    flags.is_empty().then_some(method)
}

fn list(value: &str) -> Option<Vec<String>> {
    value
        .split(',')
        .map(|v| (!v.trim().is_empty()).then(|| v.to_owned()))
        .collect()
}

/// Send a fully specified request and preserve the shared typed error/exit mapping.
pub(super) fn run(args: &[String]) -> std::io::Result<i32> {
    let Some(method) = parse(args) else {
        eprintln!("usage: {USAGE}");
        return Ok(super::exit::USAGE);
    };
    super::print_response(&super::send_request(&Request {
        id: "cli:profile".into(),
        method,
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_carries_review_identifiers_and_is_never_retried() {
        for args in [
            ["enable", "--plan", "review-plan", "--expect", "digest"],
            ["enable", "--expect", "digest", "--plan", "review-plan"],
        ] {
            let method = parse(&args.map(String::from)).unwrap();
            assert_eq!(
                method,
                Method::ProfileEnable(ProfileEnableParams {
                    plan: "review-plan".into(),
                    expect: "digest".into()
                })
            );
            assert!(!method.is_retry_safe());
            assert!(method.needs_current_protocol());
        }
    }

    #[test]
    fn enable_requires_both_identifiers_and_rejects_ambiguous_flags() {
        for args in [
            vec![],
            vec!["enable"],
            vec!["enable", "--plan", "p"],
            vec!["enable", "--plan", "p", "--expect"],
            vec!["enable", "--plan", "", "--expect", "d"],
            vec!["enable", "--plan", "--expect", "d"],
            vec!["enable", "--plan", "p", "--expect", "d", "--plan", "q"],
            vec!["enable", "--plan", "p", "--expect", "d", "--unknown", "x"],
            vec!["start", "--plan", "p", "--expect", "d"],
        ] {
            assert!(parse(&args.into_iter().map(String::from).collect::<Vec<_>>()).is_none());
        }
    }
}

#[cfg(test)]
mod binding_tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn closed_commands_reject_unsupported_duplicate_and_incomplete_flags() {
        for values in [
            vec!["status", "--binding", "b"],
            vec!["revoke", "--binding", "b"],
            vec!["revoke", "--binding", "b", "--reason", "r", "--reason", "q"],
            vec!["bind", "--plan", "p", "--expect", "d", "--budget", "b"],
            vec!["plan", "rotate"],
        ] {
            assert!(parse(&args(&values)).is_none(), "{values:?}");
        }
        assert!(matches!(
            parse(&args(&["status"])),
            Some(Method::ProfileStatus(_))
        ));
        assert!(list("node-a,,node-b").is_none());
    }
    #[test]
    fn plan_flags_only_prepare_identifiers_and_support_existing_binding() {
        let values = args(&[
            "plan",
            "bind",
            "--repo",
            "repo-fixture",
            "--nodes",
            "node-a,node-b",
            "--agents",
            "agent-fixture",
            "--account",
            "fixture",
            "--bundle",
            "digest-a,digest-b",
            "--credential-ref",
            "opaque-ref",
            "--generation",
            "2",
            "--binding",
            "binding-fixture",
        ]);
        let method = parse(&values).unwrap();
        assert!(!method.is_retry_safe());
        let Method::ProfilePlan(PlanRequest::Bind(spec)) = method else {
            panic!("bind plan expected")
        };
        assert_eq!(spec.binding_id.as_deref(), Some("binding-fixture"));
        assert_eq!(spec.target.node_ids, ["node-a", "node-b"]);
        assert_eq!(spec.bundle_digests, ["digest-a", "digest-b"]);
        assert!(spec.grants.is_empty());
        for (flag, bad) in [
            ("--generation", "0"),
            ("--generation", "oops"),
            ("--nodes", "a,,b"),
        ] {
            let mut invalid = values.clone();
            let index = invalid.iter().position(|v| v == flag).unwrap();
            invalid[index + 1] = bad.into();
            assert!(parse(&invalid).is_none());
        }
    }
}
