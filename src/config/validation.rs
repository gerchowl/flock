//! Closed producer causes for ADR-0041 §4. No value or input-derived hint is
//! accepted. Schema projection happens before returned strings or immediate logs.
use super::safe_diagnostic::{Diagnostic, Outcome};

#[derive(Debug, Clone, Copy)]
pub(crate) enum Cause {
    InvalidBinding,
    Range,
    Indexed,
    InvalidModifier,
    NavigatePrefix,
    NavigateEsc,
    Reserved,
    Conflict,
    DirectPrintable,
    EmptyCommand,
    SoundFormat,
    SoundMissing,
    SoundInvalid,
    UnknownAgent,
    AgentAlias,
    EmptyEnv,
    EqualsEnv,
    WildcardEnv,
    WhitespaceEnv,
    MissingName,
    DuplicateName,
    Interval { minimum: u64 },
    Timeout,
    Debounce,
    Program,
    ChannelName,
    ChannelOwner,
    ChannelDuplicate,
    ChannelTarget,
}
impl Cause {
    pub(super) fn description(self) -> (&'static str, Outcome) {
        use Outcome::*;
        match self {
            Self::InvalidBinding => ("invalid keybinding", DisableBinding),
            Self::Range => (
                "range keybinding is only valid for indexed actions",
                DisableBinding,
            ),
            Self::InvalidModifier => ("invalid indexed keybinding: expected modifier combination", DisableBinding),
            Self::Indexed => ("indexed keybinding must use 1..9", DisableBinding),
            Self::NavigatePrefix => (
                "navigate keybinding must not include prefix",
                DisableBinding,
            ),
            Self::NavigateEsc => ("navigate keybinding cannot use esc", DisableBinding),
            Self::Reserved => (
                "reserved keybinding: uses keys.prefix; pressing the prefix twice sends a literal prefix key",
                DisableBinding,
            ),
            Self::Conflict => ("conflicting keybinding: first binding kept", DisableBinding),
            Self::DirectPrintable => (
                "unsafe direct keybinding: would intercept typing; require the prefix",
                DisableBinding,
            ),
            Self::EmptyCommand => ("empty custom command", DisableCommand),
            Self::SoundFormat => (
                "unsupported sound file format; expected an mp3 file",
                DefaultSound,
            ),
            Self::SoundMissing => ("missing sound file", DefaultSound),
            Self::SoundInvalid => ("invalid sound file: expected a regular file", DefaultSound),
            Self::UnknownAgent => ("unknown agent: not an agent flock detects", DropEntry),
            Self::AgentAlias => ("duplicate agent alias: first spelling kept", DropEntry),
            Self::EmptyEnv => ("environment key is empty", DropEntry),
            Self::EqualsEnv => (
                "environment key contains '=', which is not part of a variable name",
                DropEntry,
            ),
            Self::WildcardEnv => (
                "environment key looks like a wildcard; only exact names are matched",
                DropEntry,
            ),
            Self::WhitespaceEnv => (
                "environment key contains whitespace, which no shell can export",
                DropEntry,
            ),
            Self::MissingName => ("entry is missing name", DropEntry),
            Self::DuplicateName => ("duplicate name: later entry ignored", DropEntry),
            Self::Interval { .. } => (
                "interval_secs is less than checks.min_tick_secs; clamped",
                Clamp,
            ),
            Self::Timeout => ("timeout_secs is less than 1; using 1s", Clamp),
            Self::Debounce => ("debounce is 0; using 1 so a single fire still emits", Clamp),
            Self::Program => (
                "program is not absolute and not under ~/.flock/checks/",
                DropEntry,
            ),
            Self::ChannelName => (
                "invalid channel name: needs 1-64 characters from [A-Za-z0-9_-]",
                DropEntry,
            ),
            Self::ChannelOwner => ("invalid channel entry: needs an owner_node", DropEntry),
            Self::ChannelDuplicate => ("invalid channel entry: needs a unique name", DropEntry),
            Self::ChannelTarget => (
                "invalid channel target: expected fleet, repo:<name> or agent:<id>",
                Ignore,
            ),
        }
    }
}
/// Numeric indices refer to the original declaration, before filtering entries.
pub(crate) fn message(cause: Cause, key: &str, index: Option<usize>) -> String {
    Diagnostic::validation(cause, key, index).to_string()
}
