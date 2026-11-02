//! Typed launch diagnostics, preserved through API message adapters (ADR-0029).
use std::{fmt, io, ops::Deref, path::PathBuf};

/// A failed launch, distinct from a child exiting unsuccessfully.
#[derive(Debug)]
pub(super) struct SpawnFailure {
    pub program: String,
    pub path_length: usize,
    pub current_dir: Option<PathBuf>,
    pub source: io::Error,
}

impl SpawnFailure {
    fn data(&self) -> serde_json::Map<String, serde_json::Value> {
        serde_json::Map::from_iter([
            (
                "reason".into(),
                serde_json::json!(if self.current_dir.is_some() {
                    "current_dir_not_found"
                } else {
                    "program_not_found"
                }),
            ),
            ("program".into(), serde_json::json!(self.program)),
            ("path_length".into(), serde_json::json!(self.path_length)),
            ("current_dir".into(), serde_json::json!(self.current_dir)),
        ])
    }
}

impl fmt::Display for SpawnFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(dir) = &self.current_dir {
            write!(
                f,
                "cannot start {:?}: working directory {:?} not found",
                self.program, dir
            )
        } else {
            write!(f, "program {:?} was not found on the server's PATH ({} bytes). Run `flk preflight` to check dependencies", self.program, self.path_length)
        }
    }
}

impl std::error::Error for SpawnFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// An error message with optional machine-readable launch evidence.
/// Plain filesystem and child-exit messages retain their original text and data.
#[derive(Clone, Debug)]
pub(crate) struct CommandFailure {
    pub message: String,
    pub data: Option<serde_json::Map<String, serde_json::Value>>,
}

impl CommandFailure {
    pub(crate) fn with_message(mut self, message: String) -> Self {
        self.message = message;
        self
    }
}
impl std::error::Error for CommandFailure {}

impl From<io::Error> for CommandFailure {
    fn from(error: io::Error) -> Self {
        let data = error
            .get_ref()
            .and_then(|e| e.downcast_ref::<SpawnFailure>())
            .map(SpawnFailure::data);
        Self {
            message: error.to_string(),
            data,
        }
    }
}
impl From<String> for CommandFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            data: None,
        }
    }
}
impl From<&str> for CommandFailure {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}
impl From<&String> for CommandFailure {
    fn from(message: &String) -> Self {
        message.clone().into()
    }
}
impl From<&CommandFailure> for CommandFailure {
    fn from(error: &CommandFailure) -> Self {
        error.clone()
    }
}
impl From<CommandFailure> for String {
    fn from(error: CommandFailure) -> Self {
        error.message
    }
}
impl Deref for CommandFailure {
    type Target = str;
    fn deref(&self) -> &str {
        &self.message
    }
}
impl fmt::Display for CommandFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(f)
    }
}
impl PartialEq<&str> for CommandFailure {
    fn eq(&self, other: &&str) -> bool {
        self.message == *other
    }
}
impl PartialEq<String> for CommandFailure {
    fn eq(&self, other: &String) -> bool {
        self.message == *other
    }
}
