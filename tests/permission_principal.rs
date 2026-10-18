//! Compile-fail guards for permission capabilities (ADR-0036 §5a/§10b).
//! Nextest runs this integration test even though the product is binary-only.
//! Rustc checks standalone probes against the actual sealed declarations.
//! Positive controls must compile; forbidden probes must report their exact error
//! code, so an unrelated compilation failure cannot silently satisfy the guard.
mod support;

use std::{
    fs,
    time::{Duration, Instant},
};
use support::environment::Command;

fn check_probes(context: &str, cases: &[(&str, Option<&str>)]) {
    let manifest: toml::Value =
        toml::from_str(include_str!("../Cargo.toml")).expect("parse package edition");
    let edition = manifest["package"]["edition"]
        .as_str()
        .expect("explicit package edition for compile probes");
    let directory = support::socket_dir::SocketDir::reserve("permission-ui");
    let deadline = Instant::now() + Duration::from_secs(30);
    for (index, (body, error)) in cases.iter().enumerate() {
        let path = directory.path().join(format!("probe_{index}.rs"));
        fs::write(&path, format!("{context}\nfn probe() {{ {body} }}\n"))
            .expect("write owned compile probe");
        let mut command = Command::new("rustc");
        command
            .args([
                "--edition",
                edition,
                "--crate-type=lib",
                "--emit=metadata",
                "--color=never",
            ])
            .arg(&path)
            .arg("-o")
            .arg(directory.path().join(format!("probe_{index}.rmeta")))
            .current_dir(directory.path())
            .env_remove("RUSTC_BOOTSTRAP");
        for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("FLOCK_")) {
            command.env_remove(key);
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("permission compile probes exceeded their command deadline");
        let output = support::command::output(command, None, remaining).unwrap_or_else(|error| {
            panic!("permission compile guards require rustc on PATH and a completed compiler invocation: {error}")
        });
        let stderr = String::from_utf8_lossy(&output.stderr);
        if let Some(code) = error {
            assert!(
                !output.status.success(),
                "forbidden probe {index} compiled successfully: {body}"
            );
            let diagnostic = format!("error[{code}]");
            assert!(
                stderr.contains(&diagnostic),
                "probe {index} expected {diagnostic}: {body}\n{stderr}"
            );
        } else {
            assert!(
                output.status.success(),
                "positive control {index} did not compile: {body}\n{stderr}"
            );
        }
    }
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
    check_probes(
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
    check_probes(
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
