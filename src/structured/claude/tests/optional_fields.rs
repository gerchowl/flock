//! Drift guard for ADR-0036 §8 metadata null preservation.
use std::collections::{BTreeMap, BTreeSet};

use syn::parse::{discouraged::Speculative, Parse, ParseStream};
use syn::{braced, parenthesized, Attribute, Field, Fields, Ident, Item, Token, Type, Visibility};

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

fn type_name(ty: &Type) -> Option<String> {
    match ty {
        Type::Path(path) => Some(path.path.segments.last()?.ident.to_string()),
        _ => None,
    }
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
                if matches!(field.vis, Visibility::Public(_))
                    && type_name(&field.ty).as_deref() == Some("Option")
                {
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

fn nested_fields(
    fields: &[Field],
    models: &BTreeMap<String, Vec<Field>>,
    prefix: &str,
    ancestors: &mut BTreeSet<String>,
    paths: &mut BTreeSet<String>,
    include_optional: bool,
) {
    for field in fields {
        let path = format!("{prefix}/{}", wire_name(field));
        let Some(ty) = type_name(&field.ty) else {
            continue;
        };
        if ty == "Option" {
            if include_optional {
                assert!(paths.insert(path), "duplicate modeled optional field");
            }
        } else if let Some(nested) = models.get(&ty) {
            assert!(ancestors.insert(ty.clone()), "recursive model {ty}");
            nested_fields(nested, models, &path, ancestors, paths, true);
            ancestors.remove(&ty);
        }
    }
}

fn modeled_cases(source: &str) -> Cases {
    let file = syn::parse_file(source).unwrap();
    let mut models = BTreeMap::new();
    let mut envelope = None;
    let mut events = None;
    for item in file.items {
        match item {
            Item::Struct(model) => {
                if let Fields::Named(fields) = model.fields {
                    models.insert(model.ident.to_string(), fields.named.into_iter().collect());
                }
            }
            Item::Macro(model) if model.ident.as_ref().is_some_and(|id| id == "events") => {
                envelope = Some(syn::parse2::<EnvelopeFields>(model.mac.tokens).unwrap().0);
            }
            Item::Macro(model) if model.mac.path.is_ident("events") => {
                events = Some(syn::parse2::<Events>(model.mac.tokens).unwrap().0);
            }
            _ => {}
        }
    }
    let envelope = envelope.expect("events macro definition");
    assert!(!envelope.is_empty(), "missing envelope optional fields");
    let mut cases = Cases::new();
    for event in events.expect("events catalogue") {
        let mut paths = BTreeSet::new();
        nested_fields(
            &envelope,
            &models,
            "",
            &mut BTreeSet::new(),
            &mut paths,
            true,
        );
        nested_fields(
            &event.required,
            &models,
            "",
            &mut BTreeSet::new(),
            &mut paths,
            false,
        );
        for field in event.optional {
            assert!(paths.insert(format!("/{}", wire_name(&field))));
        }
        cases.extend(paths.into_iter().map(|path| (event.name.clone(), path)));
    }
    cases
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
