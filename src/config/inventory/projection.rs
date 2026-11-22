//! Internal, caller-independent redaction (ADR-0041 §4, #2384 slice 3a).
//!
//! Output constructors are private. Inputs are inert values: this module never
//! opens references, executes commands, expands env, or logs raw input.
//! Existing diagnostic producers/sinks remain assigned to approved slice 3b.

use super::*;
use std::collections::BTreeMap;

/// Serializable safe output, with no raw-view selector or raw-value accessor.
#[derive(Debug, Serialize, PartialEq)]
pub(super) struct Projection(Node);

#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
enum Node {
    Public(Value),
    Object {
        fields: BTreeMap<String, Node>,
        unclassified: bool,
    },
    Array(Vec<Node>),
    Redacted(Reason),
    Required,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Reason {
    Sensitive(ContentKind),
    Unclassified,
    Structural,
    Invalid,
}

/// Candidate and running snapshots use exactly the same mandatory projection.
#[derive(Debug, Serialize, PartialEq)]
pub(super) struct Snapshots {
    candidate: Projection,
    running: Projection,
}

pub(super) fn snapshots(catalog: &Catalog, candidate: &Value, running: &Value) -> Snapshots {
    Snapshots {
        candidate: document(catalog, candidate),
        running: document(catalog, running),
    }
}

pub(super) fn document(catalog: &Catalog, input: &Value) -> Projection {
    Projection(object(catalog, "", input))
}

fn object(catalog: &Catalog, path: &str, input: &Value) -> Node {
    let Some(input) = input.as_object() else {
        return Node::Redacted(Reason::Invalid);
    };
    let prefix = if path.is_empty() {
        String::new()
    } else {
        format!("{path}.")
    };
    let mut fields = BTreeMap::new();
    let mut unclassified = false;
    for (key, value) in input {
        let full = format!("{prefix}{key}");
        let field = catalog.fields.iter().find(|f| {
            f.path
                .strip_prefix(&prefix)
                .is_some_and(|name| !name.chars().any(|ch| matches!(ch, '.' | '[' | '<')))
                && (f.path == full || f.file_aliases.contains(&full))
        });
        let Some(field) = field else {
            unclassified = true;
            continue;
        };
        let Some(name) = field.path.strip_prefix(&prefix) else {
            unclassified = true;
            continue;
        };
        let node = project(catalog, field, value);
        if fields.insert(name.to_string(), node).is_some() {
            fields.insert(name.to_string(), Node::Redacted(Reason::Invalid));
        }
    }
    Node::Object {
        fields,
        unclassified,
    }
}

fn project(catalog: &Catalog, field: &Field, value: &Value) -> Node {
    if field.boundary.is_some() || field.value_class == Sensitivity::Opaque {
        return Node::Redacted(Reason::Unclassified);
    }
    match field.value_class {
        Sensitivity::PotentialSecret | Sensitivity::RoutingMetadata => {
            Node::Redacted(Reason::Sensitive(field.content_kind))
        }
        Sensitivity::Aggregate => match field.shape {
            Shape::Section if field.nullable && value.is_null() => Node::Public(Value::Null),
            Shape::Section => object(catalog, &field.path, value),
            Shape::SectionArray => {
                value
                    .as_array()
                    .map_or(Node::Redacted(Reason::Invalid), |items| {
                        Node::Array(
                            items
                                .iter()
                                .map(|item| object(catalog, &format!("{}[]", field.path), item))
                                .collect(),
                        )
                    })
            }
            _ => Node::Redacted(Reason::Unclassified),
        },
        Sensitivity::Public => {
            if field.shape == Shape::Array {
                let Some(ty) = collection_value_type(&field.model_type) else {
                    return Node::Redacted(Reason::Unclassified);
                };
                value
                    .as_array()
                    .map_or(Node::Redacted(Reason::Invalid), |items| {
                        Node::Array(
                            items
                                .iter()
                                .map(|v| public(catalog, &ty, false, v))
                                .collect(),
                        )
                    })
            } else if field.shape == Shape::Scalar {
                public(catalog, &field.model_type, field.nullable, value)
            } else {
                Node::Redacted(Reason::Unclassified)
            }
        }
        Sensitivity::Opaque => Node::Redacted(Reason::Unclassified),
    }
}

fn public(catalog: &Catalog, ty: &str, nullable: bool, value: &Value) -> Node {
    if nullable && value.is_null() {
        return Node::Public(Value::Null);
    }
    let ty = ty
        .strip_prefix("Option<")
        .and_then(|t| t.strip_suffix('>'))
        .unwrap_or(ty);
    let ty = ty.rsplit("::").next().unwrap_or(ty);
    let accepted = match ty {
        "bool" => value.is_boolean(),
        "u8" => value.as_u64().is_some_and(|v| u8::try_from(v).is_ok()),
        "u16" => value.as_u64().is_some_and(|v| u16::try_from(v).is_ok()),
        "u32" => value.as_u64().is_some_and(|v| u32::try_from(v).is_ok()),
        "u64" => value.as_u64().is_some(),
        "usize" => value.as_u64().is_some_and(|v| usize::try_from(v).is_ok()),
        "NonZeroU64" => value.as_u64().is_some_and(|v| v != 0),
        "NonZeroUsize" => value
            .as_u64()
            .is_some_and(|v| v != 0 && usize::try_from(v).is_ok()),
        "f32" => value
            .as_f64()
            .is_some_and(|v| v.is_finite() && (v as f32).is_finite()),
        "f64" => value.as_f64().is_some_and(f64::is_finite),
        _ => value.as_str().is_some_and(|text| {
            catalog.representations.iter().any(|r| {
                r.model == ty
                    && !r.members.is_empty()
                    && r.members.iter().all(|m| !m.contains(':'))
                    && r.members.iter().any(|m| m == text)
            })
        }),
    };
    if accepted {
        Node::Public(value.clone())
    } else {
        Node::Redacted(Reason::Invalid)
    }
}

/// Every declared default context is represented, including required entries.
#[derive(Debug, Serialize, PartialEq)]
pub(super) struct Defaults(BTreeMap<String, DefaultEntry>);

#[derive(Debug, Serialize, PartialEq)]
struct DefaultEntry {
    parent_absent: bool,
    value: Node,
}

pub(super) fn defaults(catalog: &Catalog) -> Result<Defaults, InventoryError> {
    let mut fields = BTreeMap::new();
    for field in &catalog.fields {
        let value = if field.boundary.is_some() || field.value_class == Sensitivity::Opaque {
            Node::Redacted(Reason::Unclassified)
        } else {
            match field.compiled_default()? {
                CompiledDefault::Value { value, .. } => project(catalog, field, &value),
                CompiledDefault::Required => Node::Required,
                CompiledDefault::Opaque => Node::Redacted(Reason::Unclassified),
            }
        };
        fields.insert(
            field.path.clone(),
            DefaultEntry {
                parent_absent: field.default.parent_absent,
                value,
            },
        );
    }
    Ok(Defaults(fields))
}

/// Closed source categories carry no paths, env values or executable names.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Layer {
    CompiledDefault,
    Environment,
    BaseFile,
    OverlayFile,
    CommandLine,
}

/// Untrusted source identifier. Deliberately not Debug or Serialize.
/// Only a recognized env name for a public field can be represented, rebuilt
/// from catalog metadata rather than copied from this input.
pub(super) struct Source<'a> {
    pub layer: Layer,
    pub identifier: &'a str,
}

#[derive(Debug, Serialize, PartialEq)]
pub(super) struct Provenance {
    field: Option<String>,
    origin: Origin,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Origin {
    Layers {
        winner: SafeSource,
        shadowed: Vec<SafeSource>,
    },
    Redacted(Reason),
}

/// Keep layer categories only for classified public fields. Sensitive values
/// hide their provenance; unknown paths are not echoed. Source identifiers are
/// hidden unless they exactly match a catalog-derived public env name. File
/// paths are never guessed safe from their spelling.
pub(super) fn provenance(
    catalog: &Catalog,
    path: &str,
    winner: &Source<'_>,
    shadowed: &[Source<'_>],
) -> Provenance {
    let field = catalog
        .fields
        .iter()
        .find(|f| f.path == path || f.file_aliases.iter().any(|a| a == path));
    let origin = match field {
        Some(f) if f.boundary.is_none() && f.value_class == Sensitivity::Public => Origin::Layers {
            winner: safe_source(f, winner),
            shadowed: shadowed.iter().map(|s| safe_source(f, s)).collect(),
        },
        Some(f) if f.boundary.is_none() && f.value_class == Sensitivity::Aggregate => {
            Origin::Redacted(Reason::Structural)
        }
        Some(f) if f.boundary.is_none() && f.value_class != Sensitivity::Opaque => {
            Origin::Redacted(Reason::Sensitive(f.content_kind))
        }
        _ => Origin::Redacted(Reason::Unclassified),
    };
    Provenance {
        field: field.map(|f| f.path.clone()),
        origin,
    }
}

#[derive(Debug, Serialize, PartialEq)]
struct SafeSource {
    layer: Layer,
    identifier: Identifier,
}
#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Identifier {
    Hidden,
    Environment(String),
}
fn safe_source(field: &Field, source: &Source<'_>) -> SafeSource {
    let mut identifier = Identifier::Hidden;
    if source.layer == Layer::Environment {
        if let Ok(Sources {
            env: EnvSupport::Generic(name),
            ..
        }) = field.sources()
        {
            if name == source.identifier {
                identifier = Identifier::Environment(name);
            }
        }
    }
    SafeSource {
        layer: source.layer,
        identifier,
    }
}
