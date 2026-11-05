//! What the shell tells the bundle and the webview (ADR-0038 §6): the bridge
//! configuration and where the webview may navigate. On the same machine the
//! shell carries `/v1/ws` itself (`bridge.rs`), so the webview names no server
//! and opens no connection; the CSP's `connect-src 'none'` stays as checked in.

use url::Url;

/// `web/client/src/config.ts`: the global the bundle reads before it renders.
const SHELL_CONFIG_GLOBAL: &str = "__FLOCK_SHELL_CONFIG__";
/// The contract's `openBridge` over the three bridge commands.
const OPEN_BRIDGE: &str = include_str!("open-bridge.js");

/// The script that runs before the bundle: the contract's bridge transport.
pub fn init_script() -> String {
    format!(
        "window.{SHELL_CONFIG_GLOBAL} = Object.freeze({{ transport: \"bridge\", openBridge: {} }});",
        OPEN_BRIDGE.trim_end()
    )
}

/// The bundle's own origin only: `tauri://localhost` on macOS and Linux,
/// `http://tauri.localhost` on Windows, and the dev server in `tauri dev`.
/// Elsewhere `tauri.localhost` is a loopback name any local process may serve.
/// macOS asks for subframes too, so the bundle's own `blob:` artifact frames pass (§7).
pub fn navigation_allowed(url: &Url, dev_url: Option<&Url>) -> bool {
    match url.scheme() {
        "blob" => Url::parse(url.path())
            .is_ok_and(|inner| inner.scheme() != "blob" && navigation_allowed(&inner, dev_url)),
        #[cfg(not(windows))]
        "tauri" => url.host_str() == Some("localhost") && url.port().is_none(),
        #[cfg(windows)]
        "http" if url.host_str() == Some("tauri.localhost") && url.port().is_none() => true,
        "http" | "https" => dev_url.is_some_and(|dev| dev.origin() == url.origin()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    #[test]
    fn init_script_installs_the_bridge_and_no_server_or_token() {
        let script = init_script();
        let prefix =
            "window.__FLOCK_SHELL_CONFIG__ = Object.freeze({ transport: \"bridge\", openBridge: ";
        assert!(script.starts_with(prefix), "{script}");
        assert!(script.ends_with("\n} });"), "{script}");
        for command in ["\"bridge_open\"", "\"bridge_send\"", "\"bridge_close\""] {
            assert!(script.contains(command), "{command}");
        }
        for gone in ["serverUrl", "tokenProvider", "mint_token", "token:"] {
            assert!(!script.contains(gone), "{gone}");
        }
    }

    #[test]
    fn navigation_stays_in_the_bundle() {
        let bundle = ["tauri://localhost/index.html", "blob:tauri://localhost/1f"];
        for raw in bundle {
            assert_eq!(navigation_allowed(&url(raw), None), !cfg!(windows), "{raw}");
        }
        // Only Windows serves the bundle from tauri.localhost; elsewhere it is loopback.
        for raw in ["http://tauri.localhost/", "blob:http://tauri.localhost/1f"] {
            assert_eq!(navigation_allowed(&url(raw), None), cfg!(windows), "{raw}");
        }
        let dev = url("http://127.0.0.1:5173");
        assert!(navigation_allowed(
            &url("http://127.0.0.1:5173/"),
            Some(&dev)
        ));
        let refused = [
            "http://127.0.0.1:5173/",
            "https://tauri.localhost/",
            "tauri://evil/",
        ];
        let more = [
            "http://tauri.localhost:8080/",
            "file:///etc/passwd",
            "data:text/html,x",
        ];
        for raw in refused
            .into_iter()
            .chain(more)
            .chain(["blob:https://example.com/1f"])
        {
            assert!(!navigation_allowed(&url(raw), None), "{raw}");
        }
    }
}
