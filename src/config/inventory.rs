//! Complete config classification, before any read API (ADR-0041 §2, #2384).
//!
//! The reviewed data follows core and imported declarations, including optional
//! and collection-entry shapes, not just populated defaults. Classification grants
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
    ChecksPolicy,
    CronExpression,
    ChannelBindings,
    StructuredLaunch,
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
    EmptyMap,
    Script,
    Restart,
    CronTz,
    Action,
    Sound,
    DigestPath,
    /// No independent default: the entry or selected variant requires this field.
    Required,
    Opaque,
}

/// Distinguish absent parent context, compiled values and required entry fields.
#[derive(Debug, PartialEq)]
pub(super) enum CompiledDefault {
    Value { value: Value, parent_absent: bool },
    Opaque,
    Required,
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
            Factory::Required => return Ok(CompiledDefault::Required),
            Factory::EmptyArray => Value::Array(Vec::new()),
            Factory::EmptyMap => serde_json::json!({}),
            Factory::Script => serialize(crate::checks::config::ScriptCheck::default())?,
            Factory::Restart => serialize(crate::agent_restart::RestartPolicy::default())?,
            Factory::CronTz => serialize(crate::checks::cron::CronTz::default())?,
            Factory::Action => serialize(crate::checks::ActionSpec::default())?,
            Factory::Sound => serialize(crate::api::schema::NotificationShowSound::default())?,
            Factory::DigestPath => {
                serialize(crate::checks::config::default_digest_path_template())?
            }
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

/// Stable declared type at the failing field, without compiler type strings.
/// Unknown keys retain the nearest declared container type. User map keys and
/// array indices select a schema shape, never become catalog identifiers.
pub(super) fn diagnostic_type(section: &str, path: Option<&serde_path_to_error::Path>) -> String {
    let Ok(catalog) = core_catalog() else {
        return "unclassified config field".into();
    };
    let mut canonical = section.to_string();
    let mut current = catalog.fields.iter().find(|f| f.path == canonical);
    let mut expected = current.map(|f| f.model_type.clone());
    if let Some(path) = path {
        for segment in path.iter() {
            let next = match segment {
                serde_path_to_error::Segment::Map { key } => {
                    if current.is_some_and(|f| f.path == canonical && f.shape == Shape::Map) {
                        expected = current.and_then(|f| map_value_type(f, &catalog));
                        format!("{canonical}<key>")
                    } else {
                        format!("{canonical}.{key}")
                    }
                }
                serde_path_to_error::Segment::Seq { .. } => {
                    expected = expected.as_deref().and_then(collection_value_type);
                    format!("{canonical}[]")
                }
                serde_path_to_error::Segment::Enum { variant } => format!("{canonical}.{variant}"),
                _ => break,
            };
            // Collection/variant shapes do not necessarily have a separate
            // record; descendants are still resolved from their canonical path.
            canonical = next;
            if let Some(field) = catalog
                .fields
                .iter()
                .find(|f| f.path == canonical || f.file_aliases.contains(&canonical))
            {
                canonical = field.path.clone();
                expected = Some(field.model_type.clone());
                current = Some(field);
            }
        }
    }
    expected.unwrap_or_else(|| "unclassified config field".into())
}

fn map_value_type(field: &Field, catalog: &Catalog) -> Option<String> {
    // Transparent map wrappers pin their storage type in the representation.
    // The storage field is not an additional accepted file key.
    let declared = catalog
        .representations
        .iter()
        .find(|r| r.model == field.model_type && r.members.len() == 1)
        .and_then(|r| r.members.first())
        .and_then(|member| member.split_once(':'))
        .map_or(field.model_type.as_str(), |(_, ty)| ty);
    collection_value_type(declared)
}

fn collection_value_type(ty: &str) -> Option<String> {
    let start = ty.find('<')? + 1;
    let body = ty.get(start..ty.len().checked_sub(1)?)?;
    let mut depth = 0;
    let mut last = 0;
    for (index, ch) in body.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => last = index + 1,
            _ => {}
        }
    }
    Some(body[last..].trim().to_string())
}

#[cfg(test)]
mod tests;
