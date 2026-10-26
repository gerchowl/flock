fn main() {
    // Declaring the app's commands puts them under the ACL: each needs its
    // generated `allow-<command>` permission in a capability (ADR-0038 §6).
    let manifest = tauri_build::AppManifest::new().commands(&["mint_token"]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))
        .expect("tauri-build failed");
}
