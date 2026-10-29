//! The flock desktop shell: a Tauri v2 window around the unchanged
//! `web/client` bundle (ADR-0038 §6, #1601). See `desktop/README.md`.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bridge;
mod config;
#[cfg(test)]
mod hardening;
mod mint;

use std::sync::Arc;

use tauri::{
    ipc::Channel, webview::PageLoadEvent, Manager, State, Webview, WebviewUrl,
    WebviewWindowBuilder, WindowEvent,
};

fn main() {
    let context = tauri::generate_context!();
    let dev_url = tauri::is_dev()
        .then(|| context.config().build.dev_url.clone())
        .flatten();
    let ws = mint::ws_socket_path(&mint::socket_path(|key| std::env::var(key).ok()));
    let bridge = bridge::Bridge::new(
        ws,
        Arc::new(mint::mint),
        Arc::new(bridge::suspend_inclusive_ms),
    );
    tauri::Builder::default()
        .manage(bridge)
        .setup(move |app| {
            WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
                .title("flock")
                .inner_size(1280.0, 800.0)
                .initialization_script(config::init_script())
                .on_navigation(move |url| config::navigation_allowed(url, dev_url.as_ref()))
                // A reload or navigation starts a new page: the old one's
                // sessions end (`Bridge::webview_gone`), or each would live on.
                .on_page_load(|webview, load| {
                    if load.event() == PageLoadEvent::Started {
                        webview
                            .state::<bridge::Bridge>()
                            .webview_gone(webview.label());
                    }
                })
                .build()?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::Destroyed = event {
                window
                    .state::<bridge::Bridge>()
                    .webview_gone(window.label());
            }
        })
        .invoke_handler(tauri::generate_handler![
            bridge_open,
            bridge_send,
            bridge_close
        ])
        .run(context)
        .expect("the Tauri runtime failed");
}

// The webview's three commands: the bridge's open, send and close (ADR-0038 §6,
// "Never in the webview"). None of them returns or reports the bearer.

/// Opens a bridged session for this webview; its frames and close arrive
/// on `events`. A send that fails means the webview is gone.
#[tauri::command]
async fn bridge_open(
    bridge: State<'_, bridge::Bridge>,
    webview: Webview,
    renew: bool,
    events: Channel<bridge::Event>,
) -> Result<bridge::Opened, String> {
    let sink = Arc::new(move |event| events.send(event).is_ok());
    // The refusal, never the bearer: `open` has cut it out of every message.
    let opened = bridge.open(webview.label(), renew, sink).await;
    opened.inspect_err(|reason| eprintln!("flock-desktop: {reason}"))
}

/// A text frame for the session. Synchronous, so frames keep their order.
#[tauri::command]
fn bridge_send(bridge: State<'_, bridge::Bridge>, id: u64, text: String) -> Result<(), String> {
    bridge.send(id, text)
}

/// Closes the session from the webview's side.
#[tauri::command]
fn bridge_close(
    bridge: State<'_, bridge::Bridge>,
    id: u64,
    code: Option<u16>,
    reason: Option<String>,
) -> Result<(), String> {
    bridge.close(id, code, reason)
}
