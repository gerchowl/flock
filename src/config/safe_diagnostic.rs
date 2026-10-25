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
    Validation(super::validation::Cause),
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
    DisableBinding,
    DisableCommand,
    Fallback,
    DefaultSound,
    Clamp,
    KeepCurrent,
    KeepBase,
    Ignore,
    RefuseEdit,
    DropEntry,
}
impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Self::DisableCommand => "disabling custom command",
            Self::DisableBinding => "disabling binding",
            Self::Fallback => "using fallback",
            Self::DefaultSound => "using default sound",
            Self::Clamp => "using clamped value",
            Self::KeepCurrent => "keeping current config",
            Self::KeepBase => "keeping base config",
            Self::Ignore => "setting ignored",
            Self::RefuseEdit => "leaving config unchanged",
            Self::DropEntry => "entry dropped",
        }
    }
}
/// Only static, reviewed hints may supplement a safe diagnostic.
#[derive(Debug)]
enum Hint {
    ToastSection,
    MeshEdge,
    RepairInput,
    OwnEdit,
}
impl Hint {
    fn label(&self) -> &'static str {
        match self {
            Self::ToastSection => "did you mean [ui.toast]?",
            Self::MeshEdge => "peers now always hold a mesh edge",
            Self::RepairInput => "Fix it manually or move it aside",
            Self::OwnEdit => "flock's own edit produced invalid TOML",
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
    hint: Option<Hint>,
    entry_index: Option<usize>,
    related: Option<(String, Option<usize>)>,
    item_index: Option<usize>,
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
            hint: None,
            entry_index: None,
            related: None,
            item_index: None,
        }
    }
    pub fn validation(cause: super::validation::Cause, key: &str, index: Option<usize>) -> Self {
        let mut d = Self::new(
            Cause::Validation(cause),
            key,
            Source::Unavailable,
            cause.description().1,
        );
        d.entry_index = index.or_else(|| declaration_index(key));
        if let super::validation::Cause::Interval { minimum } = cause {
            d.expected = Some(format!("interval_secs >= {minimum}"));
        }
        d
    }
    pub fn with_related(mut self, key: &str) -> Self {
        self.related = Some((
            if key == "navigate reserved keys" {
                key.into()
            } else {
                schema_key(key)
            },
            declaration_index(key),
        ));
        self
    }
    pub fn with_item_index(mut self, index: usize) -> Self {
        self.item_index = Some(index);
        self
    }
    pub fn fallback(mut self) -> Self {
        self.outcome = Outcome::Fallback;
        self
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
            d.hint = Some(Hint::ToastSection);
        }
        d
    }
    pub fn removed(key: &str, line: Option<usize>) -> Self {
        let mut d = Self::new(Cause::Removed, key, Source::File, Outcome::Ignore);
        d.position = line.map(|line| (line, None));
        if key == "peers.summary_command" {
            d.hint = Some(Hint::MeshEdge);
        }
        d
    }
    pub fn with_source(mut self, source: Source) -> Self {
        self.source = source;
        self
    }
    pub fn read(kind: ErrorKind, source: Source, outcome: Outcome) -> Self {
        Self::new(Cause::Read(kind), "", source, outcome)
    }
    pub fn dropped_entry(cause: Cause, key: &str, index: usize) -> Self {
        let mut d = Self::new(cause, key, Source::File, Outcome::DropEntry);
        d.entry_index = Some(index);
        d
    }
    /// Numeric coordinates only; callers must not guess a missing position.
    pub fn with_position(mut self, position: Option<(usize, usize)>) -> Self {
        self.position = position.map(|(line, column)| (line, Some(column)));
        self
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
        let cause_label = match self.cause {
            Cause::Validation(cause) => cause.description().0,
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
            "{cause_label}; key={}; source={}",
            self.key,
            self.source.label()
        )?;
        match self.position {
            Some((line, Some(column))) => write!(f, "; at {line}:{column}")?,
            Some((line, None)) => write!(f, "; at line {line}")?,
            None => write!(f, "; location unavailable")?,
        }
        if let Some(index) = self.entry_index {
            write!(f, "; entry_index={index}")?;
        }
        if let Some(index) = self.item_index {
            write!(f, "; item_index={index}")?;
        }
        if let Some((key, index)) = &self.related {
            write!(f, "; kept {key}")?;
            if let Some(index) = index {
                write!(f, "; kept_entry_index={index}")?;
            }
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
        if let Some(hint) = &self.hint {
            write!(f, "; {}", hint.label())?;
        }
        write!(f, "; {}", self.outcome.label())
    }
}
fn declaration_index(key: &str) -> Option<usize> {
    key.split_once('[')?.1.split_once(']')?.0.parse().ok()
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
    let mut d = Diagnostic::syntax(
        error,
        text,
        if output {
            Source::Output
        } else {
            Source::Input
        },
        Outcome::RefuseEdit,
    );
    d.hint = Some(if output {
        Hint::OwnEdit
    } else {
        Hint::RepairInput
    });
    d.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cause_labels_are_closed_documented_diagnostics_not_error_codes() {
        let mut cases = vec![
            (Cause::InvalidToml, "config parse error: invalid_toml"),
            (Cause::InvalidType, "invalid_config_type"),
            (Cause::Unknown, "unknown_config_key"),
            (Cause::Removed, "removed_config_key"),
            (Cause::Read(ErrorKind::Other), "read_error"),
            (Cause::InvalidCatalog, "invalid_catalog"),
            (Cause::MissingPeerName, "missing_peer_name"),
            (Cause::DuplicatePeer, "duplicate_peer"),
            (Cause::InvalidPeerTarget, "invalid_peer_target"),
        ];
        use crate::config::validation::Cause as ValidationCause;
        let validation_cases = [
            (ValidationCause::InvalidBinding, "invalid keybinding"),
            (ValidationCause::Range, "range keybinding is only valid for indexed actions"),
            (ValidationCause::InvalidModifier, "invalid indexed keybinding: expected modifier combination"),
            (ValidationCause::Indexed, "indexed keybinding must use 1..9"),
            (ValidationCause::NavigatePrefix, "navigate keybinding must not include prefix"),
            (ValidationCause::NavigateEsc, "navigate keybinding cannot use esc"),
            (ValidationCause::Reserved, "reserved keybinding: uses keys.prefix; pressing the prefix twice sends a literal prefix key"),
            (ValidationCause::Conflict, "conflicting keybinding: first binding kept"),
            (ValidationCause::DirectPrintable, "unsafe direct keybinding: would intercept typing; require the prefix"),
            (ValidationCause::EmptyCommand, "empty custom command"),
            (ValidationCause::SoundFormat, "unsupported sound file format; expected an mp3 file"),
            (ValidationCause::SoundMissing, "missing sound file"),
            (ValidationCause::SoundInvalid, "invalid sound file: expected a regular file"),
            (ValidationCause::UnknownAgent, "unknown agent: not an agent flock detects"),
            (ValidationCause::AgentAlias, "duplicate agent alias: first spelling kept"),
            (ValidationCause::EmptyEnv, "environment key is empty"),
            (ValidationCause::EqualsEnv, "environment key contains '=', which is not part of a variable name"),
            (ValidationCause::WildcardEnv, "environment key looks like a wildcard; only exact names are matched"),
            (ValidationCause::WhitespaceEnv, "environment key contains whitespace, which no shell can export"),
            (ValidationCause::MissingName, "entry is missing name"),
            (ValidationCause::DuplicateName, "duplicate name: later entry ignored"),
            (ValidationCause::Interval { minimum: 1 }, "interval_secs is less than checks.min_tick_secs; clamped"),
            (ValidationCause::Timeout, "timeout_secs is less than 1; using 1s"),
            (ValidationCause::Debounce, "debounce is 0; using 1 so a single fire still emits"),
            (ValidationCause::Program, "program is not absolute and not under ~/.flock/checks/"),
            (ValidationCause::ChannelName, "invalid channel name: needs 1-64 characters from [A-Za-z0-9_-]"),
            (ValidationCause::ChannelOwner, "invalid channel entry: needs an owner_node"),
            (ValidationCause::ChannelDuplicate, "invalid channel entry: needs a unique name"),
            (ValidationCause::ChannelTarget, "invalid channel target: expected fleet, repo:<name> or agent:<id>"),
        ];
        let validation_source = syn::parse_file(include_str!("validation.rs")).unwrap();
        let validation_variants = validation_source
            .items
            .iter()
            .find_map(|item| match item {
                syn::Item::Enum(item) if item.ident == "Cause" => Some(item.variants.len()),
                _ => None,
            })
            .unwrap();
        let validation_covered: std::collections::HashSet<_> = validation_cases
            .iter()
            .map(|(cause, _)| std::mem::discriminant(cause))
            .collect();
        assert_eq!(
            validation_covered.len(),
            validation_variants,
            "every validation Cause needs a documented label fixture"
        );
        cases.extend(
            validation_cases
                .into_iter()
                .map(|(cause, label)| (Cause::Validation(cause), label)),
        );
        let source = syn::parse_file(include_str!("safe_diagnostic.rs")).unwrap();
        let variants = source
            .items
            .iter()
            .find_map(|item| match item {
                syn::Item::Enum(item) if item.ident == "Cause" => Some(item.variants.len()),
                _ => None,
            })
            .unwrap();
        let covered: std::collections::HashSet<_> = cases
            .iter()
            .map(|(cause, _)| std::mem::discriminant(cause))
            .collect();
        assert_eq!(
            covered.len(),
            variants,
            "every Cause needs a documented label fixture"
        );
        let mut labels = std::collections::BTreeSet::new();
        for (cause, label) in cases {
            assert!(labels.insert(label), "cause labels must be distinct");
            let text =
                Diagnostic::new(cause, "", Source::Unavailable, Outcome::KeepCurrent).to_string();
            assert_eq!(text.split_once("; key=").unwrap().0, label);
            for document in [
                include_str!("../../docs/adr/0041-configuration-v2.md"),
                include_str!("inventory/README.md"),
            ] {
                assert!(
                    document.contains(&format!("`{label}`")),
                    "undocumented cause label: {label}"
                );
            }
        }
    }

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
