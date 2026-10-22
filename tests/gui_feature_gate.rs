//! The GUI bundle belongs wholly to the optional web build, including its route.

// Reuse the shared isolation/timeout helpers without importing unrelated harnesses.
#[allow(dead_code)]
#[path = "support/environment.rs"]
mod environment;
// The collector exposes shared limits as well as the bounded output operation.
#[allow(dead_code)]
#[path = "support/command.rs"]
mod command;

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
            "non-web binary must exclude the configured fixture"
        );
    }
    let mut command = environment::Command::new(env!("CARGO_BIN_EXE_flk"));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("FLOCK_") {
            command.env_remove(name);
        }
    }
    command.args(["web", "--help"]);
    let output = command::output(command, None, command::CLI_LIMIT).unwrap();
    if cfg!(feature = "web") {
        assert!(output.status.success(), "web help must be available");
    } else {
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("built without the `web` feature"));
    }
}
