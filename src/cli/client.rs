//! Local-operator client credentials and browser entry (ADR-0036 §5).
#![expect(
    clippy::print_stdout,
    reason = "CLI prints explicit operator-requested URLs and usage"
)]
use super::{exit, output};
use crate::api::error_code::ErrorCode::{InternalError, InvalidParams};
use crate::api::{
    client_tokens::{AddParams, RemoveParams},
    schema::{EmptyParams, Method, Request},
};

const USAGE: &str = "flk client token add NAME --scopes read,send,answer,operate | token rm NAME | token rm --entry ID | token list | open [--print]";
pub(super) fn run(args: &[String]) -> std::io::Result<i32> {
    if super::help::asks_for_help(args) || args.first().is_some_and(|s| s == "help") {
        println!("usage: {USAGE}");
        return Ok(exit::SUCCESS);
    }
    let Some(method) = parse(args) else {
        return Ok(output::failure(InvalidParams, USAGE, exit::USAGE));
    };
    let open = matches!(method, Method::ClientOpen(_));
    let response = super::send_request(&Request {
        id: "cli:client".into(),
        method,
    })?;
    if response.get("error").is_some() || !open {
        return super::print_response(&response);
    }
    let failure = |message| Ok(output::failure(InternalError, message, exit::FAILURE));
    let Some(url) = response["result"]["url"].as_str() else {
        return failure("client open returned no URL");
    };
    let print = args.iter().any(|s| s == "--print");
    if !print && crate::platform::open_url(url).is_err() {
        return failure("could not open browser; use client open --print");
    }
    if output::json() && !print {
        return super::print_response(&response);
    }
    if print {
        println!("{url}");
    }
    Ok(exit::SUCCESS)
}
fn parse(args: &[String]) -> Option<Method> {
    let args: Vec<_> = args.iter().map(String::as_str).collect();
    Some(match args.as_slice() {
        ["open"] | ["open", "--print"] => Method::ClientOpen(EmptyParams {}),
        ["token", "list"] => Method::ClientTokenList(EmptyParams {}),
        ["token", "rm", name] => Method::ClientTokenRemove(RemoveParams {
            name: Some((*name).into()),
            entry: None,
        }),
        ["token", "rm", "--entry", entry] => Method::ClientTokenRemove(RemoveParams {
            name: None,
            entry: Some((*entry).into()),
        }),
        ["token", "add", name, "--scopes", scopes] if !scopes.is_empty() => {
            Method::ClientTokenAdd(AddParams {
                name: (*name).into(),
                scopes: scopes
                    .split(',')
                    .map(|s| serde_json::from_value(serde_json::json!(s)))
                    .collect::<Result<_, _>>()
                    .ok()?,
            })
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_operator_cli_parses_exact_scopes_and_exclusive_revocation_targets() {
        let parse =
            |args: &[&str]| super::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(matches!(
            parse(&["open", "--print"]),
            Some(Method::ClientOpen(_))
        ));
        assert!(
            matches!(parse(&["token", "add", "phone", "--scopes", "read,operate"]), Some(Method::ClientTokenAdd(p)) if p.scopes.len() == 2)
        );
        assert!(parse(&["token", "add", "phone", "--scopes", "operator"]).is_none());
        assert!(parse(&["token", "rm", "phone", "--entry", "aa"]).is_none());
        assert!(parse(&["open", "--print", "http://example.test"]).is_none());
    }
}
