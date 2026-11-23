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

// Keep sealed declarations/impls verbatim. Storage submodules need product
// dependencies, so retain only their namespaces in these standalone probes.
fn token_context(auth: &str) -> String {
    let tree = syn::parse_file(auth).expect("parse actual auth module");
    let mut context = auth.to_owned();
    let modules = tree.items.into_iter().filter_map(|item| match item {
        syn::Item::Mod(module) if module.content.is_none() => Some(module.ident),
        _ => None,
    });
    for module in modules {
        let declaration = format!("mod {module};");
        assert_eq!(
            auth.matches(&declaration).count(),
            1,
            "auth submodule boundary"
        );
        context = context.replace(&declaration, &format!("mod {module} {{}}"));
    }
    format!("mod auth {{\n{context}\n}}")
}

#[test]
fn permission_token_compile_fail() {
    let context = token_context(include_str!("../src/structured/permissions/auth.rs"));
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

// Stub only dependency types; the witness fields and privacy come from production.
fn local_operator_context() -> String {
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
    format!(
        "mod platform {{ pub mod peer {{ pub struct Credentials; }} }} \
         mod app {{ pub struct App; mod api {{ use super::App;\n{declaration}\n}} \
         pub(crate) use api::LocalOperator; }}"
    )
}

#[test]
fn permission_local_operator_compile_fail() {
    let context = local_operator_context();
    check_probes(
        &context,
        &[
            ("let _: Option<app::LocalOperator<'static>> = None;", None),
            (
                "let mut app = app::App; let _ = app::LocalOperator { _app: &mut app, peer: platform::peer::Credentials };",
                Some("E0451"),
            ),
        ],
    );
}

/// Browser scopes, including operate, never mint the local operator witness.
#[test]
fn websocket_token_cannot_become_local_operator() {
    let auth = token_context(include_str!("../src/structured/permissions/auth.rs"));
    let operator = local_operator_context();
    let context = format!("{auth} {operator}");
    check_probes(&context, &[
        ("fn keep(token: auth::VerifiedToken<'static>) -> auth::VerifiedToken<'static> { token }", None),
        ("fn elevate(token: auth::VerifiedToken<'static>) -> app::LocalOperator<'static> { token.into() }", Some("E0277")),
    ]);
}

/// Grant capabilities are neither freely constructible nor local credentials.
#[test]
fn lifecycle_grant_compile_fail() {
    let source = include_str!("../src/web/wire/commands/admission.rs");
    let structure = source.find("pub(crate) struct GrantAdmission {").unwrap();
    let start = source[..structure].rfind("\n\n").unwrap() + 2;
    let end = source[structure..].find("\n}").unwrap() + structure + 2;
    let declaration = &source[start..end];
    syn::parse_str::<syn::ItemStruct>(declaration).expect("actual grant declaration");
    let issuer = source.find("async fn claim(").expect("claim issuer");
    let issuer_start = source[..issuer].rfind('\n').unwrap() + 1;
    let issuer_end = source[issuer_start..].find('{').unwrap() + issuer_start;
    let issuer = &source[issuer_start..issuer_end];
    let method = |source: &str, name: &str| {
        let start = source
            .find(&format!("pub(crate) fn {name}("))
            .expect("actual management entry point");
        let end = source[start..].find('{').unwrap() + start;
        let signature = &source[start..end];
        format!("{signature} {{ Err(()) }}")
    };
    let manage = method(
        include_str!("../src/structured/permissions/auth/gateway.rs"),
        "manage",
    );
    let acknowledge = method(
        include_str!("../src/structured/permissions/auth/gateway/clocks.rs"),
        "acknowledge_clock",
    );
    let context = format!("{} struct Ownership; type ErrorCode = (); \
        mod journal {{ pub struct Admission; pub mod grants {{ pub struct Claim; }} }} \
        mod admission {{ use crate::{{Ownership, ErrorCode, journal}}; type AgentLifecycleCommand = (); \
        {declaration} impl GrantAdmission {{ {issuer} {{ Err(()) }} }} }} struct Session; \
        mod gateway {{ use crate::app::LocalOperator; type E = (); type Change = (); type Listing = (); \
        pub struct Store; impl Store {{ {manage} {acknowledge} }} }}", local_operator_context());
    check_probes(&context, &[
        ("let _: Option<admission::GrantAdmission> = None;", None),
        ("let _ = admission::GrantAdmission::claim(&Ownership, 0, journal::grants::Claim);", Some("E0624")),
        ("fn allowed(s: &gateway::Store, a: &app::LocalOperator<'_>) { let _ = s.manage(a, None); let _ = s.acknowledge_clock(a); }", None),
        ("fn forbidden(s: &gateway::Store, g: &admission::GrantAdmission) { let _ = s.manage(g, None); }", Some("E0308")),
        ("fn forbidden(s: &gateway::Store, g: &admission::GrantAdmission) { let _ = s.acknowledge_clock(g); }", Some("E0308")),
        ("let _ = admission::GrantAdmission { home: String::new(), principal: String::new(), issuance: String::new(), ordinal: 1, destination: String::new(), target: String::new(), workspace: String::new(), payload_digest: [0;32], command: () };", Some("E0451")),
        ("fn duplicate(g: admission::GrantAdmission) { let _ = g.clone(); }", Some("E0599")),
        ("fn manage(_: &app::LocalOperator<'_>) {} fn forbidden(g: &admission::GrantAdmission) { manage(g); }", Some("E0308")),
        ("fn session(g: admission::GrantAdmission) -> Session { g.into() }", Some("E0277")),
        ("fn elevate(g: admission::GrantAdmission) -> app::LocalOperator<'static> { g.into() }", Some("E0277")),
    ]);
}
