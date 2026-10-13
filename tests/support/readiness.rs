//! Strict startup-announcement parser shared with #1314.
//! Callers bound the reader and own child cleanup on failure.

use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

const READY: &str = "flk server running; you can use any flk CLI command in another terminal.";

/// Read the existing server banner and socket path in order, accepting PTY CRLF.
///
/// A socket path alone also appears in startup errors, so it is insufficient.
/// The client-socket line follows the API line in the successful announcement.
/// Every mismatch after the banner fails immediately. Existing socket endpoints
/// also compare by filesystem canonicalization: the server prints explicit
/// environment overrides verbatim, including paths through symlinks.
pub fn read_announcement(reader: impl Read, api_socket: &Path) -> io::Result<()> {
    let mut reader = BufReader::new(reader);
    let expected_api = format!("api socket: {}", api_socket.display());
    let mut output = String::new();
    let mut stage = 0;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("output closed before readiness\n{output}"),
                ));
            }
            Err(err) => {
                return Err(io::Error::new(err.kind(), format!("{err}\n{output}")));
            }
            Ok(_) => {}
        }
        output.push_str(&line);
        let line = line.trim_end_matches(['\r', '\n']);
        stage = match stage {
            0 => usize::from(line == READY),
            1 => {
                let matches = line.strip_prefix("api socket: ").is_some_and(|path| {
                    let path = Path::new(path);
                    path == api_socket
                        || matches!(
                            (path.canonicalize(), api_socket.canonicalize()),
                            (Ok(actual), Ok(expected)) if actual == expected
                        )
                });
                if !matches {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("expected {expected_api} after ready banner\n{output}"),
                    ));
                }
                2
            }
            _ => {
                if line
                    .strip_prefix("client socket: ")
                    .is_some_and(|path| !path.is_empty())
                {
                    return Ok(());
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("expected client socket after API socket\n{output}"),
                ));
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_reports_eof_before_the_complete_announcement() {
        for output in [String::new(), format!("{READY}\r\n")] {
            let error = read_announcement(output.as_bytes(), Path::new("fixture.sock"))
                .expect_err("an incomplete announcement must fail");
            assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
            assert!(error.to_string().contains(&output));
        }
    }

    #[test]
    fn readiness_rejects_mismatch_without_reading_again() {
        struct NoFurtherRead;
        impl Read for NoFurtherRead {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                panic!("a malformed announcement must fail before another read")
            }
        }
        for output in [
            format!("{READY}\r\nunexpected line\r\n"),
            format!("{READY}\r\napi socket: fixture.sock\r\nlogs: wrong order\r\n"),
        ] {
            let reader = io::Cursor::new(output.as_bytes()).chain(NoFurtherRead);
            let error = read_announcement(reader, Path::new("fixture.sock"))
                .expect_err("an unexpected stage must fail");
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains(&output));
        }
    }
}
