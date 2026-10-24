use super::*;
use std::collections::{BTreeMap, BTreeSet};

fn declarations() -> BTreeMap<String, syn::Item> {
    [
        include_str!("../model.rs"),
        include_str!("../theme.rs"),
        include_str!("../sound.rs"),
        include_str!("../spawn_env.rs"),
        include_str!("../keybinds.rs"),
        include_str!("../../structured/launch.rs"),
        include_str!("../../checks/config.rs"),
        include_str!("../../checks/cron.rs"),
        include_str!("../../spawn/mod.rs"),
        include_str!("../../channels.rs"),
        include_str!("../../agent_restart.rs"),
        include_str!("../../api/schema.rs"),
    ]
    .into_iter()
    .flat_map(|source| syn::parse_file(source).unwrap().items)
    .filter_map(|item| match &item {
        syn::Item::Struct(s) => Some((s.ident.to_string(), item)),
        syn::Item::Enum(e) => Some((e.ident.to_string(), item)),
        _ => None,
    })
    .fold(BTreeMap::new(), |mut graph, (name, item)| {
        assert!(
            graph.insert(name.clone(), item).is_none(),
            "ambiguous declared model: {name}"
        );
        graph
    })
}

/// Comparisons retain every path segment; only graph lookup uses model names.
fn type_name(ty: &syn::Type) -> String {
    format_type(ty, true)
}

fn model_type(ty: &syn::Type) -> String {
    format_type(ty, false)
}

fn format_type(ty: &syn::Type, qualified: bool) -> String {
    let syn::Type::Path(path) = ty else {
        panic!("unclassified Rust type")
    };
    assert!(path.qself.is_none(), "unclassified qualified-self type");
    let start = if qualified {
        0
    } else {
        path.path.segments.len() - 1
    };
    let names = path
        .path
        .segments
        .iter()
        .skip(start)
        .map(|segment| {
            let mut name = segment.ident.to_string();
            match &segment.arguments {
                syn::PathArguments::AngleBracketed(args) => {
                    let args = args
                        .args
                        .iter()
                        .map(|arg| {
                            let syn::GenericArgument::Type(ty) = arg else {
                                panic!("unclassified generic argument")
                            };
                            format_type(ty, qualified)
                        })
                        .collect::<Vec<_>>();
                    name.push_str(&format!("<{}>", args.join(", ")));
                }
                syn::PathArguments::None => {}
                _ => panic!("unclassified path arguments"),
            }
            name
        })
        .collect::<Vec<_>>()
        .join("::");
    if qualified && path.path.leading_colon.is_some() {
        format!("::{names}")
    } else {
        names
    }
}

fn serde_strings(attrs: &[syn::Attribute], key: &str) -> Vec<String> {
    let mut strings = Vec::new();
    for attr in attrs.iter().filter(|a| a.path().is_ident("serde")) {
        let items = attr
            .parse_args_with(
                syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
            )
            .unwrap();
        for item in items {
            assert!(
                !item.path().is_ident("flatten"),
                "unsupported serde(flatten): inventory needs explicit review"
            );
            if let syn::Meta::NameValue(value) = item {
                if value.path.is_ident(key) {
                    let syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(s),
                        ..
                    }) = value.value
                    else {
                        panic!("unsupported serde naming")
                    };
                    strings.push(s.value());
                }
            }
        }
    }
    strings
}

fn serde_name(attrs: &[syn::Attribute], fallback: String) -> String {
    serde_strings(attrs, "rename")
        .into_iter()
        .next()
        .unwrap_or(fallback)
}

fn variant_name(e: &syn::ItemEnum, v: &syn::Variant) -> String {
    let name = v.ident.to_string();
    let fallback = match serde_strings(&e.attrs, "rename_all")
        .first()
        .map(String::as_str)
    {
        None => name,
        Some("lowercase") => name.to_ascii_lowercase(),
        Some(rule @ ("snake_case" | "kebab-case")) => {
            let separator = if rule == "snake_case" { '_' } else { '-' };
            let mut renamed = String::new();
            for (i, ch) in name.chars().enumerate() {
                if i > 0 && ch.is_ascii_uppercase() {
                    renamed.push(separator);
                }
                renamed.push(ch.to_ascii_lowercase());
            }
            renamed
        }
        Some(rule) => panic!("unsupported serde rename_all: {rule}"),
    };
    serde_name(&v.attrs, fallback)
}

fn members(item: &syn::Item) -> Vec<String> {
    match item {
        syn::Item::Enum(e) => e
            .variants
            .iter()
            .map(|v| {
                let fields: Vec<_> = v.fields.iter().map(|f| type_name(&f.ty)).collect();
                let name = variant_name(e, v);
                if fields.is_empty() {
                    name
                } else {
                    format!("{}:{}", name, fields.join(", "))
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
                    serde_name(
                        &f.attrs,
                        f.ident
                            .as_ref()
                            .map_or_else(|| i.to_string(), ToString::to_string)
                    ),
                    type_name(&f.ty)
                )
            })
            .collect(),
        _ => panic!("not a config declaration"),
    }
}

fn declared_shape(ty: &str, catalog: &Catalog, graph: &BTreeMap<String, syn::Item>) -> Shape {
    let ty = ty
        .strip_prefix("Option<")
        .and_then(|ty| ty.strip_suffix('>'))
        .unwrap_or(ty);
    if ty.starts_with("BTreeMap<") || ty.starts_with("HashMap<") || ty == "SpawnEnvConfig" {
        return Shape::Map;
    }
    if let Some(inner) = ty.strip_prefix("Vec<").and_then(|ty| ty.strip_suffix('>')) {
        return if matches!(graph.get(inner), Some(syn::Item::Struct(_))) {
            Shape::SectionArray
        } else {
            Shape::Array
        };
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
            if !matches!(item, syn::Item::Enum(_)) {
                return;
            }
        }
        if matches!(item, syn::Item::Enum(_)) {
            assert!(
                catalog.representations.iter().any(|r| r.model == model),
                "unclassified enum: {model}"
            );
        }
        let groups: Vec<(&syn::Fields, String)> = match item {
            syn::Item::Struct(s) => vec![(&s.fields, prefix.to_string())],
            syn::Item::Enum(e) => e
                .variants
                .iter()
                .map(|v| (&v.fields, format!("{prefix}.{}", variant_name(e, v))))
                .collect(),
            _ => panic!("unclassified declaration: {model}"),
        };
        for (fields, prefix) in groups {
            let prefix = prefix.as_str();
            for (index, f) in fields.iter().enumerate() {
                let key = serde_name(
                    &f.attrs,
                    f.ident
                        .as_ref()
                        .map_or_else(|| index.to_string(), ToString::to_string),
                );
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
                let parent = prefix;
                let aliases: Vec<_> = serde_strings(&f.attrs, "alias")
                    .into_iter()
                    .map(|alias| {
                        if parent.is_empty() {
                            alias
                        } else {
                            format!("{parent}.{alias}")
                        }
                    })
                    .collect();
                assert_eq!(spec.file_aliases, aliases, "file aliases: {path}");
                let ty = type_name(&f.ty);
                assert_eq!(spec.model_type, ty, "type drift: {path}");
                assert_eq!(spec.declaring_model, model);
                assert_eq!(
                    spec.nullable,
                    model_type(&f.ty).starts_with("Option<"),
                    "optionality drift: {path}"
                );
                assert!(spec.boundary.is_none(), "unresolved boundary: {path}");
                assert_eq!(
                    spec.shape,
                    declared_shape(&model_type(&f.ty), catalog, graph),
                    "shape drift: {path}"
                );
                assert_eq!(
                    spec.key_class.is_some(),
                    spec.shape == Shape::Map,
                    "key policy: {path}"
                );
                if matches!(spec.shape, Shape::Section | Shape::SectionArray) {
                    assert_eq!(spec.value_class, Sensitivity::Aggregate);
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
                let inner = model_type(inner);
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
                            "f32",
                            "u8",
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
    assert_eq!(get("checks").env, EnvSupport::CollectionOrSection);
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

fn field_path(expr: &syn::Expr) -> Option<String> {
    match expr {
        syn::Expr::Path(path) => Some(
            path.path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect::<Vec<_>>()
                .join("::"),
        ),
        syn::Expr::Field(field) => {
            let syn::Member::Named(name) = &field.member else {
                return None;
            };
            Some(format!("{}.{name}", field_path(&field.base)?))
        }
        _ => None,
    }
}

fn reload_evidence(source: &str) -> (BTreeSet<String>, bool) {
    use syn::visit::Visit;
    #[derive(Default)]
    struct Evidence {
        copied: BTreeSet<String>,
        event_hub: bool,
    }
    impl<'ast> Visit<'ast> for Evidence {
        fn visit_expr_assign(&mut self, assign: &'ast syn::ExprAssign) {
            let right = match assign.right.as_ref() {
                syn::Expr::MethodCall(call) if call.method == "clone" && call.args.is_empty() => {
                    call.receiver.as_ref()
                }
                right => right,
            };
            if let (Some(left), Some(right)) = (field_path(&assign.left), field_path(right)) {
                if let Some(section) = left.strip_prefix("self.state.config.") {
                    if !section.contains('.') && right == format!("config.{section}") {
                        self.copied.insert(section.to_string());
                    }
                }
            }
            syn::visit::visit_expr_assign(self, assign);
        }
        fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
            if call.method == "set_read_max_wait_ms"
                && field_path(&call.receiver).as_deref() == Some("self.event_hub")
                && call.args.len() == 1
                && field_path(&call.args[0]).as_deref() == Some("config.events.read_max_wait_ms")
            {
                self.event_hub = true;
            }
            syn::visit::visit_expr_method_call(self, call);
        }
    }
    let file = syn::parse_file(source).unwrap();
    let method = file
        .items
        .iter()
        .filter_map(|item| {
            if let syn::Item::Impl(i) = item {
                Some(i)
            } else {
                None
            }
        })
        .filter(|i| type_name(&i.self_ty) == "App")
        .flat_map(|i| &i.items)
        .find_map(|item| {
            if let syn::ImplItem::Fn(f) = item {
                (f.sig.ident == "apply_live_config").then_some(f)
            } else {
                None
            }
        })
        .unwrap();
    let mut evidence = Evidence::default();
    evidence.visit_block(&method.block);
    (evidence.copied, evidence.event_hub)
}

#[test]
fn app_live_claims_match_actual_incoming_config_copies() {
    let catalog = core_catalog().unwrap();
    let (copied, _) = reload_evidence(include_str!("../../app/mod.rs"));
    assert!(
        copied_startup_sections(&catalog, &copied).is_empty(),
        "copied section classified ProcessStartup"
    );
    let unsupported: BTreeSet<_> = catalog
        .fields
        .iter()
        .filter(|f| f.reload == ReloadOwner::AppLive)
        .map(|f| {
            f.path
                .split('.')
                .next()
                .unwrap()
                .trim_end_matches("[]")
                .to_string()
        })
        .filter(|section| !copied.contains(section))
        .collect();
    assert!(
        unsupported.is_empty(),
        "AppLive sections not copied from incoming config: {unsupported:?}"
    );
}

#[test]
fn reload_evidence_rejects_self_copies_and_does_not_scan_other_methods() {
    let source = "impl App { fn apply_live_config(&mut self, config: &Config) { self.state.config.msg = self.state.config.msg.clone(); } fn unrelated(&mut self, config: &Config) { self.state.config.spawn = config.spawn.clone(); } }";
    assert_eq!(reload_evidence(source), (BTreeSet::new(), false));
}

#[test]
fn startup_limit_2397_and_event_hub_owner_match_reload_evidence() {
    let catalog = core_catalog().unwrap();
    let (copied, event_hub) = reload_evidence(include_str!("../../app/mod.rs"));
    assert!(
        event_hub,
        "event hub must receive config.events.read_max_wait_ms"
    );
    for field in &catalog.fields {
        if field.reload_issue == Some(2397) {
            assert_eq!(field.reload, ReloadOwner::ProcessStartup);
            assert!(
                !copied.contains(field.path.split('.').next().unwrap()),
                "#2397 changed: {} needs owner review",
                field.path
            );
        }
        if field.path == "events" || field.path.starts_with("events.") {
            assert_eq!(field.reload, ReloadOwner::EventHubLive);
        }
    }
    assert_eq!(
        catalog
            .fields
            .iter()
            .filter(|f| f.reload_issue == Some(2397))
            .count(),
        15
    );
}

#[test]
fn aliases_and_renamed_members_use_accepted_file_spellings() {
    let catalog = core_catalog().unwrap();
    let get = |path: &str| catalog.fields.iter().find(|f| f.path == path).unwrap();
    assert_eq!(get("keys.zoom").file_aliases, ["keys.fullscreen"]);
    assert_eq!(
        get("advanced.scrollback_limit_bytes").file_aliases,
        ["advanced.scrollback_lines"]
    );
    let members = &catalog
        .representations
        .iter()
        .find(|r| r.model == "AgentPanelScopeConfig")
        .unwrap()
        .members;
    assert!(members.iter().any(|m| m == "blocked&done"));
    assert!(!members.iter().any(|m| m == "BlockedAndDone"));
    assert!(catalog
        .representations
        .iter()
        .find(|r| r.model == "ToastFlockPosition")
        .unwrap()
        .members
        .iter()
        .any(|m| m == "top-left"));
}

#[test]
fn type_comparison_preserves_qualifiers_including_generic_arguments() {
    let a: syn::Type = syn::parse_quote!(alpha::Thing<beta::Item>);
    let b: syn::Type = syn::parse_quote!(other::Thing<beta::Item>);
    let c: syn::Type = syn::parse_quote!(alpha::Thing<other::Item>);
    assert_eq!(type_name(&a), "alpha::Thing<beta::Item>");
    assert_ne!(type_name(&a), type_name(&b));
    assert_ne!(type_name(&a), type_name(&c));
    let absolute: syn::Type = syn::parse_quote!(::alpha::Thing<beta::Item>);
    assert_ne!(type_name(&a), type_name(&absolute));
}

#[test]
#[should_panic(expected = "unsupported serde(flatten)")]
fn flattened_fields_require_explicit_inventory_review() {
    let catalog = core_catalog().unwrap();
    let mut graph = declarations();
    let syn::Item::Struct(ui) = graph.get_mut("UiConfig").unwrap() else {
        unreachable!()
    };
    ui.fields
        .iter_mut()
        .next()
        .unwrap()
        .attrs
        .push(syn::parse_quote!(#[serde(flatten)]));
    let _ = core_coverage(&catalog, &graph);
}

#[test]
fn imported_secrets_and_user_keys_are_not_public() {
    let catalog = core_catalog().unwrap();
    for field in &catalog.fields {
        if field.path == "structured.token_file"
            || field.path.starts_with("checks.issue_guard.")
                && ["owner", "repos", "gh_bin"]
                    .iter()
                    .any(|k| field.path.ends_with(k))
            || [
                "checks.script[].program",
                "checks.script[].args",
                "checks.script[].env",
                "channels[].name",
                "channels[].owner_node",
                "channels[].target",
            ]
            .contains(&field.path.as_str())
            || field.path.ends_with(".notify.title")
            || field.path.ends_with(".event.label")
            || field.path.ends_with(".write_digest.path_template")
        {
            assert_eq!(
                field.value_class,
                Sensitivity::PotentialSecret,
                "{}",
                field.path
            );
        }
        if field.shape == Shape::Map {
            assert_eq!(
                field.key_class,
                Some(Sensitivity::PotentialSecret),
                "{}",
                field.path
            );
        }
        assert!(field.boundary.is_none());
        assert_ne!(field.value_class, Sensitivity::Opaque);
    }
}

#[test]
fn reverse_reload_evidence_rejects_copied_startup_sections() {
    let mut catalog = core_catalog().unwrap();
    let (mut copied, _) = reload_evidence(include_str!("../../app/mod.rs"));
    assert!(copied_startup_sections(&catalog, &copied).is_empty());
    copied.insert("security".into());
    assert_eq!(
        copied_startup_sections(&catalog, &copied),
        BTreeSet::from(["security".into()])
    );
    copied.remove("security");
    catalog
        .fields
        .iter_mut()
        .find(|f| f.path == "session")
        .unwrap()
        .reload = ReloadOwner::ProcessStartup;
    assert_eq!(
        copied_startup_sections(&catalog, &copied),
        BTreeSet::from(["session".into()])
    );
}

fn copied_startup_sections(catalog: &Catalog, copied: &BTreeSet<String>) -> BTreeSet<String> {
    catalog
        .fields
        .iter()
        .filter(|f| {
            !f.path.contains('.')
                && copied.contains(&f.path)
                && f.reload == ReloadOwner::ProcessStartup
        })
        .map(|f| f.path.clone())
        .collect()
}

#[test]
fn an_unclassified_imported_leaf_is_rejected() {
    let catalog = core_catalog().unwrap();
    let mut graph = declarations();
    let syn::Item::Struct(s) = graph.get_mut("StructuredConfig").unwrap() else {
        unreachable!()
    };
    let syn::Fields::Named(fields) = &mut s.fields else {
        unreachable!()
    };
    fields.named.push(
        syn::Field::parse_named
            .parse_str("pub private_probe: Option<String>")
            .unwrap(),
    );
    assert_eq!(
        core_coverage(&catalog, &graph),
        Err(vec![
            "unclassified core field: structured.private_probe".into()
        ])
    );
}

#[test]
#[should_panic(expected = "representation drift: ActionSpec")]
fn an_unclassified_imported_variant_is_rejected() {
    let catalog = core_catalog().unwrap();
    let mut graph = declarations();
    let syn::Item::Enum(e) = graph.get_mut("ActionSpec").unwrap() else {
        unreachable!()
    };
    e.variants
        .push(syn::parse_str("PrivateProbe { value: String }").unwrap());
    let _ = core_coverage(&catalog, &graph);
}

#[test]
fn compiled_default_factories_keep_their_declared_context() {
    let catalog = core_catalog().unwrap();
    for field in &catalog.fields {
        let path = &field.path;
        let expected = match field.default.factory {
            Factory::Root => Some(path.as_str()),
            Factory::Peer => path.strip_prefix("peers[]."),
            Factory::KeyCommand => path.strip_prefix("keys.command[]."),
            Factory::CustomTheme => path.strip_prefix("theme.custom."),
            Factory::Script => path.strip_prefix("checks.script[]."),
            Factory::Restart => path
                .strip_prefix("session.restart.")
                .or_else(|| path.strip_prefix("session.restart_agents<key>.")),
            Factory::EmptyArray
            | Factory::EmptyMap
            | Factory::Required
            | Factory::CronTz
            | Factory::Action
            | Factory::Sound
            | Factory::DigestPath => Some(""),
            Factory::Opaque => panic!("unresolved default: {path}"),
        };
        assert_eq!(expected, Some(field.default.path.as_str()), "{path}");
    }
    let get = |path: &str| {
        catalog
            .fields
            .iter()
            .find(|f| f.path == path)
            .unwrap()
            .compiled_default()
            .unwrap()
    };
    assert_eq!(get("checks.cron[].expr"), CompiledDefault::Required);
    assert_eq!(get("channels[].name"), CompiledDefault::Required);
    assert_eq!(
        get("checks.script[].on_fire.write_digest.path_template"),
        CompiledDefault::Value {
            value: serde_json::json!("{date}.html"),
            parent_absent: true
        }
    );
}

#[test]
#[should_panic(expected = "unclassified enum: RunAs")]
fn an_enum_cannot_omit_its_variant_inventory() {
    let mut catalog = core_catalog().unwrap();
    catalog.representations.retain(|r| r.model != "RunAs");
    let _ = core_coverage(&catalog, &declarations());
}
