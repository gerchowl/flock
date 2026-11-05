//! Checked assertions over the shell's hardening config (ADR-0011 §3,
//! ADR-0038 §6). Each fails when its rule is removed from the checked-in files.

use serde_json::{json, Value};

fn json_file(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

#[test]
fn capabilities_grant_only_the_bridge() {
    let conf = json_file(include_str!("../tauri.conf.json"));
    assert_eq!(conf["app"]["security"]["capabilities"], json!(["main"]));
    assert_eq!(conf["app"]["withGlobalTauri"], false);
    // Exactly the bridge's open, send and close, and they are the only app
    // commands: no shell, fs, http or dialog, and none returns the bearer.
    let main = json_file(include_str!("../capabilities/main.json"));
    let bridge = json!([
        "allow-bridge-open",
        "allow-bridge-send",
        "allow-bridge-close"
    ]);
    assert_eq!(
        (&main["windows"], &main["permissions"]),
        (&json!(["main"]), &bridge)
    );
    let declared =
        r#"const BRIDGE_COMMANDS: [&str; 3] = ["bridge_open", "bridge_send", "bridge_close"];"#;
    assert!(include_str!("../build.rs").contains(declared));
    assert!(include_str!("../build.rs").contains(".commands(&BRIDGE_COMMANDS)"));
    let handler = "tauri::generate_handler![bridge_open,bridge_send,bridge_close]";
    let code: String = main_code().split_whitespace().collect();
    assert!(code.contains(handler));
    assert_eq!(main_code().matches("#[tauri::command]").count(), 3);
    assert!(
        main.get("remote").is_none(),
        "no remote origin may reach the IPC"
    );
    let files = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/capabilities"));
    assert_eq!(files.unwrap().count(), 1, "an unreviewed capability file");
}

#[test]
fn csp_connects_nowhere() {
    let security = &json_file(include_str!("../tauri.conf.json"))["app"]["security"];
    let policy = "default-src 'self'; connect-src 'none'; frame-src blob: 'self'";
    assert_eq!(security["csp"], policy);
    assert!(security.get("devCsp").is_none());
    assert!(security
        .get("dangerousDisableAssetCspModification")
        .is_none());
    // The context main runs with keeps the checked-in policy.
    assert!(!main_code().contains("security.csp"));
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
fn a_page_that_goes_away_ends_its_sessions() {
    let code: String = main_code().split_whitespace().collect();
    let reload = "ifload.event()==PageLoadEvent::Started{webview.state::<bridge::Bridge>().webview_gone(webview.label());}";
    let closed = "ifletWindowEvent::Destroyed=event{window.state::<bridge::Bridge>().webview_gone(window.label());}";
    assert!(code.contains(".on_page_load(") && code.contains(reload));
    assert!(code.contains(".on_window_event(") && code.contains(closed));
    assert!(code.contains("events.send(event).is_ok()"));
    assert!(code.contains("bridge.open(webview.label(),renew,sink)"));
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
