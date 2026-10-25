//! Closed diagnostic data (ADR-0041 §4). Input bytes never enter Display/Debug.
//! Keys are rebuilt from catalog metadata; paths, values and parser messages
//! are discarded at construction. Existing String consumers receive only Display.
use super::inventory::{self, Shape};
use std::{fmt, io::ErrorKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Source {
    File,
    Base,
    Environment,
    Overlay,
    Input,
    Output,
    Unavailable,
}
impl Source {
    fn label(self) -> &'static str {
        match self {
            Self::File => "config file",
            Self::Base => "base file",
            Self::Overlay => "overlay file",
            Self::Environment => "environment",
            Self::Input => "input document",
            Self::Output => "output document",
            Self::Unavailable => "unavailable",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "base file" => Self::Base,
            "overlay file" => Self::Overlay,
            "environment" => Self::Environment,
            _ => Self::Unavailable,
        }
    }
}
#[derive(Debug)]
pub(super) enum Cause {
    InvalidToml,
    InvalidType,
    Unknown,
    Removed,
    Read(ErrorKind),
    InvalidCatalog,
    MissingPeerName,
    DuplicatePeer,
    InvalidPeerTarget,
}
#[derive(Debug, Clone, Copy)]
pub(super) enum Outcome {
    KeepCurrent,
    KeepBase,
    Ignore,
    RefuseEdit,
}
impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Self::KeepCurrent => "keeping current config",
            Self::KeepBase => "keeping base config",
            Self::Ignore => "setting ignored",
            Self::RefuseEdit => "leaving config unchanged",
        }
    }
}
/// Safe by construction. No raw string constructor or raw diagnostic accessor.
#[derive(Debug)]
pub(super) struct Diagnostic {
    cause: Cause,
    key: String,
    source: Source,
    outcome: Outcome,
    position: Option<(usize, Option<usize>)>,
    expected: Option<String>,
    suppliers: Vec<Source>,
}
impl Diagnostic {
    fn new(cause: Cause, key: &str, source: Source, outcome: Outcome) -> Self {
        Self {
            cause,
            key: schema_key(key),
            source,
            outcome,
            position: None,
            expected: None,
            suppliers: Vec::new(),
        }
    }
    pub fn syntax(error: &toml::de::Error, text: &str, source: Source, outcome: Outcome) -> Self {
        let mut d = Self::new(Cause::InvalidToml, "", source, outcome);
        d.position = super::diagnostics::syntax_position(text, error.span())
            .map(|(line, col)| (line, Some(col)));
        d.expected = Some("TOML document".into());
        d
    }
    pub fn unknown(key: &str, line: Option<usize>) -> Self {
        let mut d = Self::new(Cause::Unknown, key, Source::File, Outcome::Ignore);
        d.position = line.map(|line| (line, None));
        if key == "toast" {
            d.expected = Some("[ui.toast] section".into());
        }
        d
    }
    pub fn removed(key: &str, line: Option<usize>) -> Self {
        let mut d = Self::new(Cause::Removed, key, Source::File, Outcome::Ignore);
        d.position = line.map(|line| (line, None));
        d
    }
    pub fn with_source(mut self, source: Source) -> Self {
        self.source = source;
        self
    }
    pub fn read(kind: ErrorKind, source: Source, outcome: Outcome) -> Self {
        Self::new(Cause::Read(kind), "", source, outcome)
    }
    pub fn validation(cause: Cause, key: &str) -> Self {
        Self::new(cause, key, Source::File, Outcome::Ignore)
    }
    pub fn catalog() -> Self {
        Self::new(
            Cause::InvalidCatalog,
            "",
            Source::Unavailable,
            Outcome::KeepCurrent,
        )
    }
    pub fn type_error(
        section: &str,
        path: Option<&serde_path_to_error::Path>,
        sources: &toml::Table,
    ) -> Self {
        let mut key = section.to_owned();
        let mut source = sources.get(section);
        if let Some(path) = path {
            for segment in path.iter() {
                match segment {
                    serde_path_to_error::Segment::Map { key: field } => {
                        key.push('.');
                        key.push_str(field);
                        source = source.and_then(|s| s.get(field));
                    }
                    serde_path_to_error::Segment::Seq { index } => {
                        key.push_str(&format!("[{index}]"));
                        source = source.and_then(|s| s.get(*index));
                    }
                    _ => break,
                }
            }
        }
        fn collect(value: &toml::Value, names: &mut std::collections::BTreeSet<Source>) {
            match value {
                toml::Value::String(s) => {
                    names.insert(Source::parse(s));
                }
                toml::Value::Table(t) => {
                    for v in t.values() {
                        collect(v, names);
                    }
                }
                toml::Value::Array(a) => {
                    for v in a {
                        collect(v, names);
                    }
                }
                _ => {}
            }
        }
        let mut names = std::collections::BTreeSet::new();
        if let Some(v) = source {
            collect(v, &mut names);
        }
        let mut d = Self::new(
            Cause::InvalidType,
            &key,
            Source::Unavailable,
            Outcome::KeepCurrent,
        );
        d.suppliers = names.into_iter().collect();
        d.expected = Some(inventory::diagnostic_type(section, path));
        d
    }
}
impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = match self.cause {
            Cause::InvalidToml => "config parse error: invalid_toml",
            Cause::InvalidType => "invalid_config_type",
            Cause::Unknown => "unknown_config_key",
            Cause::Removed => "removed_config_key",
            Cause::Read(_) => "read_error",
            Cause::InvalidCatalog => "invalid_catalog",
            Cause::MissingPeerName => "missing_peer_name",
            Cause::DuplicatePeer => "duplicate_peer",
            Cause::InvalidPeerTarget => "invalid_peer_target",
        };
        write!(
            f,
            "{code}; key={}; source={}",
            self.key,
            self.source.label()
        )?;
        match self.position {
            Some((line, Some(column))) => write!(f, "; at {line}:{column}")?,
            Some((line, None)) => write!(f, "; at line {line}")?,
            None => write!(f, "; location unavailable")?,
        }
        if let Cause::Read(kind) = self.cause {
            write!(f, "; io_kind={kind:?}")?;
        }
        if let Some(expected) = &self.expected {
            write!(f, "; expected {expected}")?;
        }
        if !self.suppliers.is_empty() {
            write!(
                f,
                "; supplying layer={}",
                self.suppliers
                    .iter()
                    .map(|s| s.label())
                    .collect::<Vec<_>>()
                    .join(", ")
            )?;
        }
        if matches!(self.cause, Cause::Removed) {
            write!(f, "; was removed (mesh); delete this line")?;
        }
        write!(f, "; {}", self.outcome.label())
    }
}
/// Return a trusted schema shape, never an input key. Unknown descendants stop
/// at the nearest declared container. User map entries become `<key>`, indices
/// become `[]`. Removed keys are a closed, historical schema exception.
pub(super) fn schema_key(raw: &str) -> String {
    const REMOVED: &[&str] = &[
        "msg.uplink_timeout_secs",
        "msg.uplink_heartbeat_secs",
        "msg.deferral_relay_concurrency",
        "peers.summary_command",
    ];
    if let Some(key) = REMOVED.iter().find(|&&k| k == raw) {
        return (*key).into();
    }
    let Ok(catalog) = inventory::core_catalog() else {
        return "document root".into();
    };
    let normalized = raw.replace("[]", "[0]").replace('[', ".").replace(']', "");
    let mut canonical = String::new();
    let mut current = None;
    for part in normalized.split('.').filter(|s| !s.is_empty()) {
        let map_entry = current.is_some_and(|f: &inventory::Field| f.shape == Shape::Map);
        let index = part.parse::<usize>().is_ok();
        let next = if map_entry {
            format!("{canonical}<key>")
        } else if index {
            format!("{canonical}[]")
        } else if canonical.is_empty() {
            part.to_owned()
        } else {
            format!("{canonical}.{part}")
        };
        let field = catalog
            .fields
            .iter()
            .find(|f| f.path == next || f.file_aliases.contains(&next));
        if let Some(field) = field {
            canonical = field.path.clone();
            current = Some(field);
        } else if map_entry || index {
            canonical = next;
            current = None;
        } else {
            break;
        }
    }
    if canonical.is_empty() {
        "document root".into()
    } else {
        canonical
    }
}

/// Safe refusal for both input parsing and the edited-document output guard.
pub(crate) fn edit_parse_error(error: &toml::de::Error, text: &str, output: bool) -> String {
    Diagnostic::syntax(
        error,
        text,
        if output {
            Source::Output
        } else {
            Source::Input
        },
        Outcome::RefuseEdit,
    )
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schema_shapes_never_echo_adversarial_map_or_unknown_keys() {
        for path in [
            "SECRET<key>",
            "ui.SECRET<key>",
            "SECRET[0].name",
            "ui.agent_aliases.SECRET<key>",
            "session.restart_agents.SECRET.max_restarts",
            "ui.SECRET[1]",
        ] {
            assert!(!schema_key(path).contains("SECRET"), "{path}");
        }
        for (input, output) in [
            ("peers[7].ssh", "peers[].ssh"),
            ("ui.agent_aliases.private", "ui.agent_aliases<key>"),
            ("ui.sidebar_width", "ui.sidebar_width"),
            (
                "session.restart_agents.private.max_restarts",
                "session.restart_agents<key>.max_restarts",
            ),
        ] {
            assert_eq!(schema_key(input), output);
        }
    }
    #[test]
    fn custom_io_messages_and_unknown_source_layers_cannot_enter_safe_output() {
        let error = std::io::Error::other("SECRET_MESSAGE_2384");
        let d = Diagnostic::read(error.kind(), Source::Overlay, Outcome::KeepBase);
        assert!(!format!("{d} {d:?}").contains("SECRET"));
        assert!(d.to_string().contains("io_kind=Other"));
        let sources = toml::from_str("[ui]\nsidebar_width='SECRET_LAYER_2384'").unwrap();
        let d = Diagnostic::type_error("ui", None, &sources);
        assert!(!format!("{d} {d:?}").contains("SECRET"));
        assert!(d.to_string().contains("supplying layer=unavailable"));
    }
}
