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
fn csp_is_self_only_until_the_runtime_names_the_server() {
    let security = &json_file(include_str!("../tauri.conf.json"))["app"]["security"];
    assert_eq!(security["csp"], crate::config::csp(None).as_str());
    assert!(security.get("devCsp").is_none());
    assert!(security
        .get("dangerousDisableAssetCspModification")
        .is_none());
    assert!(include_str!("main.rs").contains("csp = Some(Csp::Policy(config::csp("));
}

#[test]
fn navigation_is_locked_to_the_bundle() {
    let lock = ".on_navigation(move |url| config::navigation_allowed(url, dev_url.as_ref()))";
    assert!(include_str!("main.rs").contains(lock));
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
    let macos = &json_file(include_str!("../tauri.conf.json"))["bundle"]["macOS"];
    assert_eq!(
        (&macos["hardenedRuntime"], &macos["entitlements"]),
        (&json!(true), &json!("entitlements.plist"))
    );
    let entitlements: plist::Dictionary =
        plist::from_bytes(include_bytes!("../entitlements.plist")).unwrap();
    assert!(!entitlements.contains_key("com.apple.security.get-task-allow"));
}
