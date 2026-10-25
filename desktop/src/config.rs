//! The shell's configuration: which `flk` server it connects to, and what it
//! tells the bundle and the webview about it (ADR-0038 §6).
//!
//! The server URL comes from `FLOCK_DESKTOP_SERVER_URL`, else `server_url` in
//! `config.toml` under the platform config dir, else [`DEFAULT_SERVER_URL`].
//! A malformed value is an error the bundle shows; it never falls back.

use std::{io, path::Path};

use serde::Deserialize;
use serde_json::json;
use url::{Host, Url};

/// `[client] bind` as the flk docs suggest it; the flk listener has no default port.
const DEFAULT_SERVER_URL: &str = "ws://127.0.0.1:7682";
const SERVER_URL_ENV: &str = "FLOCK_DESKTOP_SERVER_URL";
/// `web/client/src/config.ts`: the global the bundle reads before it renders.
const SHELL_CONFIG_GLOBAL: &str = "__FLOCK_SHELL_CONFIG__";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    server_url: Option<String>,
}

/// Reads the environment and `<config dir>/dev.flock.desktop/config.toml`
/// (the directory Tauri names after the identifier), then [`resolve`]s them.
pub fn load() -> Result<Url, String> {
    let env = match std::env::var(SERVER_URL_ENV) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => return Err(format!("{SERVER_URL_ENV}: {error}")),
    };
    let path = dirs::config_dir().map(|dir| dir.join("dev.flock.desktop/config.toml"));
    let text = match path.as_deref().map(std::fs::read_to_string) {
        Some(Ok(text)) => Some(text),
        Some(Err(error)) if error.kind() == io::ErrorKind::NotFound => None,
        Some(Err(error)) => return Err(format!("{}: {error}", path.unwrap_or_default().display())),
        None => None,
    };
    resolve(env.as_deref(), path.as_deref().zip(text.as_deref()))
}

/// The environment wins over the file, and the file over the default. A
/// malformed file is an error even when the environment overrides it.
pub fn resolve(env: Option<&str>, file: Option<(&Path, &str)>) -> Result<Url, String> {
    let parse = |(path, text): (&Path, &str)| {
        toml::from_str::<FileConfig>(text)
            .map_err(|e| format!("{}: {}", path.display(), e.message()))
    };
    let from_file = file
        .map(parse)
        .transpose()?
        .and_then(|file| file.server_url);
    let (raw, source) = match (env, from_file) {
        (Some(value), _) => (value.to_owned(), SERVER_URL_ENV),
        (None, Some(value)) => (value, "server_url"),
        (None, None) => (DEFAULT_SERVER_URL.to_owned(), "the default server URL"),
    };
    server_url(&raw).map_err(|error| format!("{source}: {error}"))
}

/// An origin on a loopback address. Remote servers take the pasted-code path (#2293).
fn server_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|e| format!("{raw:?} is not an absolute URL ({e})"))?;
    if !matches!(url.scheme(), "ws" | "wss" | "http" | "https") {
        return Err(format!("{raw:?} must use ws:, wss:, http: or https:"));
    }
    let loopback = match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    if !loopback {
        return Err(format!(
            "{raw:?} must be on a loopback address such as 127.0.0.1"
        ));
    }
    // An origin serialises to itself plus "/": no credentials, path, query or fragment.
    if url.as_str() != format!("{}/", url.origin().ascii_serialization()) {
        return Err(format!(
            "{raw:?} must be an origin, with no path or credentials"
        ));
    }
    Ok(url)
}

/// The script that runs before the bundle: the wire config, or the contract's
/// shell-reported error, which the client shows in its ConfigError state.
pub fn init_script(server: &Result<Url, String>) -> String {
    let config = match server {
        Ok(url) => json!({ "transport": "wire", "serverUrl": url.origin().ascii_serialization() }),
        Err(reason) => json!({
            "transport": "error",
            "message": format!("Invalid desktop configuration: {reason}"),
        }),
    };
    format!("window.{SHELL_CONFIG_GLOBAL} = Object.freeze({config});")
}

/// `default-src 'self'`, the configured server's `ws:` and `http:` forms and
/// nothing else for `connect-src`, and `blob:` artifact frames (ADR-0038 §6, §7).
pub fn csp(server: Option<&Url>) -> String {
    let connect = server.map_or("'none'".to_owned(), |url| {
        let origin = url.origin().ascii_serialization();
        match url.scheme() {
            "ws" | "wss" => format!("{origin} {}", origin.replacen("ws", "http", 1)),
            _ => format!("{} {origin}", origin.replacen("http", "ws", 1)),
        }
    });
    format!("default-src 'self'; connect-src {connect}; frame-src blob: 'self'")
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

    fn resolved(env: Option<&str>, file: Option<&str>) -> Result<String, String> {
        resolve(env, file.map(|text| (Path::new("c.toml"), text))).map(String::from)
    }

    #[test]
    fn env_wins_over_file_and_file_over_default() {
        let file = Some("server_url = \"ws://127.0.0.1:9000\"");
        assert_eq!(
            resolved(Some("http://127.0.0.1:9100"), file).unwrap(),
            "http://127.0.0.1:9100/"
        );
        assert_eq!(resolved(None, file).unwrap(), "ws://127.0.0.1:9000/");
        assert_eq!(resolved(None, None).unwrap(), "ws://127.0.0.1:7682/");
        assert_eq!(
            resolved(Some("wss://[::1]:7682"), None).unwrap(),
            "wss://[::1]:7682/"
        );
    }

    #[test]
    fn malformed_config_is_an_error_never_the_default() {
        let env = Some("ws://127.0.0.1:1");
        assert!(resolved(env, Some("server_url = "))
            .unwrap_err()
            .starts_with("c.toml: "));
        assert!(resolved(None, Some("serverUrl = \"ws://127.0.0.1:1\"")).is_err());
        let refused = [
            "127.0.0.1:7682",
            "ftp://127.0.0.1",
            "ws://example.com:7682",
            "ws://localhost:7682",
        ];
        for raw in refused
            .into_iter()
            .chain(["ws://127.0.0.1:7682/v1/ws", "ws://u:p@127.0.0.1:1"])
        {
            assert!(resolved(Some(raw), None).is_err(), "{raw} was accepted");
        }
    }

    #[test]
    fn init_script_carries_the_origin_or_the_reason() {
        let ok = init_script(&Ok(url("ws://127.0.0.1:7682")));
        let wire = r#"{"serverUrl":"ws://127.0.0.1:7682","transport":"wire"}"#;
        assert_eq!(
            ok,
            format!("window.__FLOCK_SHELL_CONFIG__ = Object.freeze({wire});")
        );
        let err = init_script(&Err("c.toml: \"</script>\" is bad".into()));
        let reason = r#"{"message":"Invalid desktop configuration: c.toml: \"</script>\" is bad","transport":"error"}"#;
        assert_eq!(
            err,
            format!("window.__FLOCK_SHELL_CONFIG__ = Object.freeze({reason});")
        );
    }

    #[test]
    fn a_malformed_config_file_is_the_shell_reported_error() {
        let reason = resolved(None, Some("server_url = ")).unwrap_err();
        let script = init_script(&Err(reason.clone()));
        let config = script
            .strip_prefix("window.__FLOCK_SHELL_CONFIG__ = Object.freeze(")
            .and_then(|rest| rest.strip_suffix(");"))
            .unwrap();
        let config: serde_json::Value = serde_json::from_str(config).unwrap();
        let message = format!("Invalid desktop configuration: {reason}");
        assert_eq!(config, json!({ "transport": "error", "message": message }));
        assert!(reason.starts_with("c.toml: "), "{reason}");
    }

    #[test]
    fn csp_admits_the_configured_server_only() {
        let policy = |connect: &str| {
            format!("default-src 'self'; connect-src {connect}; frame-src blob: 'self'")
        };
        let ws = policy("ws://127.0.0.1:7682 http://127.0.0.1:7682");
        assert_eq!(csp(Some(&url("ws://127.0.0.1:7682"))), ws);
        assert_eq!(csp(Some(&url("http://127.0.0.1:7682"))), ws);
        assert_eq!(
            csp(Some(&url("https://[::1]"))),
            policy("wss://[::1] https://[::1]")
        );
        assert_eq!(csp(None), policy("'none'"));
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
