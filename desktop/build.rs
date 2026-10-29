/// The bridge's open, send and close (src/main.rs), and no other command.
const BRIDGE_COMMANDS: [&str; 3] = ["bridge_open", "bridge_send", "bridge_close"];

fn main() {
    // Declaring the app's commands puts them under the ACL: each needs its
    // generated `allow-<command>` permission in a capability (ADR-0038 §6).
    let manifest = tauri_build::AppManifest::new().commands(&BRIDGE_COMMANDS);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))
        .expect("tauri-build failed");
}
