//! GUI bundle hosting (ADR-0038 §6, ADR-0036 §5/§7, #1826).
//!
//! Slice #1828 merges this stateless router after installing its own state and
//! `/v1/*` routes. It owns the listener: bind only loopback, off by default, and
//! enforce its funnel/auth guards. This module never binds or starts a bridge.
//! `FLOCK_GUI_DIST` selects the prebuilt dist at compile time (default
//! `web/client/dist`); #1731 supplies the Nix `flock-client` output. No build or
//! dependency fetching happens here. Missing `index.html` returns a typed 503.
//! There is no SPA fallback: presentation navigation needs no server routes.

use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use rust_embed::RustEmbed;
use std::{io, net::SocketAddr};

use crate::api::error_code::ErrorCode;

include!(concat!(env!("OUT_DIR"), "/gui_assets.rs"));

/// Build a stateless GUI router for the actual numeric HTTP loopback authority.
///
/// Merge after the listener's state is installed. Reserved `/v1/*` paths always
/// fall through to 404 here, leaving transport routes to #1828. Origins must
/// match `http://{bind}` with HTTP's default port normalized; aliases, proxy
/// origins and opaque origins are refused.
/// Hashed Vite assets are immutable; every other file revalidates by ETag.
/// HTTP/1 Host is required. HTTP/2 `:authority` without Host receives 403,
/// so the mounting listener must serve this router over HTTP/1.
/// A non-loopback address is rejected before any route is constructed.
// #1828 mounts this independently delivered router.
#[allow(dead_code)]
pub(crate) fn router(bind: SocketAddr) -> io::Result<Router> {
    router_for::<GuiAssets>(bind)
}

fn router_for<A: RustEmbed + 'static>(bind: SocketAddr) -> io::Result<Router> {
    if !bind.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "GUI router requires a loopback listener",
        ));
    }
    Ok(Router::new()
        .fallback(get(serve::<A>))
        .with_state(bind.to_string()))
}

fn refusal(status: StatusCode, code: ErrorCode, message: &'static str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({
            "error": { "code": code.as_str(), "message": message }
        })),
    )
        .into_response()
}

fn origin_allowed(headers: &HeaderMap, authority: &str) -> bool {
    // Browsers omit the default HTTP port when serializing Host and Origin.
    let canonical = authority.strip_suffix(":80").unwrap_or(authority);
    if headers.get_all(header::HOST).iter().count() != 1
        || !headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .is_some_and(|host| host == authority || host == canonical)
    {
        return false;
    }
    if let Some(site) = headers.get("sec-fetch-site") {
        if !matches!(site.to_str(), Ok("none" | "same-origin")) {
            return false;
        }
    }
    if headers.get_all(header::ORIGIN).iter().count() > 1 {
        return false;
    }
    headers.get(header::ORIGIN).is_none_or(|origin| {
        origin.to_str().is_ok_and(|value| {
            value == format!("http://{authority}") || value == format!("http://{canonical}")
        })
    })
}

fn immutable(path: &str) -> bool {
    // Anchor Vite's default flat assets/[name]-[hash:8].[ext] pattern.
    let Some(name) = path.strip_prefix("assets/") else {
        return false;
    };
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return false;
    };
    let Some(separator) = stem.len().checked_sub(9) else {
        return false;
    };
    let prefix = &stem.as_bytes()[..separator];
    let suffix = &stem.as_bytes()[separator..];
    let name_char = |b: &u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-');
    !prefix.is_empty()
        && prefix.iter().all(name_char)
        && suffix[0] == b'-'
        && suffix[1..].iter().all(name_char)
        && !extension.is_empty()
        && extension.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Restrict the document to bundled resources and its listener's transport.
///
/// CSP3's `'self'` includes same-host/port WebSockets. An explicit IPv4 source
/// also supports browsers with older matching rules. IPv6 literals are invalid
/// CSP host sources, so IPv6 uses `'self'` and fails closed on older browsers.
fn document_policy(authority: &str) -> axum::http::HeaderValue {
    let websocket = if authority.starts_with('[') {
        String::new()
    } else {
        format!(" ws://{authority}/v1/ws")
    };
    format!(
        "default-src 'self'; script-src 'self'; style-src 'self'; \
         connect-src 'self'{websocket}; object-src 'none'; base-uri 'none'; \
         frame-ancestors 'none'; form-action 'none'"
    )
    .parse()
    .expect("numeric listener authority produces a valid CSP header")
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

async fn serve<A: RustEmbed>(
    State(authority): State<String>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    if !origin_allowed(&headers, &authority) {
        return refusal(
            StatusCode::FORBIDDEN,
            ErrorCode::OriginMismatch,
            "cross-origin GUI request refused",
        );
    }
    let raw = uri.path().strip_prefix('/').unwrap_or_default();
    if raw == "v1" || raw.starts_with("v1/") {
        return refusal(
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "GUI route not found",
        );
    }
    if A::get("index.html").is_none() {
        return refusal(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::ServerUnavailable,
            "GUI bundle missing: build with FLOCK_GUI_DIST pointing to the prebuilt flock-client dist (#1731)");
    }
    let path = if raw.is_empty() { "index.html" } else { raw };
    let Some(file) = A::get(path) else {
        return refusal(
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "GUI asset not found",
        );
    };
    let hash: String = file
        .metadata
        .sha256_hash()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let etag = format!("\"{hash}\"");
    let unchanged = headers.get_all(header::IF_NONE_MATCH).iter().any(|value| {
        value.to_str().is_ok_and(|list| {
            list.split(',').any(|tag| {
                let tag = tag.trim();
                tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == etag
            })
        })
    });
    let cache = if immutable(path) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut response = if unchanged {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        file.data.into_owned().into_response()
    };
    let output = response.headers_mut();
    if path == "index.html" {
        output.insert(header::CONTENT_SECURITY_POLICY, document_policy(&authority));
    }
    output.insert(
        header::CACHE_CONTROL,
        cache.parse().expect("constant cache header"),
    );
    output.insert(
        header::ETAG,
        etag.parse().expect("hex digest is a valid ETag"),
    );
    output.insert(
        header::CONTENT_TYPE,
        content_type(path).parse().expect("constant content type"),
    );
    output.insert(
        header::VARY,
        "Origin, Sec-Fetch-Site"
            .parse()
            .expect("constant Vary header"),
    );
    output.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        "nosniff".parse().expect("constant nosniff header"),
    );
    response
}

#[cfg(test)]
mod tests;
