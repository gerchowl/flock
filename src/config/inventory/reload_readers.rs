//! Focused consumer evidence for msg/spawn, including command snapshots.
//!
//! Only proven bindings carry the internal config marker, so an unrelated
//! local named `msg` is not mistaken for a live config reader.
//! This is a syntax audit, not Rust type inference: follow AppState config
//! reads, local reference aliases, reachable config helper methods and the
//! MCP constructor with proven startup or live config inputs. Unknown or
//! missing reader evidence fails closed.

use super::*;
use std::path::{Path, PathBuf};
use syn::visit::Visit;

fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("test")
            || (a.path().is_ident("cfg")
                && a.parse_args::<syn::Path>()
                    .is_ok_and(|p| p.is_ident("test")))
    })
}

fn production_items(path: &Path, output: &mut Vec<syn::Item>) {
    fn collect(items: Vec<syn::Item>, base: &Path, output: &mut Vec<syn::Item>) {
        for item in items {
            if let syn::Item::Mod(module) = item {
                if test_only(&module.attrs) {
                    continue;
                }
                if let Some((_, items)) = module.content {
                    collect(items, &base.join(module.ident.to_string()), output);
                } else {
                    let explicit = module.attrs.iter().find_map(|a| {
                        if let syn::Meta::NameValue(value) = &a.meta {
                            if value.path.is_ident("path") {
                                if let syn::Expr::Lit(syn::ExprLit {
                                    lit: syn::Lit::Str(path),
                                    ..
                                }) = &value.value
                                {
                                    return Some(base.join(path.value()));
                                }
                            }
                        }
                        None
                    });
                    let direct = base.join(format!("{}.rs", module.ident));
                    let path = explicit.unwrap_or_else(|| {
                        if direct.is_file() {
                            direct
                        } else {
                            base.join(module.ident.to_string()).join("mod.rs")
                        }
                    });
                    production_items(&path, output);
                }
            } else {
                output.push(item);
            }
        }
    }
    let source = std::fs::read_to_string(path).unwrap();
    let items = syn::parse_file(&source).unwrap().items;
    let base = if path.file_name().unwrap() == "mod.rs" {
        path.parent().unwrap().to_path_buf()
    } else {
        path.with_extension("")
    };
    collect(items, &base, output);
}

fn derived_clone_models(items: &[syn::Item]) -> BTreeSet<String> {
    items
        .iter()
        .filter_map(|item| {
            let syn::Item::Struct(model) = item else {
                return None;
            };
            model
                .attrs
                .iter()
                .any(|attr| {
                    attr.path().is_ident("derive") && attr.parse_args_with(
                syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
            ).is_ok_and(|paths| paths.iter().any(|path| path.is_ident("Clone")))
                })
                .then(|| model.ident.to_string())
        })
        .collect()
}

/// Helper lookup uses the nominal model, independent of an impl's lifetimes.
/// Config declaration formatting remains strict about every generic argument.
fn helper_model(ty: &syn::Type) -> String {
    let syn::Type::Path(path) = ty else {
        panic!("unclassified helper receiver")
    };
    assert!(path.qself.is_none(), "qualified helper receiver");
    path.path.segments.last().unwrap().ident.to_string()
}

#[test]
fn helper_lookup_accepts_lifetimes_without_weakening_declared_types() {
    assert_eq!(
        helper_model(&syn::parse_str("crate::app::LocalOperator<'_>").unwrap()),
        "LocalOperator"
    );
    assert_eq!(
        type_name(&syn::parse_str("Option<crate::config::MsgConfig>").unwrap()),
        "Option<crate::config::MsgConfig>"
    );
}

struct Readers<'a> {
    aliases: BTreeMap<String, String>,
    fields: &'a BTreeSet<String>,
    derived_clones: &'a BTreeSet<String>,
    models: &'a BTreeMap<String, String>,
    helpers: &'a BTreeMap<(String, String), syn::ImplItemFn>,
    active_helpers: BTreeSet<(String, String)>,
    live: BTreeSet<String>,
    command: BTreeSet<String>,
    startup_loads: usize,
    command_context: bool,
    function: String,
}

impl Readers<'_> {
    fn function(&mut self, signature: &syn::Signature, block: &syn::Block) {
        let previous = std::mem::take(&mut self.aliases);
        let previous_function = std::mem::replace(&mut self.function, signature.ident.to_string());
        for input in &signature.inputs {
            let syn::FnArg::Typed(param) = input else {
                continue;
            };
            let syn::Pat::Ident(name) = param.pat.as_ref() else {
                continue;
            };
            let syn::Type::Reference(reference) = param.ty.as_ref() else {
                continue;
            };
            let syn::Type::Path(path) = reference.elem.as_ref() else {
                continue;
            };
            if path.qself.is_none()
                && path.path.segments.last().is_some_and(|s| {
                    s.ident == "Config" && matches!(s.arguments, syn::PathArguments::None)
                })
            {
                self.aliases
                    .insert(name.ident.to_string(), "@config".into());
            }
        }
        self.visit_block(block);
        self.aliases = previous;
        self.function = previous_function;
    }

    fn path(&self, expr: &syn::Expr) -> Option<String> {
        match expr {
            syn::Expr::Reference(r) => self.path(&r.expr),
            syn::Expr::Paren(p) => self.path(&p.expr),
            syn::Expr::Path(p) => {
                let path = p
                    .path
                    .segments
                    .iter()
                    .map(|s| s.ident.to_string())
                    .collect::<Vec<_>>()
                    .join("::");
                Some(self.aliases.get(&path).cloned().unwrap_or(path))
            }
            syn::Expr::Field(f) => {
                let syn::Member::Named(name) = &f.member else {
                    return None;
                };
                let path = format!("{}.{name}", self.path(&f.base)?);
                Some(
                    path.strip_prefix("self.state.config.")
                        .map(|tail| format!("@config.{tail}"))
                        .unwrap_or(path),
                )
            }
            syn::Expr::Call(call)
                if field_path(&call.func).is_some_and(|p| p.ends_with("Config::load")) =>
            {
                Some("@load".into())
            }
            _ => None,
        }
    }

    fn helper(&mut self, model: &str, method: &str, aliases: BTreeMap<String, String>) {
        let key = (model.to_string(), method.to_string());
        let helper = self
            .helpers
            .get(&key)
            .unwrap_or_else(|| panic!("missing consumer helper: {key:?}"))
            .clone();
        assert!(
            self.active_helpers.insert(key.clone()),
            "recursive helper: {key:?}"
        );
        let previous = std::mem::replace(&mut self.aliases, aliases);
        self.visit_block(&helper.block);
        self.aliases = previous;
        self.active_helpers.remove(&key);
    }
}

impl<'ast> Visit<'ast> for Readers<'_> {
    fn visit_block(&mut self, block: &'ast syn::Block) {
        let previous = self.aliases.clone();
        syn::visit::visit_block(self, block);
        self.aliases = previous;
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        if let (syn::Pat::Ident(name), Some(init)) = (&local.pat, &local.init) {
            if let Some(path) = self.path(&init.expr) {
                self.aliases.insert(name.ident.to_string(), path);
            }
        }
        syn::visit::visit_local(self, local);
    }

    fn visit_expr_assign(&mut self, assign: &'ast syn::ExprAssign) {
        // A write is not evidence of a consumer reading the value.
        self.visit_expr(&assign.right);
    }

    fn visit_expr_field(&mut self, field: &'ast syn::ExprField) {
        if let syn::Member::Named(name) = &field.member {
            if let Some(base) = self.path(&field.base) {
                let path = format!("{base}.{name}");
                let path = path
                    .strip_prefix("self.state.config.")
                    .map(|tail| format!("@config.{tail}"))
                    .unwrap_or(path);
                if let Some(path) = path
                    .strip_prefix("@config.")
                    .filter(|p| self.fields.contains(*p))
                {
                    if self.command_context {
                        self.command.insert(path.to_string());
                    } else {
                        self.live.insert(path.to_string());
                    }
                }
            }
        }
        syn::visit::visit_expr_field(self, field);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if let Some(path) = self.path(&call.receiver) {
            if let Some(section) = path.strip_prefix("@config.") {
                if let Some(model) = self.models.get(section) {
                    // Derived whole-section clones are copy evidence, checked
                    // by reload_evidence, rather than leaf consumer evidence.
                    if call.method == "clone"
                        && call.args.is_empty()
                        && self.derived_clones.contains(model)
                    {
                        syn::visit::visit_expr_method_call(self, call);
                        return;
                    }
                    self.helper(
                        model,
                        &call.method.to_string(),
                        BTreeMap::from([("self".into(), path)]),
                    );
                }
            }
        }
        syn::visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if field_path(&call.func).is_some_and(|p| p.ends_with("ChannelOptions::from_config")) {
            assert_eq!(call.args.len(), 1);
            let path = self.path(&call.args[0]).unwrap();
            let (section, command) = if let Some(section) = path.strip_prefix("@load.config.") {
                assert_eq!(
                    self.function, "serve_over_stdio",
                    "constructor moved outside the command startup entry"
                );
                self.startup_loads += 1;
                (section, true)
            } else {
                (
                    path.strip_prefix("@config.")
                        .expect("MCP constructor must read a proven config section"),
                    false,
                )
            };
            let constructor = &self.helpers[&("ChannelOptions".into(), "from_config".into())];
            let syn::FnArg::Typed(param) = &constructor.sig.inputs[0] else {
                panic!("constructor argument")
            };
            let syn::Pat::Ident(name) = param.pat.as_ref() else {
                panic!("constructor binding")
            };
            let syn::Type::Reference(reference) = param.ty.as_ref() else {
                panic!("constructor type")
            };
            assert_eq!(model_type(&reference.elem), self.models[section]);
            let previous_context = std::mem::replace(&mut self.command_context, command);
            self.helper(
                "ChannelOptions",
                "from_config",
                BTreeMap::from([(name.ident.to_string(), format!("@config.{section}"))]),
            );
            self.command_context = previous_context;
        }
        syn::visit::visit_expr_call(self, call);
    }
}

#[test]
fn msg_spawn_leaf_owners_match_consumer_evidence() {
    assert_msg_spawn_leaf_owners(&core_catalog().unwrap());
}

fn assert_msg_spawn_leaf_owners(catalog: &Catalog) {
    let (copied, _) = reload_evidence(include_str!("../../app/mod.rs"));
    let models: BTreeMap<_, _> = catalog
        .fields
        .iter()
        .filter(|f| matches!(f.path.as_str(), "msg" | "spawn"))
        .map(|f| {
            (
                f.path.clone(),
                model_type(&syn::parse_str::<syn::Type>(&f.model_type).unwrap()),
            )
        })
        .collect();
    let fields: BTreeSet<_> = catalog
        .fields
        .iter()
        .filter(|f| {
            models
                .keys()
                .any(|root| f.path.starts_with(&format!("{root}.")))
        })
        .map(|f| f.path.clone())
        .collect();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut items = Vec::new();
    production_items(&root.join("app/mod.rs"), &mut items);
    production_items(&root.join("mcp/mod.rs"), &mut items);
    let mut helper_items = syn::parse_file(include_str!("../model.rs")).unwrap().items;
    helper_items.extend(items.clone());
    let derived_clones = derived_clone_models(&declarations().into_values().collect::<Vec<_>>());
    let helpers = helper_items
        .iter()
        .filter_map(|i| {
            if let syn::Item::Impl(i) = i {
                Some(i)
            } else {
                None
            }
        })
        .flat_map(|i| {
            i.items.iter().filter_map(move |item| match item {
                syn::ImplItem::Fn(f) if !test_only(&f.attrs) => Some((
                    (helper_model(&i.self_ty), f.sig.ident.to_string()),
                    f.clone(),
                )),
                _ => None,
            })
        })
        .collect();
    let mut readers = Readers {
        aliases: BTreeMap::new(),
        fields: &fields,
        derived_clones: &derived_clones,
        models: &models,
        helpers: &helpers,
        active_helpers: BTreeSet::new(),
        live: BTreeSet::new(),
        command: BTreeSet::new(),
        startup_loads: 0,
        command_context: false,
        function: String::new(),
    };
    for item in &items {
        let functions: Vec<_> = match item {
            syn::Item::Fn(f) if !test_only(&f.attrs) => vec![(&f.sig, &*f.block)],
            syn::Item::Impl(i) if !test_only(&i.attrs) => i
                .items
                .iter()
                .filter_map(|item| match item {
                    syn::ImplItem::Fn(f) if !test_only(&f.attrs) => Some((&f.sig, &f.block)),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        for (signature, block) in functions {
            readers.function(signature, block);
        }
    }
    assert_eq!(
        readers.startup_loads, 1,
        "MCP command load must have consumer evidence"
    );
    let expected: BTreeMap<_, _> = fields
        .iter()
        .map(|path| {
            let live = readers.live.contains(path);
            let command = readers.command.contains(path);
            if live {
                assert!(
                    copied.contains(path.split('.').next().unwrap()),
                    "{path}: no live copy"
                );
            }
            let owner = match (live, command) {
                (true, true) => ReloadOwner::Mixed,
                (true, false) => ReloadOwner::AppLive,
                (false, true) => ReloadOwner::CommandStartup,
                (false, false) => panic!("{path}: no consumer evidence"),
            };
            (path.clone(), owner)
        })
        .collect();
    for field in &catalog.fields {
        if let Some(owner) = expected.get(&field.path) {
            assert_eq!(
                &field.reload,
                owner,
                "{}: live={}, command={}",
                field.path,
                readers.live.contains(&field.path),
                readers.command.contains(&field.path)
            );
        } else if models.contains_key(&field.path) {
            let mut owners = expected
                .iter()
                .filter(|(path, _)| path.starts_with(&format!("{}.", field.path)))
                .map(|(_, owner)| owner);
            let first = owners.next().unwrap();
            let owner = if owners.all(|owner| owner == first) {
                first
            } else {
                &ReloadOwner::Mixed
            };
            assert_eq!(&field.reload, owner, "{}: aggregate owner", field.path);
        }
    }
}

fn constructor_evidence(source: &str) -> (BTreeSet<String>, BTreeSet<String>, usize) {
    let fields = BTreeSet::from(["msg.channel_push".into()]);
    let models = BTreeMap::from([("msg".into(), "MsgConfig".into())]);
    let constructor: syn::ImplItemFn = syn::parse_str(
        "fn from_config(msg: &MsgConfig) -> Self { Self { push: msg.channel_push } }",
    )
    .unwrap();
    let helpers = BTreeMap::from([(("ChannelOptions".into(), "from_config".into()), constructor)]);
    let derived_clones = derived_clone_models(&[syn::parse_str(
        "#[derive(Clone)] struct MsgConfig { channel_push: bool }",
    )
    .unwrap()]);
    let mut readers = Readers {
        aliases: BTreeMap::new(),
        fields: &fields,
        derived_clones: &derived_clones,
        models: &models,
        helpers: &helpers,
        active_helpers: BTreeSet::new(),
        live: BTreeSet::new(),
        command: BTreeSet::new(),
        startup_loads: 0,
        command_context: false,
        function: String::new(),
    };
    let function: syn::ItemFn = syn::parse_str(source).unwrap();
    readers.function(&function.sig, &function.block);
    (readers.live, readers.command, readers.startup_loads)
}

#[test]
fn constructor_evidence_follows_typed_live_config_and_reference_aliases() {
    let (live, command, loads) = constructor_evidence(
        "fn compare(candidate: &crate::config::Config) {
            let policy = &candidate.msg;
            ChannelOptions::from_config(policy);
        }",
    );
    assert_eq!(live, BTreeSet::from(["msg.channel_push".into()]));
    assert!(command.is_empty());
    assert_eq!(loads, 0);
}

#[test]
fn constructor_evidence_keeps_command_loads_separate() {
    let (live, command, loads) = constructor_evidence(
        "fn serve_over_stdio() {
            let loaded = Config::load();
            ChannelOptions::from_config(&loaded.config.msg);
        }",
    );
    assert!(live.is_empty());
    assert_eq!(command, BTreeSet::from(["msg.channel_push".into()]));
    assert_eq!(loads, 1);
}

#[test]
#[should_panic(expected = "MCP constructor must read a proven config section")]
fn constructor_evidence_rejects_unproven_parameter_types() {
    constructor_evidence(
        "fn compare(candidate: &OtherConfig) { ChannelOptions::from_config(&candidate.msg); }",
    );
}

#[test]
#[should_panic(expected = "constructor moved outside the command startup entry")]
fn constructor_evidence_rejects_relocated_command_loads() {
    constructor_evidence(
        "fn another_entry() {
            let loaded = Config::load();
            ChannelOptions::from_config(&loaded.config.msg);
        }",
    );
}

#[test]
fn constructor_evidence_does_not_count_derived_section_copies_as_consumers() {
    let (live, command, loads) =
        constructor_evidence("fn copy_config(config: &Config) { let copy = config.msg.clone(); }");
    assert!(live.is_empty() && command.is_empty());
    assert_eq!(loads, 0);
}

#[test]
fn startup_only_labels_are_rejected_for_live_mcp_comparison_readers() {
    for path in [
        "msg.channel_push_body_max_bytes",
        "msg.channel_push_reconnect_secs",
    ] {
        let mut catalog = core_catalog().unwrap();
        catalog
            .fields
            .iter_mut()
            .find(|field| field.path == path)
            .unwrap()
            .reload = ReloadOwner::CommandStartup;
        let failure = std::panic::catch_unwind(|| assert_msg_spawn_leaf_owners(&catalog))
            .expect_err("a startup-only label must not hide the live diagnostic reader");
        let message = failure.downcast_ref::<String>().unwrap();
        assert!(
            message.contains(&format!("{path}: live=true, command=true")),
            "{message}"
        );
    }
}
