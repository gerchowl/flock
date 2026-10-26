//! Checked assertions over the shell's hardening config (ADR-0011 §3,
//! ADR-0038 §6). Each fails when its rule is removed from the checked-in files.

use serde_json::{json, Value};

fn json_file(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

#[test]
fn capabilities_grant_no_shell_fs_http_or_dialog() {
    let conf = json_file(include_str!("../tauri.conf.json"));
    assert_eq!(conf["app"]["security"]["capabilities"], json!(["main"]));
    assert_eq!(conf["app"]["withGlobalTauri"], false);
    // The bundle calls no command yet; PR 2 adds the token command, and nothing else.
    let main = json_file(include_str!("../capabilities/main.json"));
    assert_eq!(
        (&main["windows"], &main["permissions"]),
        (&json!(["main"]), &json!([]))
    );
    assert!(
        main.get("remote").is_none(),
        "no remote origin may reach the IPC"
    );
    let files = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/capabilities"));
    assert_eq!(files.unwrap().count(), 1, "an unreviewed capability file");
}

#[test]
fn csp_names_the_configured_server_only() {
    let security = &json_file(include_str!("../tauri.conf.json"))["app"]["security"];
    assert_eq!(security["csp"], crate::config::csp(None).as_str());
    assert!(security.get("devCsp").is_none());
    assert!(security
        .get("dangerousDisableAssetCspModification")
        .is_none());
    // The context main runs with carries the policy for the configured server.
    let csp = |server| {
        crate::context(&server)
            .config()
            .app
            .security
            .csp
            .as_ref()
            .map(ToString::to_string)
    };
    let server = url::Url::parse("ws://127.0.0.1:7682").unwrap();
    let connect = "connect-src ws://127.0.0.1:7682 http://127.0.0.1:7682";
    assert_eq!(
        csp(Ok(server)),
        Some(format!(
            "default-src 'self'; {connect}; frame-src blob: 'self'"
        ))
    );
    assert_eq!(csp(Err("bad".into())), Some(crate::config::csp(None)));
}

/// `main.rs` without its comments, so a commented-out rule does not count.
fn main_code() -> String {
    let source = include_str!("main.rs");
    assert!(!source.contains("/*"), "strip block comments too");
    source
        .lines()
        .map(|line| line.split("//").next().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn navigation_is_locked_to_the_bundle() {
    let lock = ".on_navigation(move |url| config::navigation_allowed(url, dev_url.as_ref()))";
    assert!(main_code().contains(lock));
}

#[test]
fn devtools_are_off_in_release() {
    let manifest: toml::Table = toml::from_str(include_str!("../Cargo.toml")).unwrap();
    let features = manifest["dependencies"]["tauri"]["features"]
        .as_array()
        .unwrap();
    assert!(
        !features.contains(&"devtools".into()),
        "devtools opens the inspector in release"
    );
}

#[test]
fn macos_release_runs_hardened_without_get_task_allow() {
    let macos = &json_file(include_str!("../tauri.macos.conf.json"))["bundle"]["macOS"];
    assert_eq!(
        (&macos["hardenedRuntime"], &macos["entitlements"]),
        (&json!(true), &json!("entitlements.plist"))
    );
    let entitlements: plist::Dictionary =
        plist::from_bytes(include_bytes!("../entitlements.plist")).unwrap();
    assert!(!entitlements.contains_key("com.apple.security.get-task-allow"));
}
