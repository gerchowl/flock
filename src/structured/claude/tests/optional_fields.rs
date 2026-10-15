//! Drift guard for ADR-0036 §8 metadata null preservation.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use syn::parse::{discouraged::Speculative, Parse, ParseStream};
use syn::{
    braced, parenthesized, Attribute, Field, Fields, GenericArgument, Ident, Item, PathArguments,
    Token, Type,
};

type Cases = BTreeSet<(String, String)>;

fn wire_name(field: &Field) -> String {
    let mut name = field.ident.as_ref().unwrap().to_string();
    for attribute in &field.attrs {
        if attribute.path().is_ident("serde") {
            attribute
                .parse_nested_meta(|meta| {
                    if meta.path.is_ident("rename") {
                        name = meta.value()?.parse::<syn::LitStr>()?.value();
                    } else if meta.input.peek(Token![=]) {
                        let _: syn::Expr = meta.value()?.parse()?;
                    }
                    Ok(())
                })
                .unwrap();
        }
    }
    name
}

// The macro template contains Rust fields among macro repetition tokens. Parse
// actual fields, descending into groups without interpreting macro variables.
struct EnvelopeFields(Vec<Field>);
impl Parse for EnvelopeFields {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut fields = Vec::new();
        while !input.is_empty() {
            let fork = input.fork();
            if let Ok(field) = Field::parse_named(&fork) {
                input.advance_to(&fork);
                // Envelope discriminators are required shape identity, not
                // nullable metadata (ADR-0036 §8), regardless of visibility.
                if !matches!(wire_name(&field).as_str(), "type" | "subtype") {
                    fields.push(field);
                }
            } else if input.peek(syn::token::Brace)
                || input.peek(syn::token::Bracket)
                || input.peek(syn::token::Paren)
            {
                let content;
                if input.peek(syn::token::Brace) {
                    braced!(content in input);
                } else if input.peek(syn::token::Bracket) {
                    syn::bracketed!(content in input);
                } else {
                    parenthesized!(content in input);
                }
                fields.extend(content.parse::<EnvelopeFields>()?.0);
            } else {
                input.step(|cursor| {
                    let (_, rest) = cursor.token_tree().ok_or_else(|| cursor.error("token"))?;
                    Ok(((), rest))
                })?;
            }
        }
        Ok(Self(fields))
    }
}

struct Event {
    name: String,
    required: Vec<Field>,
    optional: Vec<Field>,
}
struct Events(Vec<Event>);
impl Parse for Events {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut events = Vec::new();
        while !input.is_empty() {
            let _ = input.call(Attribute::parse_outer)?;
            let name = input.parse::<Ident>()?.to_string();
            let tags;
            parenthesized!(tags in input);
            let _: syn::LitStr = tags.parse()?;
            let _: Token![,] = tags.parse()?;
            let _: syn::LitStr = tags.parse()?;
            let _: Token![,] = tags.parse()?;
            let _: syn::LitStr = tags.parse()?;
            assert!(tags.is_empty(), "unexpected event discriminator syntax");
            let required;
            braced!(required in input);
            let required = required.parse_terminated(Field::parse_named, Token![,])?;
            let optional: Ident = input.parse()?;
            assert_eq!(optional, "optional");
            let fields;
            braced!(fields in input);
            let optional = fields.parse_terminated(Field::parse_named, Token![,])?;
            events.push(Event {
                name,
                required: required.into_iter().collect(),
                optional: optional.into_iter().collect(),
            });
        }
        Ok(Self(events))
    }
}

/// Source-only index of named adapter structs and aliases. Names must be
/// unique: ambiguity fails instead of silently choosing a different wire model.
/// This guard does not expand arbitrary macros or interpret custom serializers.
#[derive(Default)]
struct Models {
    fields: BTreeMap<String, Vec<Field>>,
    aliases: BTreeMap<String, Type>,
    names: BTreeSet<String>,
}

impl Models {
    fn insert(&mut self, name: String) {
        assert!(
            self.names.insert(name.clone()),
            "ambiguous model name {name}"
        );
    }

    /// Follow transparent wrappers and representative collection elements.
    /// Arrays/sequences use JSON pointer index 0, maps use key `example`, and
    /// tuples use their actual indices. Null fixtures must supply these values.
    /// Unsupported generic wrappers containing a model fail rather than hide it.
    fn walk(
        &self,
        ty: &Type,
        path: &str,
        optional: bool,
        ancestors: &mut BTreeSet<String>,
        paths: &mut BTreeSet<String>,
    ) {
        match ty {
            Type::Path(ty) => {
                let segment = ty.path.segments.last().unwrap();
                let name = segment.ident.to_string();
                let args: Vec<_> = match &segment.arguments {
                    PathArguments::AngleBracketed(args) => args
                        .args
                        .iter()
                        .filter_map(|arg| {
                            if let GenericArgument::Type(ty) = arg {
                                Some(ty)
                            } else {
                                None
                            }
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                if let Some(fields) = self.fields.get(&name) {
                    assert!(
                        args.is_empty(),
                        "generic model {name} needs explicit type substitution"
                    );
                    assert!(ancestors.insert(name.clone()), "recursive model {name}");
                    self.walk_fields(fields, path, true, ancestors, paths);
                    ancestors.remove(&name);
                } else if let Some(alias) = self.aliases.get(&name) {
                    assert!(
                        args.is_empty(),
                        "generic alias {name} needs explicit type substitution"
                    );
                    assert!(
                        ancestors.insert(name.clone()),
                        "recursive model alias {name}"
                    );
                    self.walk(alias, path, optional, ancestors, paths);
                    ancestors.remove(&name);
                } else {
                    match name.as_str() {
                        "Option" => {
                            assert_eq!(args.len(), 1, "Option must have one type argument");
                            if optional {
                                assert!(
                                    paths.insert(path.to_owned()),
                                    "duplicate modeled optional field {path}"
                                );
                            }
                            self.walk(args[0], path, false, ancestors, paths);
                        }
                        "Box" | "Rc" | "Arc" | "Cow" | "RefCell" | "Cell" | "Mutex" | "RwLock" => {
                            assert_eq!(args.len(), 1, "transparent wrapper {name}");
                            self.walk(args[0], path, optional, ancestors, paths);
                        }
                        "Vec" | "VecDeque" | "LinkedList" | "BTreeSet" | "HashSet" => {
                            assert_eq!(args.len(), 1, "sequence {name}");
                            self.walk(args[0], &format!("{path}/0"), true, ancestors, paths);
                        }
                        "Map" | "BTreeMap" | "HashMap" => {
                            assert_eq!(args.len(), 2, "map {name}");
                            self.walk(args[1], &format!("{path}/example"), true, ancestors, paths);
                        }
                        _ => {
                            let mut nested = BTreeSet::new();
                            for arg in args {
                                self.walk(arg, path, true, ancestors, &mut nested);
                            }
                            assert!(
                                nested.is_empty(),
                                "unsupported wrapper {name} contains optional model fields"
                            );
                        }
                    }
                }
            }
            Type::Reference(ty) => self.walk(&ty.elem, path, optional, ancestors, paths),
            Type::Paren(ty) => self.walk(&ty.elem, path, optional, ancestors, paths),
            Type::Group(ty) => self.walk(&ty.elem, path, optional, ancestors, paths),
            Type::Array(ty) => self.walk(&ty.elem, &format!("{path}/0"), true, ancestors, paths),
            Type::Slice(ty) => self.walk(&ty.elem, &format!("{path}/0"), true, ancestors, paths),
            Type::Tuple(ty) => {
                for (index, element) in ty.elems.iter().enumerate() {
                    self.walk(element, &format!("{path}/{index}"), true, ancestors, paths);
                }
            }
            _ => panic!("unsupported wire type at {path}"),
        }
    }

    fn walk_fields(
        &self,
        fields: &[Field],
        prefix: &str,
        optional: bool,
        ancestors: &mut BTreeSet<String>,
        paths: &mut BTreeSet<String>,
    ) {
        for field in fields {
            let key = wire_name(field).replace('~', "~0").replace('/', "~1");
            self.walk(
                &field.ty,
                &format!("{prefix}/{key}"),
                optional,
                ancestors,
                paths,
            );
        }
    }
}

#[derive(Default)]
struct Catalogue {
    models: Models,
    envelope: Option<Vec<Field>>,
    events: Option<Vec<Event>>,
}

/// Read the declared module tree, including inline modules and #[path] files.
/// Test-only modules are excluded; missing/ambiguous files and duplicate model
/// names fail. External crates remain leaves; this is not a Rust name resolver.
fn collect_items(
    items: Vec<Item>,
    source_dir: &Path,
    module_dir: &Path,
    read: &mut impl FnMut(&Path) -> Option<String>,
    catalogue: &mut Catalogue,
) {
    for item in items {
        match item {
            Item::Struct(model) => {
                catalogue.models.insert(model.ident.to_string());
                assert!(
                    model.generics.params.is_empty(),
                    "generic model requires explicit type substitution"
                );
                if let Fields::Named(fields) = model.fields {
                    catalogue
                        .models
                        .fields
                        .insert(model.ident.to_string(), fields.named.into_iter().collect());
                }
            }
            Item::Type(alias) => {
                catalogue.models.insert(alias.ident.to_string());
                assert!(
                    alias.generics.params.is_empty(),
                    "generic alias requires explicit type substitution"
                );
                catalogue
                    .models
                    .aliases
                    .insert(alias.ident.to_string(), *alias.ty);
            }
            Item::Mod(module) => {
                if module.attrs.iter().any(|attr| matches!(&attr.meta,
                    syn::Meta::List(meta) if meta.path.is_ident("cfg") && meta.tokens.to_string() == "test")) {
                    continue;
                }
                let child_dir = module_dir.join(module.ident.to_string());
                if let Some((_, items)) = module.content {
                    collect_items(items, &child_dir, &child_dir, read, catalogue);
                    continue;
                }
                let explicit = module.attrs.iter().find_map(|attr| {
                    if !attr.path().is_ident("path") {
                        return None;
                    }
                    let syn::Meta::NameValue(value) = &attr.meta else {
                        panic!("invalid module path attribute")
                    };
                    let syn::Expr::Lit(value) = &value.value else {
                        panic!("module path must be literal")
                    };
                    let syn::Lit::Str(path) = &value.lit else {
                        panic!("module path must be a string")
                    };
                    Some(source_dir.join(path.value()))
                });
                let candidates = explicit.map_or_else(
                    || {
                        vec![
                            module_dir.join(format!("{}.rs", module.ident)),
                            child_dir.join("mod.rs"),
                        ]
                    },
                    |path| vec![path],
                );
                let found: Vec<_> = candidates
                    .iter()
                    .filter_map(|path| read(path).map(|source| (path, source)))
                    .collect();
                assert_eq!(
                    found.len(),
                    1,
                    "missing or ambiguous module {}: {candidates:?}",
                    module.ident
                );
                let (path, source) = &found[0];
                let file = syn::parse_file(source).expect("valid module source");
                let parent = path.parent().unwrap();
                let next = if path.file_name().unwrap() == "mod.rs" {
                    parent.to_path_buf()
                } else {
                    parent.join(path.file_stem().unwrap())
                };
                collect_items(file.items, parent, &next, read, catalogue);
            }
            Item::Macro(model) if model.ident.as_ref().is_some_and(|id| id == "events") => {
                assert!(
                    catalogue
                        .envelope
                        .replace(syn::parse2::<EnvelopeFields>(model.mac.tokens).unwrap().0)
                        .is_none(),
                    "duplicate events macro"
                );
            }
            Item::Macro(model) if model.mac.path.is_ident("events") => {
                assert!(
                    catalogue
                        .events
                        .replace(syn::parse2::<Events>(model.mac.tokens).unwrap().0)
                        .is_none(),
                    "duplicate events catalogue"
                );
            }
            _ => {}
        }
    }
}

fn modeled_cases_with_reader(
    source: &str,
    dir: &Path,
    read: &mut impl FnMut(&Path) -> Option<String>,
) -> Cases {
    let mut catalogue = Catalogue::default();
    collect_items(
        syn::parse_file(source).unwrap().items,
        dir,
        dir,
        read,
        &mut catalogue,
    );
    let envelope = catalogue.envelope.expect("events macro definition");
    assert!(!envelope.is_empty(), "missing envelope metadata fields");
    let mut cases = Cases::new();
    for event in catalogue.events.expect("events catalogue") {
        let mut paths = BTreeSet::new();
        for (fields, optional) in [(&envelope, true), (&event.required, false)] {
            catalogue
                .models
                .walk_fields(fields, "", optional, &mut BTreeSet::new(), &mut paths);
        }
        for field in event.optional {
            let key = wire_name(&field).replace('~', "~0").replace('/', "~1");
            let path = format!("/{key}");
            assert!(
                paths.insert(path.clone()),
                "duplicate optional field {path}"
            );
            catalogue
                .models
                .walk(&field.ty, &path, false, &mut BTreeSet::new(), &mut paths);
        }
        cases.extend(paths.into_iter().map(|path| (event.name.clone(), path)));
    }
    cases
}

#[allow(clippy::disallowed_methods)] // Test-only discovery reads adapter source; no runtime file IO is added.
fn modeled_cases(source: &str) -> Cases {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/structured/claude");
    modeled_cases_with_reader(
        source,
        &dir,
        &mut |path| match std::fs::read_to_string(path) {
            Ok(source) => Some(source),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => panic!("cannot read model source {}: {err}", path.display()),
        },
    )
}

#[test]
fn optional_null_cases_cover_every_modeled_field() {
    let modeled = modeled_cases(include_str!("../mod.rs"));
    let mut tested = Cases::new();
    for (variant, paths) in super::OPTIONAL_NULL_CASES {
        for path in *paths {
            assert!(
                tested.insert((variant.to_string(), path.to_string())),
                "duplicate null case"
            );
        }
    }
    let missing: Vec<_> = modeled.difference(&tested).collect();
    let stale: Vec<_> = tested.difference(&modeled).collect();
    assert!(
        missing.is_empty() && stale.is_empty(),
        "null case drift: missing {missing:?}, stale {stale:?}"
    );
}

#[test]
fn discovery_preserves_wire_names_and_required_nullable_fields() {
    let source = r#"
        struct Details {
            #[serde(rename = "wireNested")]
            pub nested: Option<Option<String>>,
            pub payload: String,
        }
        macro_rules! events {
            () => {
                struct Envelope {
                    #[serde(rename = "wireEnvelope")]
                    pub shared: Option<Option<String>>,
                    subtype: Option<String>,
                }
            }
        }
        events! {
            Example("example", "", "") {
                nullable_required: Option<String>,
                #[serde(rename = "wireDetails")]
                details: Details,
            } optional {
                #[serde(rename = "wireOptional")]
                optional: bool,
            }
        }
    "#;
    let expected = ["/wireEnvelope", "/wireDetails/wireNested", "/wireOptional"]
        .into_iter()
        .map(|path| ("Example".to_owned(), path.to_owned()))
        .collect();
    assert_eq!(modeled_cases(source), expected);
}

#[test]
fn wrapped_optional_metadata_discovers_real_nested_fields() {
    let original = include_str!("../mod.rs");
    let source = original.replacen(
        "tool_use_meta: Value,",
        "tool_use_meta: Option<Vec<Box<ToolRequest>>>,",
        1,
    );
    assert_ne!(source, original, "real metadata field must still exist");
    let cases = modeled_cases(&source);
    for field in [
        "display_name",
        "description",
        "permission_suggestions",
        "requires_user_interaction",
    ] {
        assert!(
            cases.contains(&("Assistant".into(), format!("/tool_use_meta/0/{field}"))),
            "missing nested {field}"
        );
    }
}

#[test]
fn private_envelope_metadata_is_discovered() {
    let source = r#"
        macro_rules! events { () => {
            struct Envelope {
                subtype: Option<String>,
                #[serde(rename = "privateWire")]
                metadata: Option<Option<String>>,
            }
        } }
        events! { Example("example", "", "") {} optional {} }
    "#;
    assert_eq!(
        modeled_cases(source),
        [("Example".into(), "/privateWire".into())]
            .into_iter()
            .collect()
    );
}

/// Construct a small event catalogue without changing the real wire model.
fn fixture_source(models: &str, required: &str, optional: &str) -> String {
    format!(
        r#"
        {models}
        macro_rules! events {{ () => {{ struct Envelope {{ metadata: Option<String> }} }} }}
        events! {{ Example("example", "", "") {{ {required} }} optional {{ {optional} }} }}
    "#
    )
}

#[test]
fn wrappers_preserve_object_array_and_tuple_wire_paths() {
    let source = fixture_source(
        "struct Detail { value: Option<String> } type Alias = Box<Detail>;",
        "optional: Option<Vec<Box<Detail>>>, boxed: Box<Detail>, shared: std::sync::Arc<Detail>, alias: Alias, tuple: (String, Detail), array: [Detail; 2], map: BTreeMap<String, Detail>,",
        "optional: Option<Vec<Detail>>,",
    );
    // Use distinct field names so duplicate JSON paths remain an error.
    let source = source.replacen(
        "optional: Option<Vec<Box<Detail>>>",
        "required_nullable: Option<Vec<Box<Detail>>>",
        1,
    );
    let expected = [
        "/metadata",
        "/required_nullable/0/value",
        "/boxed/value",
        "/shared/value",
        "/alias/value",
        "/tuple/1/value",
        "/array/0/value",
        "/map/example/value",
        "/optional",
        "/optional/0/value",
    ]
    .into_iter()
    .map(|path| ("Example".to_owned(), path.to_owned()))
    .collect();
    assert_eq!(modeled_cases(&source), expected);
}

#[test]
#[allow(clippy::disallowed_methods)] // This regression follows real fixture module files through the same reader as production-source discovery.
fn discovery_follows_module_files_and_path_overrides() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-session/optional-fields");
    let root = std::fs::read_to_string(dir.join("mod.rs")).unwrap();
    let cases = modeled_cases_with_reader(&root, &dir, &mut |path| match std::fs::read_to_string(
        path,
    ) {
        Ok(source) => Some(source),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => panic!("{err}"),
    });
    let expected = [
        "/privateWire",
        "/records/0/leaf",
        "/records/0/leaf/wire~1leaf~0",
        "/inline/note",
        "/relocated/note",
        "/optional_records",
        "/optional_records/0/leaf",
        "/optional_records/0/leaf/wire~1leaf~0",
    ]
    .into_iter()
    .map(|path| ("Example".to_owned(), path.to_owned()))
    .collect();
    assert_eq!(cases, expected);
}

#[test]
#[should_panic(expected = "missing or ambiguous module")]
fn missing_module_source_fails_discovery() {
    let source = fixture_source("mod missing;", "value: missing::Detail,", "");
    modeled_cases_with_reader(&source, Path::new("fixture"), &mut |_| None);
}

#[test]
#[should_panic(expected = "missing or ambiguous module")]
fn ambiguous_module_source_fails_discovery() {
    let source = fixture_source("mod ambiguous;", "", "");
    modeled_cases_with_reader(&source, Path::new("fixture"), &mut |_| Some("".into()));
}

#[test]
#[should_panic(expected = "recursive model")]
fn recursion_through_wrappers_fails_without_hanging() {
    let source = fixture_source("struct Node { next: Option<Box<Node>> }", "node: Node,", "");
    modeled_cases(&source);
}

#[test]
#[should_panic(expected = "ambiguous model name")]
fn ambiguous_model_names_fail_instead_of_choosing_a_file() {
    let source = fixture_source(
        "mod a { struct Detail {} } mod b { struct Detail {} }",
        "",
        "",
    );
    modeled_cases(&source);
}

#[test]
#[should_panic(expected = "unsupported wrapper")]
fn unknown_generic_wire_wrappers_fail_instead_of_hiding_fields() {
    let source = fixture_source(
        "struct Detail { value: Option<String> }",
        "value: Custom<Detail>,",
        "",
    );
    modeled_cases(&source);
}
