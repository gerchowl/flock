#![expect(
    clippy::print_stderr,
    reason = "CLI output surface: this module's job is stdout/stderr for humans and scripts"
)]
use crate::api::schema::{
    Method, NotificationAckParams, NotificationListParams, NotificationShowParams,
    NotificationShowSound, Request,
};
use crate::config::ToastFlockPosition;

pub(super) fn run_notification_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_notification_help();
        return Ok(2);
    };

    match subcommand {
        "show" => notification_show(&args[1..]),
        "list" => notification_list(&args[1..]),
        "read" => notification_read(&args[1..]),
        "help" | "--help" | "-h" => {
            print_notification_help();
            Ok(0)
        }
        _ => {
            print_notification_help();
            Ok(2)
        }
    }
}

fn notification_show(args: &[String]) -> std::io::Result<i32> {
    let params = match parse_notification_show_args(args) {
        Ok(params) => params,
        Err(NotificationShowArgError::Usage) => {
            eprintln!(
                "usage: flk notification show <title> [--body TEXT] [--position top-left|top-right|bottom-left|bottom-right] [--sound none|done|request]"
            );
            return Ok(2);
        }
        Err(NotificationShowArgError::Message(message)) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    super::print_response(&super::send_request(&Request {
        id: "cli:notification:show".into(),
        method: Method::NotificationShow(params),
    })?)
}

/// `flk notification list` (#372) — the outcomes that outlived their toasts.
/// Listing never acknowledges: reading the list is not the same as having
/// dealt with what is in it, and acknowledging is what lets retention drop a
/// record.
fn notification_list(args: &[String]) -> std::io::Result<i32> {
    let mut params = NotificationListParams::default();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--unread" => {
                params.unread_only = true;
                index += 1;
            }
            "--limit" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --limit");
                    return Ok(2);
                };
                let Ok(limit) = value.parse::<usize>() else {
                    eprintln!("invalid --limit: {value}");
                    return Ok(2);
                };
                params.limit = Some(limit);
                index += 2;
            }
            "help" | "--help" | "-h" => {
                eprintln!("usage: flk notification list [--unread] [--limit N]");
                return Ok(2);
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:notification:list".into(),
        method: Method::NotificationList(params),
    })?)
}

/// `flk notification read <id>` / `--all` (#372) — acknowledge.
fn notification_read(args: &[String]) -> std::io::Result<i32> {
    let mut params = NotificationAckParams::default();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--all" => {
                params.all = true;
                index += 1;
            }
            "help" | "--help" | "-h" => {
                eprintln!("usage: flk notification read <notification-id> | --all");
                return Ok(2);
            }
            other if params.notification_id.is_none() && !other.starts_with("--") => {
                params.notification_id = Some(other.to_string());
                index += 1;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }
    if params.notification_id.is_none() && !params.all {
        eprintln!("usage: flk notification read <notification-id> | --all");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:notification:read".into(),
        method: Method::NotificationAck(params),
    })?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NotificationShowArgError {
    Usage,
    Message(String),
}

fn parse_notification_show_args(
    args: &[String],
) -> Result<NotificationShowParams, NotificationShowArgError> {
    let Some(title) = args.first().cloned() else {
        return Err(NotificationShowArgError::Usage);
    };
    if matches!(title.as_str(), "help" | "--help" | "-h") {
        return Err(NotificationShowArgError::Usage);
    }

    let mut body = None;
    let mut position = None;
    let mut sound = NotificationShowSound::None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--body" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(NotificationShowArgError::Message(
                        "missing value for --body".into(),
                    ));
                };
                body = Some(value.clone());
                index += 2;
            }
            "--position" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(NotificationShowArgError::Message(
                        "missing value for --position".into(),
                    ));
                };
                position = Some(parse_toast_position(value)?);
                index += 2;
            }
            "--sound" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(NotificationShowArgError::Message(
                        "missing value for --sound".into(),
                    ));
                };
                sound = parse_notification_sound(value)?;
                index += 2;
            }
            other => {
                return Err(NotificationShowArgError::Message(format!(
                    "unknown option: {other}"
                )));
            }
        }
    }

    Ok(NotificationShowParams {
        title,
        body,
        position,
        sound,
    })
}

fn parse_toast_position(value: &str) -> Result<ToastFlockPosition, NotificationShowArgError> {
    match value {
        "top-left" => Ok(ToastFlockPosition::TopLeft),
        "top-right" => Ok(ToastFlockPosition::TopRight),
        "bottom-left" => Ok(ToastFlockPosition::BottomLeft),
        "bottom-right" => Ok(ToastFlockPosition::BottomRight),
        _ => Err(NotificationShowArgError::Message(format!(
            "invalid position: {value} (expected top-left, top-right, bottom-left, or bottom-right)"
        ))),
    }
}

fn parse_notification_sound(
    value: &str,
) -> Result<NotificationShowSound, NotificationShowArgError> {
    match value {
        "none" => Ok(NotificationShowSound::None),
        "done" => Ok(NotificationShowSound::Done),
        "request" => Ok(NotificationShowSound::Request),
        _ => Err(NotificationShowArgError::Message(format!(
            "invalid sound: {value} (expected none, done, or request)"
        ))),
    }
}

fn print_notification_help() {
    eprintln!("flk notification commands:");
    eprintln!(
        "  flk notification show <title> [--body TEXT] [--position top-left|top-right|bottom-left|bottom-right] [--sound none|done|request]"
    );
    eprintln!("  flk notification list [--unread] [--limit N]");
    eprintln!("  flk notification read <notification-id> | --all");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn notification_show_args_parse_title_body_and_position() {
        let params = parse_notification_show_args(&args(&[
            "build failed",
            "--body",
            "api workspace",
            "--position",
            "top-right",
            "--sound",
            "request",
        ]))
        .unwrap();

        assert_eq!(
            params,
            NotificationShowParams {
                title: "build failed".into(),
                body: Some("api workspace".into()),
                position: Some(ToastFlockPosition::TopRight),
                sound: NotificationShowSound::Request,
            }
        );
    }

    #[test]
    fn notification_show_args_reject_invalid_position() {
        let error =
            parse_notification_show_args(&args(&["build failed", "--position", "top-center"]))
                .unwrap_err();

        assert_eq!(
            error,
            NotificationShowArgError::Message(
                "invalid position: top-center (expected top-left, top-right, bottom-left, or bottom-right)"
                    .into()
            )
        );
    }

    #[test]
    fn notification_show_args_default_sound_is_none() {
        let params = parse_notification_show_args(&args(&["build failed"])).unwrap();

        assert_eq!(params.sound, NotificationShowSound::None);
    }

    #[test]
    fn notification_show_args_reject_invalid_sound() {
        let error =
            parse_notification_show_args(&args(&["build failed", "--sound", "loud"])).unwrap_err();

        assert_eq!(
            error,
            NotificationShowArgError::Message(
                "invalid sound: loud (expected none, done, or request)".into()
            )
        );
    }
}
