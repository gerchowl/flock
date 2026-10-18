//! Compile-fail guards for permission capabilities (ADR-0036 §5a/§10b).
//! Nextest runs this integration test even though the product is binary-only.
//! Rustdoc checks standalone probes against the actual sealed declarations,
//! with positive controls so unrelated compilation failures cannot pass silently.
mod support;

use std::{fmt::Write, fs, time::Duration};
use support::environment::Command;

fn check_doctests(context: &str, cases: &[(&str, Option<&str>)]) {
    let directory = support::socket_dir::SocketDir::reserve("permission-ui");
    let path = directory.path().join("probe.rs");
    let mut source = String::new();
    for (index, (body, error)) in cases.iter().enumerate() {
        let fence = error.map_or_else(|| "rust".to_owned(), |code| format!("compile_fail,{code}"));
        let doc = format!("```{fence}\n{context}\nfn main() {{ {body} }}\n```");
        writeln!(source, "#[doc = {doc:?}]\npub fn probe_{index}() {{}}").unwrap();
    }
    fs::write(&path, source).expect("write owned compile probes");
    let mut command = Command::new("rustdoc");
    command
        .args([
            "--test",
            "--edition=2021",
            "--color=never",
            "--test-args=--test-threads=1",
        ])
        .arg(&path)
        .current_dir(directory.path());
    for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("FLOCK_")) {
        command.env_remove(key);
    }
    let output = support::command::output(command, None, Duration::from_secs(30))
        .expect("bounded rustdoc compile-fail probes");
    assert!(
        output.status.success(),
        "permission capability probes failed: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn permission_token_compile_fail() {
    let auth = include_str!("../src/structured/permissions/auth.rs");
    // Keep every declaration/impl, excluding only the dependency-heavy token store.
    let store = "\npub(crate) mod tokens;";
    assert_eq!(
        auth.matches(store).count(),
        1,
        "token store module boundary"
    );
    let context = format!("mod auth {{\n{}\n}}", auth.replace(store, ""));
    check_doctests(
        &context,
        &[
            ("let _: Option<auth::VerifiedToken<'static>> = None;", None),
            (
                "let _ = auth::VerifiedToken { subject: \"fixture\", operate: true };",
                Some("E0451"),
            ),
            (
                "let _ = auth::VerifiedToken::new(\"fixture\", true);",
                Some("E0624"),
            ),
            (
                "let _: auth::VerifiedToken = \"fixture\".into();",
                Some("E0277"),
            ),
        ],
    );
}

#[test]
fn permission_local_operator_compile_fail() {
    let api = include_str!("../src/app/api.rs");
    let start = api
        .find("pub(crate) struct LocalOperator<'a> {")
        .expect("actual LocalOperator declaration");
    let length = api[start..]
        .find("\n}")
        .expect("LocalOperator declaration terminator")
        + 2;
    let declaration = &api[start..start + length];
    syn::parse_str::<syn::ItemStruct>(declaration).expect("complete witness struct");
    let context = format!(
        "mod app {{ pub struct App; mod api {{ use super::App;\n{declaration}\n}} \
         pub(crate) use api::LocalOperator; }}"
    );
    check_doctests(
        &context,
        &[
            ("let _: Option<app::LocalOperator<'static>> = None;", None),
            (
                "let mut app = app::App; let _ = app::LocalOperator { _app: &mut app };",
                Some("E0451"),
            ),
        ],
    );
}
