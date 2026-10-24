//! Decode the text CLI without losing native handoff socket paths.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use crate::api::error_code::ErrorCode;

pub(crate) struct Arguments {
    pub(crate) text: Vec<String>,
    pub(crate) handoff_socket: Option<PathBuf>,
}

#[derive(Debug)]
pub(crate) struct InvalidUtf8Argument(usize);

impl std::fmt::Display for InvalidUtf8Argument {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: argument {} must be valid UTF-8",
            ErrorCode::InvalidParams,
            self.0
        )
    }
}

impl std::error::Error for InvalidUtf8Argument {}

impl From<InvalidUtf8Argument> for std::io::Error {
    fn from(error: InvalidUtf8Argument) -> Self {
        Self::new(std::io::ErrorKind::InvalidInput, error)
    }
}

pub(crate) fn text_argument(index: usize, arg: OsString) -> Result<String, InvalidUtf8Argument> {
    arg.into_string().map_err(|_| InvalidUtf8Argument(index))
}

/// Decode command text after session and remote globals have been stripped.
/// The normalized importer form alone reserves a native socket argument.
pub(crate) fn collect(
    args: impl Iterator<Item = OsString>,
) -> Result<Arguments, InvalidUtf8Argument> {
    let args: Vec<_> = args.collect();
    let handoff = args.get(1).map(OsString::as_os_str) == Some(OsStr::new("server"))
        && args.get(2).map(OsString::as_os_str) == Some(OsStr::new("--handoff-import"));
    let mut parsed = Arguments {
        text: Vec::with_capacity(args.len()),
        handoff_socket: None,
    };
    for (index, arg) in args.into_iter().enumerate() {
        // The handoff importer consumes a native socket path, not CLI text.
        // Keep it out of the String parser; the optional legacy token stays text.
        if handoff && index == 3 {
            parsed.handoff_socket = Some(PathBuf::from(arg));
        } else {
            parsed.text.push(text_argument(index, arg)?);
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn handoff_socket_retains_native_bytes_and_legacy_token_stays_text() {
        let path = OsString::from_vec(b"socket-\xff".to_vec());
        let parsed = collect(
            [
                "flk".into(),
                "server".into(),
                "--handoff-import".into(),
                path.clone(),
                "token".into(),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(parsed.handoff_socket.unwrap().as_os_str(), path);
        assert_eq!(parsed.text, ["flk", "server", "--handoff-import", "token"]);
    }

    #[test]
    fn handoff_token_requires_utf8() {
        let error = collect(
            [
                "flk".into(),
                "server".into(),
                "--handoff-import".into(),
                "socket".into(),
                OsString::from_vec(vec![0xff]),
            ]
            .into_iter(),
        )
        .err()
        .unwrap();
        let error: std::io::Error = error.into();
        assert!(error.get_ref().unwrap().is::<InvalidUtf8Argument>());
        assert_eq!(
            error.to_string(),
            "invalid_params: argument 4 must be valid UTF-8"
        );
    }
}
