//! Core config classification, before any read API (ADR-0041 §2, #2384).
//!
//! The reviewed data follows declarations, not just populated defaults. Imported
//! models are explicit unreadable boundaries for slice 2b. Classification grants
//! no read or write authority. Slice 3 must redact values, keys and diagnostics.
// The catalog is deliberately internal and not exposed until #2384 is complete.
#![allow(dead_code)]

use serde::Deserialize;
use serde_json::Value;

/// Static inventory data, rejected if its schema drifts.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Catalog {
    pub fields: Vec<Field>,
    pub representations: Vec<Representation>,
}

/// One declared field, including containers and currently absent shapes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Field {
    pub path: String,
    pub declaring_model: String,
    #[serde(rename = "type")]
    pub model_type: String,
    pub nullable: bool,
    pub file_aliases: Vec<String>,
    pub shape: Shape,
    pub default: DefaultRef,
    pub validation: Vec<Validation>,
    pub value_class: Sensitivity,
    pub key_class: Option<Sensitivity>,
    pub authority: Authority,
    pub reload: ReloadOwner,
    pub reload_issue: Option<u64>,
    pub boundary: Option<String>,
}

/// Serde wire shapes; nullable describes Option independently of shape.
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub(super) enum Shape {
    Scalar,
    Section,
    Array,
    SectionArray,
    Map,
    StringOrArray,
    StringUnion,
    Opaque,
}

/// Conservative classes, not permission to return the original bytes.
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub(super) enum Sensitivity {
    Public,
    PotentialSecret,
    RoutingMetadata,
    Aggregate,
    Opaque,
}

/// Current local config ownership; future transport writes need separate gates.
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub(super) enum Authority {
    LocalOperatorFile,
}

/// Actual application owner, rather than a promise that every field hot-reloads.
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub(super) enum ReloadOwner {
    AppLive,
    EventHubLive,
    AppAndClient,
    ClientLive,
    FuturePane,
    ProcessStartup,
    CommandStartup,
    ReadOnUse,
    Mixed,
    Opaque,
}

/// Validation owners supplement each stable declared type's serde contract.
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub(super) enum Validation {
    Serde,
    Keybindings,
    SidebarBounds,
    SidebarRowGap,
    SidebarPaneGap,
    PromptFloatLines,
    Color,
    ThemePalette,
    SoundFiles,
    IdleTiming,
    GossipCadence,
    PeerDefinitions,
    SpawnEnvironment,
    ReconnectBounds,
    StorageLimits,
    ToastDelay,
    Opaque,
}

/// Defaults refer only to compiled factories, never files or process env.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DefaultRef {
    pub factory: Factory,
    pub path: String,
    pub parent_absent: bool,
}

/// Entry factories describe defaults when an entry/optional parent is present.
#[derive(Debug, Deserialize)]
pub(super) enum Factory {
    Root,
    CustomTheme,
    KeyCommand,
    Peer,
    EmptyArray,
    Opaque,
}

/// Distinguish absent parent context and unclassified imported defaults.
#[derive(Debug, PartialEq)]
pub(super) enum CompiledDefault {
    Value { value: Value, parent_absent: bool },
    Opaque,
}

/// Core enum/newtype representations pin serde shapes, including payload types.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Representation {
    pub model: String,
    pub members: Vec<String>,
}

/// No source bytes or serde messages are carried by catalog failures.
#[derive(Debug)]
pub(super) enum InventoryError {
    InvalidCatalog,
    UnresolvedDefault,
}

/// Generic env support today; omitted defaults and collections are not admitted.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum EnvSupport {
    Generic(String),
    Blocklisted,
    CollectionOrSection,
    AbsentDefault,
    Opaque,
}

/// Base and adjacent overlay files are supported for every declared core field.
/// Deprecated aliases are recorded separately from the generic env layer.
#[derive(Debug)]
pub(super) struct Sources {
    pub base_file: bool,
    pub overlay_file: bool,
    pub env: EnvSupport,
    pub deprecated_alias: Option<&'static str>,
}

pub(super) fn core_catalog() -> Result<Catalog, InventoryError> {
    serde_json::from_str(include_str!("inventory/core.json"))
        .map_err(|_| InventoryError::InvalidCatalog)
}

impl Field {
    /// Resolve a compiled default without opening paths or expanding env names.
    pub fn compiled_default(&self) -> Result<CompiledDefault, InventoryError> {
        let tree = match self.default.factory {
            Factory::Opaque => return Ok(CompiledDefault::Opaque),
            Factory::EmptyArray => Value::Array(Vec::new()),
            Factory::Root => serialize(super::Config::default())?,
            Factory::CustomTheme => serialize(super::CustomThemeColors::default())?,
            Factory::KeyCommand => serialize(super::CommandKeybindConfig::default())?,
            Factory::Peer => serialize(super::PeerConfig::default())?,
        };
        let value = if self.default.path.is_empty() {
            Some(&tree)
        } else {
            self.default
                .path
                .split('.')
                .try_fold(&tree, |value, key| value.get(key))
        };
        // Serde may omit a declared Option::None. Coverage checks the factory
        // path against the declaration, so this is not an unknown-key fallback.
        let value = match value {
            Some(value) => value.clone(),
            None if self.nullable => Value::Null,
            None => return Err(InventoryError::UnresolvedDefault),
        };
        Ok(CompiledDefault::Value {
            value,
            parent_absent: self.default.parent_absent,
        })
    }

    /// Report loader support using its actual scalar walk and blocklist.
    /// No process environment values are read by this operation.
    pub fn sources(&self) -> Result<Sources, InventoryError> {
        let canonical = self.path.replace("[]", "");
        let env = if super::env::is_blocklisted(&canonical) {
            EnvSupport::Blocklisted
        } else if self.boundary.is_some() {
            EnvSupport::Opaque
        } else if self.shape != Shape::Scalar && self.shape != Shape::StringUnion {
            EnvSupport::CollectionOrSection
        } else {
            let schema = toml::Value::try_from(super::Config::default())
                .map_err(|_| InventoryError::InvalidCatalog)?;
            let table = schema.as_table().ok_or(InventoryError::InvalidCatalog)?;
            let mut name = None;
            super::env::walk_scalar_leaves(table, &mut Vec::new(), &mut |path, _| {
                if path.join(".") == self.path {
                    name = Some(super::env::env_var_name_for(path));
                }
            });
            name.map_or(EnvSupport::AbsentDefault, EnvSupport::Generic)
        };
        let deprecated_alias = match self.path.as_str() {
            "name" => Some("FLOCK_HOST_NAME"),
            "ui.sound.enabled" => Some("FLOCK_DISABLE_SOUND"),
            _ => None,
        };
        Ok(Sources {
            base_file: true,
            overlay_file: true,
            env,
            deprecated_alias,
        })
    }
}

fn serialize(value: impl serde::Serialize) -> Result<Value, InventoryError> {
    serde_json::to_value(value).map_err(|_| InventoryError::InvalidCatalog)
}

#[cfg(test)]
mod tests;
