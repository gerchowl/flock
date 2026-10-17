//! The GUI bundle belongs wholly to the optional web build, including its route.

#[test]
fn gui_bundle_is_contained_by_the_web_feature_gate() {
    let entry = syn::parse_file(include_str!("../src/main.rs")).unwrap();
    let web = entry
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Mod(module) if module.ident == "web" => Some(module),
            _ => None,
        })
        .expect("web module declaration");
    assert!(
        web.attrs.iter().any(|attribute| {
            matches!(&attribute.meta, syn::Meta::List(meta)
            if meta.path.is_ident("cfg") && meta.tokens.to_string() == "feature = \"web\"")
        }),
        "the entire web module must be feature gated"
    );
    let module = syn::parse_file(include_str!("../src/web/mod.rs")).unwrap();
    assert!(
        module.items.iter().any(|item| matches!(item,
        syn::Item::Mod(module) if module.ident == "gui")),
        "GUI route must live inside gated web module"
    );
    #[cfg(not(feature = "web"))]
    {
        let bytes = std::fs::read(env!("CARGO_BIN_EXE_flk")).unwrap();
        let marker = b"<!doctype html><title>GUI bundle fixture</title>";
        assert!(
            !bytes.windows(marker.len()).any(|window| window == marker),
            "non-web binary must not contain even a configured fixture bundle"
        );
    }
}
