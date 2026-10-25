//! The flock desktop shell: a Tauri v2 window around the unchanged
//! `web/client` bundle (ADR-0038 §6, #1601). See `desktop/README.md`.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
#[cfg(test)]
mod hardening;

use tauri::{utils::config::Csp, WebviewUrl, WebviewWindowBuilder};

fn main() {
    let server = config::load();
    if let Err(reason) = &server {
        eprintln!("flock-desktop: {reason}");
    }
    let mut context = tauri::generate_context!();
    // The asset protocol reads the policy at runtime, so it can name the configured server.
    context.config_mut().app.security.csp = Some(Csp::Policy(config::csp(server.as_ref().ok())));
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
        .run(context)
        .expect("the Tauri runtime failed");
}
