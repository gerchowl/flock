use super::*;
use std::collections::{BTreeMap, BTreeSet};

fn declarations() -> BTreeMap<String, syn::Item> {
    [
        include_str!("../model.rs"),
        include_str!("../theme.rs"),
        include_str!("../sound.rs"),
        include_str!("../spawn_env.rs"),
        include_str!("../keybinds.rs"),
    ]
    .into_iter()
    .flat_map(|source| syn::parse_file(source).unwrap().items)
    .filter_map(|item| match &item {
        syn::Item::Struct(s) => Some((s.ident.to_string(), item)),
        syn::Item::Enum(e) => Some((e.ident.to_string(), item)),
        _ => None,
    })
    .collect()
}

fn type_name(ty: &syn::Type) -> String {
    let syn::Type::Path(path) = ty else {
        panic!("unclassified Rust type")
    };
    let segment = path.path.segments.last().unwrap();
    let mut name = segment.ident.to_string();
    if let syn::PathArguments::AngleBracketed(args) = &segment.arguments {
        let args: Vec<_> = args
            .args
            .iter()
            .map(|arg| {
                let syn::GenericArgument::Type(ty) = arg else {
                    panic!("unclassified generic argument")
                };
                type_name(ty)
            })
            .collect();
        name.push_str(&format!("<{}>", args.join(", ")));
    }
    name
}

fn serde_name(attrs: &[syn::Attribute], fallback: String) -> String {
    for attr in attrs.iter().filter(|a| a.path().is_ident("serde")) {
        let items = attr
            .parse_args_with(
                syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
            )
            .unwrap();
        for item in items {
            if let syn::Meta::NameValue(value) = item {
                if value.path.is_ident("rename") {
                    if let syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(s),
                        ..
                    }) = value.value
                    {
                        return s.value();
                    }
                }
            }
        }
    }
    fallback
}

fn members(item: &syn::Item) -> Vec<String> {
    match item {
        syn::Item::Enum(e) => e
            .variants
            .iter()
            .map(|v| {
                let fields: Vec<_> = v.fields.iter().map(|f| type_name(&f.ty)).collect();
                if fields.is_empty() {
                    v.ident.to_string()
                } else {
                    format!("{}:{}", v.ident, fields.join(", "))
                }
            })
            .collect(),
        syn::Item::Struct(s) => s
            .fields
            .iter()
            .enumerate()
            .map(|(i, f)| {
                format!(
                    "{}:{}",
                    f.ident
                        .as_ref()
                        .map_or_else(|| i.to_string(), ToString::to_string),
                    type_name(&f.ty)
                )
            })
            .collect(),
        _ => panic!("not a config declaration"),
    }
}

/// The approved 2b boundaries are exact, not a general unknown-type escape.
const BOUNDARIES: &[(&str, &str)] = &[
    ("structured", "src/structured/launch.rs::StructuredConfig"),
    ("checks", "src/checks/config.rs::ChecksConfig"),
    ("fleet", "src/spawn/mod.rs::FleetConfig"),
    ("channels", "src/channels.rs::ChannelBinding"),
    ("session.restart", "src/agent_restart.rs::RestartPolicy"),
    (
        "session.restart_agents",
        "src/agent_restart.rs::RestartPolicy",
    ),
];

fn declared_shape(
    ty: &str,
    boundary: bool,
    catalog: &Catalog,
    graph: &BTreeMap<String, syn::Item>,
) -> Shape {
    let ty = ty
        .strip_prefix("Option<")
        .and_then(|ty| ty.strip_suffix('>'))
        .unwrap_or(ty);
    if ty.starts_with("BTreeMap<") || ty.starts_with("HashMap<") || ty == "SpawnEnvConfig" {
        return Shape::Map;
    }
    if let Some(inner) = ty.strip_prefix("Vec<").and_then(|ty| ty.strip_suffix('>')) {
        return if boundary || matches!(graph.get(inner), Some(syn::Item::Struct(_))) {
            Shape::SectionArray
        } else {
            Shape::Array
        };
    }
    if boundary {
        return Shape::Opaque;
    }
    match ty {
        "BindingConfig" => Shape::StringOrArray,
        "NewTerminalCwdConfig" => Shape::StringUnion,
        _ if catalog.representations.iter().any(|r| r.model == ty) => Shape::Scalar,
        _ if matches!(graph.get(ty), Some(syn::Item::Struct(_))) => Shape::Section,
        _ => Shape::Scalar,
    }
}

fn core_coverage(
    catalog: &Catalog,
    graph: &BTreeMap<String, syn::Item>,
) -> Result<(), Vec<String>> {
    fn walk(
        model: &str,
        prefix: &str,
        catalog: &Catalog,
        graph: &BTreeMap<String, syn::Item>,
        found: &mut BTreeSet<String>,
        reps: &mut BTreeSet<String>,
        errors: &mut Vec<String>,
    ) {
        let item = graph
            .get(model)
            .unwrap_or_else(|| panic!("unclassified model: {model}"));
        if let Some(rep) = catalog.representations.iter().find(|r| r.model == model) {
            assert_eq!(members(item), rep.members, "representation drift: {model}");
            reps.insert(model.to_string());
            return;
        }
        let syn::Item::Struct(s) = item else {
            panic!("unclassified enum: {model}")
        };
        for f in &s.fields {
            let key = serde_name(&f.attrs, f.ident.as_ref().unwrap().to_string());
            let path = if prefix.is_empty() {
                key
            } else {
                format!("{prefix}.{key}")
            };
            let Some(spec) = catalog.fields.iter().find(|spec| spec.path == path) else {
                errors.push(format!("unclassified core field: {path}"));
                continue;
            };
            assert!(
                found.insert(path.clone()),
                "duplicate declared path: {path}"
            );
            let ty = type_name(&f.ty);
            assert_eq!(spec.model_type, ty, "type drift: {path}");
            assert_eq!(spec.declaring_model, model);
            assert_eq!(
                spec.nullable,
                ty.starts_with("Option<"),
                "optionality drift: {path}"
            );
            let expected_boundary = BOUNDARIES
                .iter()
                .find(|(p, _)| *p == path)
                .map(|(_, source)| *source);
            assert_eq!(
                spec.boundary.as_deref(),
                expected_boundary,
                "boundary drift: {path}"
            );
            assert_eq!(
                spec.shape,
                declared_shape(&ty, expected_boundary.is_some(), catalog, graph),
                "shape drift: {path}"
            );
            assert_eq!(
                spec.key_class.is_some(),
                spec.shape == Shape::Map,
                "key policy: {path}"
            );
            if expected_boundary.is_none()
                && matches!(spec.shape, Shape::Section | Shape::SectionArray)
            {
                assert_eq!(spec.value_class, Sensitivity::Aggregate);
            }
            if expected_boundary.is_some() {
                assert_eq!(spec.value_class, Sensitivity::Opaque);
                assert_eq!(spec.reload, ReloadOwner::Opaque);
                assert!(matches!(
                    spec.compiled_default().unwrap(),
                    CompiledDefault::Opaque
                ));
                continue;
            }
            let mut inner = &f.ty;
            let mut child_path = path;
            loop {
                let syn::Type::Path(t) = inner else {
                    panic!("unclassified shape")
                };
                let last = t.path.segments.last().unwrap();
                if !matches!(
                    last.ident.to_string().as_str(),
                    "Option" | "Vec" | "HashMap" | "BTreeMap"
                ) {
                    break;
                }
                if last.ident == "Vec" {
                    child_path.push_str("[]");
                }
                if last.ident == "HashMap" || last.ident == "BTreeMap" {
                    child_path.push_str("<key>");
                }
                let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
                    panic!("generic shape")
                };
                inner = args
                    .args
                    .iter()
                    .rev()
                    .find_map(|arg| {
                        if let syn::GenericArgument::Type(ty) = arg {
                            Some(ty)
                        } else {
                            None
                        }
                    })
                    .unwrap();
            }
            let inner = type_name(inner);
            if graph.contains_key(&inner) {
                walk(&inner, &child_path, catalog, graph, found, reps, errors);
            } else {
                assert!(
                    [
                        "String",
                        "bool",
                        "u16",
                        "u64",
                        "u32",
                        "usize",
                        "f64",
                        "PathBuf",
                        "NonZeroUsize",
                        "NonZeroU64",
                        "SocketAddr"
                    ]
                    .contains(&inner.as_str()),
                    "unclassified imported type: {inner}"
                );
            }
        }
    }
    let mut found = BTreeSet::new();
    let mut reps = BTreeSet::new();
    let mut errors = Vec::new();
    walk(
        "Config",
        "",
        catalog,
        graph,
        &mut found,
        &mut reps,
        &mut errors,
    );
    let declared: BTreeSet<_> = catalog.fields.iter().map(|f| f.path.clone()).collect();
    assert_eq!(
        declared.len(),
        catalog.fields.len(),
        "duplicate inventory record"
    );
    for removed in declared.difference(&found) {
        errors.push(format!("orphan inventory record: {removed}"));
    }
    assert_eq!(
        reps,
        catalog
            .representations
            .iter()
            .map(|r| r.model.clone())
            .collect()
    );
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[test]
fn core_declarations_are_classified() {
    let catalog = core_catalog().unwrap();
    let graph = declarations();
    assert_eq!(core_coverage(&catalog, &graph), Ok(()));
}

#[test]
fn an_unclassified_core_field_is_rejected_even_when_default_omits_it() {
    let catalog = core_catalog().unwrap();
    let mut graph = declarations();
    let syn::Item::Struct(ui) = graph.get_mut("UiConfig").unwrap() else {
        unreachable!()
    };
    let syn::Fields::Named(fields) = &mut ui.fields else {
        unreachable!()
    };
    fields.named.push(
        syn::Field::parse_named
            .parse_str("pub private_probe: Option<String>")
            .unwrap(),
    );
    assert_eq!(
        core_coverage(&catalog, &graph),
        Err(vec!["unclassified core field: ui.private_probe".into()])
    );
}

use syn::parse::Parser;

#[test]
fn core_defaults_include_optional_parents_and_empty_entry_models() {
    let catalog = core_catalog().unwrap();
    for field in &catalog.fields {
        let relative = match field.default.factory {
            Factory::Root => Some(field.path.as_str()),
            Factory::Peer => field.path.strip_prefix("peers[]."),
            Factory::KeyCommand => field.path.strip_prefix("keys.command[]."),
            Factory::CustomTheme => field.path.strip_prefix("theme.custom."),
            Factory::EmptyArray | Factory::Opaque => Some(""),
        };
        assert_eq!(
            relative,
            Some(field.default.path.as_str()),
            "factory path: {}",
            field.path
        );
        assert_eq!(
            field.default.parent_absent,
            matches!(
                field.default.factory,
                Factory::Peer | Factory::KeyCommand | Factory::CustomTheme
            )
        );
        field
            .compiled_default()
            .unwrap_or_else(|_| panic!("unresolved compiled default: {}", field.path));
        assert!(!field.validation.is_empty());
        assert!(field.validation.contains(&Validation::Serde));
        assert_eq!(field.authority, Authority::LocalOperatorFile);
    }
    let get = |path: &str| catalog.fields.iter().find(|f| f.path == path).unwrap();
    assert_eq!(
        get("theme.custom").compiled_default().unwrap(),
        CompiledDefault::Value {
            value: Value::Null,
            parent_absent: false
        }
    );
    assert_eq!(
        get("theme.custom.accent").compiled_default().unwrap(),
        CompiledDefault::Value {
            value: Value::Null,
            parent_absent: true
        }
    );
    assert_eq!(
        get("keys.command").compiled_default().unwrap(),
        CompiledDefault::Value {
            value: serde_json::json!([]),
            parent_absent: false
        }
    );
    assert_eq!(
        get("keys.command[].command").compiled_default().unwrap(),
        CompiledDefault::Value {
            value: serde_json::json!(""),
            parent_absent: true
        }
    );
    assert_eq!(
        get("peers[].poll_interval_secs")
            .compiled_default()
            .unwrap(),
        CompiledDefault::Value {
            value: Value::Null,
            parent_absent: true
        }
    );
}

#[test]
fn sources_match_the_loader_without_reading_env_values() {
    let _guard = super::super::test_config_env_guard();
    let catalog = core_catalog().unwrap();
    let get = |path: &str| {
        catalog
            .fields
            .iter()
            .find(|f| f.path == path)
            .unwrap()
            .sources()
            .unwrap()
    };
    assert_eq!(
        get("ui.tab_mode").env,
        EnvSupport::Generic("FLOCK_UI_TAB_MODE".into())
    );
    assert_eq!(get("icon").env, EnvSupport::AbsentDefault);
    assert_eq!(get("theme.custom.accent").env, EnvSupport::Blocklisted);
    assert_eq!(get("keys.command[].command").env, EnvSupport::Blocklisted);
    assert_eq!(
        get("experimental.cjk_ime_agents").env,
        EnvSupport::CollectionOrSection
    );
    assert_eq!(get("checks").env, EnvSupport::Opaque);
    assert_eq!(get("name").deprecated_alias, Some("FLOCK_HOST_NAME"));
    assert_eq!(
        get("ui.sound.enabled").deprecated_alias,
        Some("FLOCK_DISABLE_SOUND")
    );
    for field in &catalog.fields {
        let sources = field.sources().unwrap();
        assert!(sources.base_file && sources.overlay_file);
    }
}

#[test]
fn user_chosen_keys_and_free_form_values_are_never_public() {
    let catalog = core_catalog().unwrap();
    for path in ["ui.agent_aliases", "session.restart_agents", "spawn.env"] {
        let field = catalog.fields.iter().find(|f| f.path == path).unwrap();
        assert_eq!(field.key_class, Some(Sensitivity::PotentialSecret));
        assert_ne!(field.value_class, Sensitivity::Public);
    }
    for rep in &catalog.representations {
        if rep
            .members
            .iter()
            .any(|member| member.contains("String") || member.contains("PathBuf"))
        {
            for field in catalog.fields.iter().filter(|f| f.model_type == rep.model) {
                assert_ne!(field.value_class, Sensitivity::Public, "{}", field.path);
            }
        }
    }
    for field in &catalog.fields {
        if field.shape == Shape::Map {
            assert_eq!(field.key_class, Some(Sensitivity::PotentialSecret));
        }
        if field.model_type.contains("String")
            || field.model_type.contains("PathBuf")
            || field.model_type.contains("BindingConfig")
            || field.model_type.contains("NewTerminalCwdConfig")
        {
            assert_ne!(field.value_class, Sensitivity::Public, "{}", field.path);
        }
    }
}
