//! The flock desktop shell: a Tauri v2 window around the unchanged
//! `web/client` bundle (ADR-0038 §6, #1601). See `desktop/README.md`.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
#[cfg(test)]
mod hardening;
mod mint;

use tauri::{utils::config::Csp, WebviewUrl, WebviewWindowBuilder};
use url::Url;

fn main() {
    let server = config::load();
    if let Err(reason) = &server {
        eprintln!("flock-desktop: {reason}");
    }
    let context = context(&server);
    let dev_url = tauri::is_dev()
        .then(|| context.config().build.dev_url.clone())
        .flatten();
    let script = config::init_script(&server);
    tauri::Builder::default()
        .setup(move |app| {
            WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
                .title("flock")
                .inner_size(1280.0, 800.0)
                .initialization_script(&script)
                .on_navigation(move |url| config::navigation_allowed(url, dev_url.as_ref()))
                .build()?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![mint_token])
        .run(context)
        .expect("the Tauri runtime failed");
}

/// The generated context, with a CSP that names the configured server. The
/// asset protocol reads the policy at runtime, so it can follow the config.
fn context(server: &Result<Url, String>) -> tauri::Context<tauri::Wry> {
    let mut context = tauri::generate_context!();
    context.config_mut().app.security.csp = Some(Csp::Policy(config::csp(server.as_ref().ok())));
    context
}

/// The webview's one command: a fresh app-session token, minted by this
/// process off the main thread (ADR-0038 §6, "In the webview").
#[tauri::command]
async fn mint_token() -> Result<String, String> {
    let minted = tauri::async_runtime::spawn_blocking(mint::mint).await;
    let minted = minted.map_err(|error| format!("the mint task failed: {error}"))?;
    // The reason, never the token: a refusal is the only thing printed.
    minted
        .map(mint::Token::into_secret)
        .inspect_err(|reason| eprintln!("flock-desktop: {reason}"))
}
