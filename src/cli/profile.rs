//! Operator request for the ADR-0052 profile enable transaction.
#![expect(clippy::print_stderr, reason = "CLI usage surface")]

use crate::api::schema::{Method, ProfileEnableParams, Request};

/// CLI syntax for the inactive enable transaction.
pub(super) const USAGE: &str = "flk profile enable --plan <id> --expect <digest> (unavailable until production readiness is complete)";

fn parse(args: &[String]) -> Option<Method> {
    let mut plan = None;
    let mut expect = None;
    let mut flags = match args.first().map(String::as_str) {
        Some("enable") => args[1..].iter(),
        _ => return None,
    };
    while let Some(flag) = flags.next() {
        let value = flags.next()?;
        if value.is_empty() || value.starts_with('-') {
            return None;
        }
        match flag.as_str() {
            "--plan" if plan.is_none() => plan = Some(value.clone()),
            "--expect" if expect.is_none() => expect = Some(value.clone()),
            _ => return None,
        }
    }
    Some(Method::ProfileEnable(ProfileEnableParams {
        plan: plan?,
        expect: expect?,
    }))
}

/// Send only a fully specified enable request, reporting the typed refusal.
pub(super) fn run(args: &[String]) -> std::io::Result<i32> {
    let Some(method) = parse(args) else {
        eprintln!("usage: {USAGE}");
        return Ok(super::exit::USAGE);
    };
    super::print_response(&super::send_request(&Request {
        id: "cli:profile:enable".into(),
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
