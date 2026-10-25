//! Operator CLI for exact live-runtime grants (ADR-0036 S6).
#![expect(clippy::print_stderr, reason = "CLI usage surface")]
use crate::{
    api::schema::{Method, Request},
    structured::permissions::grants::{GrantsList, GrantsRevoke},
};
pub(super) const USAGE: &str = "flk structured grants list <agent_id> | flk structured grants revoke <agent_id> <generation> <request_id> <request_seq>";
fn parse(args: &[String]) -> Option<Method> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["grants", "list", agent] => Some(Method::StructuredGrantsList(GrantsList {
            agent_id: (*agent).into(),
        })),
        ["grants", "revoke", agent, generation, request, seq] => {
            Some(Method::StructuredGrantsRevoke(GrantsRevoke {
                agent_id: (*agent).into(),
                generation: generation.parse().ok()?,
                request_id: (*request).into(),
                request_seq: seq.parse().ok()?,
            }))
        }
        _ => None,
    }
}
pub(super) fn run(args: &[String]) -> std::io::Result<i32> {
    let Some(method) = parse(args) else {
        eprintln!("usage: {USAGE}");
        return Ok(super::exit::USAGE);
    };
    super::print_response(&super::send_request(&Request {
        id: "cli:structured:grants".into(),
        method,
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grant_commands_are_typed_and_revoke_is_not_retry_safe() {
        let list = parse(&["grants".into(), "list".into(), "fixture-agent".into()]).unwrap();
        assert!(list.is_retry_safe());
        let args: Vec<_> = ["grants", "revoke", "fixture-agent", "7", "request-one", "9"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let method = parse(&args).unwrap();
        assert!(!method.is_retry_safe());
        assert!(method.needs_current_protocol());
        assert_eq!(
            serde_json::to_value(method).unwrap()["params"],
            serde_json::json!({"agent_id":"fixture-agent","generation":7,"request_id":"request-one","request_seq":9})
        );
        let mut bad = args;
        bad[3] = "invalid".into();
        assert!(parse(&bad).is_none());
    }
}
