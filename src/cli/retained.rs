//! Operator inspection, explicit purge confirmation and retention doctor surface.
#![expect(clippy::print_stdout, reason = "operator CLI output")]
use super::{exit, output};
use crate::api::{
    error_code::ErrorCode,
    retained::{PurgeParams, Selection},
    schema::{EmptyParams, Method, Request, ResponseResult},
};
pub(super) const USAGE: &str =
    "flk session retained list [--json] | purge (--dead | --all --yes) [--older-than <dur>]";
pub(super) fn run(args: &[String], doctor: bool) -> std::io::Result<i32> {
    if super::help::asks_for_help(args) || args.first().is_some_and(|s| s == "help") {
        println!(
            "usage: {}",
            if doctor { "flk doctor [--json]" } else { USAGE }
        );
        return Ok(exit::SUCCESS);
    }
    let method = select(args, doctor);
    let Some(method) = method else {
        return Ok(output::failure(
            ErrorCode::InvalidParams,
            if doctor { "flk doctor [--json]" } else { USAGE },
            exit::USAGE,
        ));
    };
    let response = super::send_request(&Request {
        id: "cli:retained".into(),
        method,
    })?;
    if output::json() || response.get("error").is_some() {
        return super::print_response(&response);
    }
    let result: ResponseResult =
        serde_json::from_value(response["result"].clone()).map_err(std::io::Error::other)?;
    println!(
        "{}",
        text(&result, doctor)
            .ok_or_else(|| std::io::Error::other("unexpected retained response"))?
    );
    Ok(exit::SUCCESS)
}
fn select(args: &[String], doctor: bool) -> Option<Method> {
    if doctor {
        return args
            .is_empty()
            .then_some(Method::SessionRetainedList(EmptyParams {}));
    }
    parse(args)
}
fn parse(args: &[String]) -> Option<Method> {
    if args == ["list"] {
        return Some(Method::SessionRetainedList(EmptyParams {}));
    }
    if args.first()?.as_str() != "purge" {
        return None;
    }
    let mut selection = None;
    let mut yes = false;
    let mut older_than_ms = None;
    let mut iter = args[1..].iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--dead" | "--all" if selection.is_none() => {
                selection = Some(if arg == "--all" {
                    Selection::All
                } else {
                    Selection::Dead
                })
            }
            "--yes" if !yes => yes = true,
            "--older-than" if older_than_ms.is_none() => {
                older_than_ms =
                    Some(crate::detect::provider_limit::parse_duration(iter.next()?).ok()?)
            }
            _ => return None,
        }
    }
    let selection = selection?;
    if selection == Selection::All && !yes {
        return None;
    }
    Some(Method::SessionRetainedPurge(PurgeParams {
        selection,
        yes,
        older_than_ms,
    }))
}
fn text(result: &ResponseResult, doctor: bool) -> Option<String> {
    let ResponseResult::Retained {
        records,
        forward,
        dead,
        removed,
        purge_hint,
    } = result
    else {
        return None;
    };
    let mut text =
        format!("retained structured records: FORWARD {forward}, DEAD {dead}; removed {removed}");
    if !doctor {
        for row in records {
            text.push_str(&format!(
                "\nslot={} kind={:?} first_seen_ms={}",
                row.slot,
                row.kind,
                row.first_seen_ms
                    .map_or_else(|| "unstamped".into(), |n| n.to_string())
            ));
        }
    }
    if let Some(hint) = purge_hint {
        text.push_str(&format!("\n{hint}"));
    }
    Some(text)
}

#[cfg(test)]
mod tests;
