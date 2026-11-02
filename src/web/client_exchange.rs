//! One-time browser handoff router mounted on the guarded structured listener.
use crate::{client_open::Codes, structured::permissions::auth::tokens::TokenStore};
use axum::{
    extract::{ConnectInfo, DefaultBodyLimit, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use std::{net::SocketAddr, path::PathBuf, time::Instant};
/// Maximum browser-code exchange body bytes, before JSON parsing.
const EXCHANGE_BODY_BYTES: usize = 1024;

/// Mount only on the configured IPv4 loopback listener; never trust forwarding headers.
/// The listener supplies the actual peer through `ConnectInfo<SocketAddr>`.
pub(crate) fn router(
    codes: Codes,
    data: PathBuf,
    bind: SocketAddr,
) -> Result<Router, &'static str> {
    let origin =
        crate::client_open::endpoint(Some(bind)).ok_or("client endpoint must be IPv4 loopback")?;
    Ok(Router::new()
        .route("/v1/client/exchange", post(exchange))
        .layer(DefaultBodyLimit::max(EXCHANGE_BODY_BYTES))
        .with_state((codes, data, origin.trim_end_matches('/').to_owned())))
}
async fn exchange(
    State((codes, data, origin)): State<(Codes, PathBuf, String)>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Result<axum::body::Bytes, axum::extract::rejection::BytesRejection>,
) -> Response {
    let authority = origin.strip_prefix("http://").unwrap_or_default();
    let canonical = authority.strip_suffix(":80").unwrap_or(authority);
    let forbidden = !peer.ip().is_loopback()
        || headers.get_all(header::HOST).iter().count() != 1
        || headers.get_all(header::ORIGIN).iter().count() != 1
        || !headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .is_some_and(|h| h == authority || h == canonical)
        || !headers
            .get(header::ORIGIN)
            .and_then(|h| h.to_str().ok())
            .is_some_and(|h| h == origin || h == format!("http://{canonical}"))
        || headers
            .get("sec-fetch-site")
            .is_some_and(|h| h != "same-origin");
    if forbidden {
        return StatusCode::FORBIDDEN.into_response();
    }
    let code = body
        .ok()
        .and_then(|body| serde_json::from_slice::<serde_json::Value>(&body).ok())
        .and_then(|v| v.get("code").and_then(|v| v.as_str()).map(str::to_owned))
        .unwrap_or_default();
    let token = codes.exchange(&code, Instant::now());
    let token = match token {
        Some(token) => super::store_io::shared()
            .run(move || {
                TokenStore::open(&data)?.authenticate(&token)?;
                Ok::<_, crate::structured::permissions::auth::tokens::TokenError>(token)
            })
            .await
            .or_else(|| {
                crate::logging::client_exchange_failed(None);
                None
            })
            .and_then(|result| {
                result
                    .map_err(|error| crate::logging::client_exchange_failed(Some(&error)))
                    .ok()
            }),
        None => None,
    };
    let response = match token {
        Some(token) => Json(serde_json::json!({"token": token})).into_response(),
        None => (StatusCode::UNAUTHORIZED, "client exchange refused").into_response(),
    };
    ([(header::CACHE_CONTROL, "no-store")], response).into_response()
}

#[cfg(test)]
mod tests;
