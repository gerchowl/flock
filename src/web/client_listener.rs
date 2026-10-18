//! Real structured-client listener ownership (ADR-0036 §5/§5a).
use super::listener_foundation::accept_next;
use super::wire::timing::{Clock, SystemClock};
use super::{client_exchange, gui, wire::Core};
use crate::api::client_listener::{Health, Status};
use crate::client_open::Codes;
use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Extension, Router,
};
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use std::future::Future;
use std::{
    io,
    net::{Ipv4Addr, SocketAddr, TcpListener},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::Semaphore,
    task::{JoinHandle, JoinSet},
};
/// Concurrent HTTP connections before upgrade; upgraded sockets have the core's cap.
const HTTP_CONNECTIONS: usize = 32;
/// Absolute HTTP lifetime, including headers, body, writes and keepalive before upgrade.
const HTTP_ADMISSION_DEADLINE: Duration = Duration::from_secs(5);
/// Maximum time for the startup public-funnel probe.
const FUNNEL_DEADLINE: Duration = Duration::from_secs(5);
/// Maximum status JSON bytes accepted from the public-funnel probe.
const FUNNEL_STATUS_BYTES: usize = 65_536;
/// Own every HTTP task; acceptance and its monotonic clock are injectable without test sleeps.
async fn serve<A, F>(
    mut accept: A,
    router: Router,
    clock: Arc<dyn Clock>,
    health: Health,
) -> io::Result<()>
where
    A: FnMut() -> F,
    F: Future<Output = io::Result<(tokio::net::TcpStream, SocketAddr)>>,
{
    let slots = Arc::new(Semaphore::new(HTTP_CONNECTIONS));
    let mut clients = JoinSet::new();
    loop {
        while clients.try_join_next().is_some() {}
        let (socket, peer) = accept_next(&mut accept, clock.as_ref(), &health).await?;
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            continue;
        };
        let service = router.clone().layer(Extension(ConnectInfo(peer)));
        clients.spawn(async move {
            let _permit = permit;
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder
                .half_close(true)
                .header_read_timeout(HTTP_ADMISSION_DEADLINE)
                .timer(TokioTimer::new());
            let _ = tokio::time::timeout(
                HTTP_ADMISSION_DEADLINE,
                builder
                    .serve_connection(TokioIo::new(socket), TowerToHyperService::new(service))
                    .with_upgrades(),
            )
            .await;
        });
    }
}
/// Prepared before handoff ready; only run activates acceptance, after commit.
pub(crate) struct Listener {
    pub(crate) socket: TcpListener,
    pub(crate) health: Health,
    core: Core,
    prepared: Option<(tokio::net::TcpListener, Router)>,
    task: Option<JoinHandle<io::Result<()>>>,
}
impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
impl Listener {
    /// Bind or adopt the exported socket; never rebind a live handoff port.
    pub(crate) async fn prepare(
        bind: Option<SocketAddr>,
        codes: Codes,
        data: PathBuf,
        inherited: Option<TcpListener>,
        health: Health,
    ) -> io::Result<Option<Self>> {
        let Some(bind) = bind else {
            return if inherited.is_none() {
                Ok(None)
            } else {
                Err(io::ErrorKind::InvalidInput.into())
            };
        };
        crate::platform::client_listener::require_loopback(bind)?;
        if bind.ip() != Ipv4Addr::LOCALHOST {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let socket = match inherited {
            Some(socket) => socket,
            None => TcpListener::bind(bind)?,
        };
        let actual = socket.local_addr()?;
        crate::platform::client_listener::require_loopback(actual)?;
        if actual.ip() != bind.ip() || (bind.port() != 0 && actual.port() != bind.port()) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        tokio::task::spawn_blocking(move || check_funnel(actual.port()))
            .await
            .map_err(io::Error::other)??;
        socket.set_nonblocking(true)?;
        let acceptor = tokio::net::TcpListener::from_std(socket.try_clone()?)?;
        let core = Core::new(data.clone()).await?;
        let csp = HeaderValue::from_str(&format!("default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self' ws://{actual}/v1/ws; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'")).map_err(io::Error::other)?;
        let router = core
            .router()
            .merge(client_exchange::router(codes, data, actual).map_err(io::Error::other)?)
            .merge(gui::router(actual)?)
            .layer(middleware::from_fn_with_state((actual, csp), guard));
        Ok(Some(Self {
            health,
            socket,
            core,
            prepared: Some((acceptor, router)),
            task: None,
        }))
    }
    /// Start only inside the owning server loop, after handoff commit if importing.
    pub(crate) fn activate(&mut self) {
        if let Some((socket, router)) = self.prepared.take() {
            self.health.publish(Status::Listening);
            let health = self.health.clone();
            self.task = Some(tokio::spawn(async move {
                serve(|| socket.accept(), router, Arc::new(SystemClock), health).await
            }));
        }
    }
    /// Cancel owned HTTP tasks, then cancel and require upgraded core lease release.
    pub(crate) async fn shutdown(mut self) -> io::Result<()> {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
        self.core.shutdown().await.map_err(io::Error::other)
    }
}
async fn guard(
    State((bind, csp)): State<(SocketAddr, HeaderValue)>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    let authority = bind.to_string();
    let headers = request.headers();
    if peer.ip() != Ipv4Addr::LOCALHOST
        || !gui::origin_allowed(headers, &authority)
        || (request.uri().path().starts_with("/v1/") && !headers.contains_key(header::ORIGIN))
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CONTENT_SECURITY_POLICY, csp);
    response
}
/// Inspect forwarding targets while tolerating unrelated configuration metadata.
#[derive(serde::Deserialize)]
struct FunnelStatus {
    #[serde(rename = "AllowFunnel", default)]
    enabled: std::collections::BTreeMap<String, bool>,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}
fn check_funnel(port: u16) -> io::Result<()> {
    let output = crate::process::TracedCommand::new("tailscale", "client-wire")
        .args(["serve", "status", "--json"])
        .output_traced_with_timeout_and_limit(FUNNEL_DEADLINE, FUNNEL_STATUS_BYTES)?;
    validate_funnel(output, port)
}
/// Validate successful probe output before any request can reach admission.
fn validate_funnel(output: std::process::Output, port: u16) -> io::Result<()> {
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "tailscale serve status failed: {}",
            output.status
        )));
    }
    let status: FunnelStatus = serde_json::from_slice(&output.stdout)
        .map_err(|_| io::Error::other("invalid tailscale status JSON"))?;
    if status
        .enabled
        .iter()
        .any(|(target, active)| *active && target_port(target) == Some(port))
        || targets_port(&serde_json::Value::Object(status.rest), port)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "tailscale serve/funnel may expose the client port",
        ));
    }
    Ok(())
}

/// Proxy strings may be a bare port, an authority, or an HTTP(S) URL (Tailscale serve schema).
fn target_port(target: &str) -> Option<u16> {
    let default = if target.to_ascii_lowercase().starts_with("https://") {
        443
    } else {
        80
    };
    target.parse().ok().or_else(|| {
        let authority = target.split_once("://").map_or(target, |(_, rest)| rest);
        format!("http://{authority}")
            .parse::<axum::http::Uri>()
            .ok()
            .map(|uri| uri.port_u16().unwrap_or(default))
    })
}
/// Traverse foreground and service configs as well as the node's background config.
fn targets_port(value: &serde_json::Value, port: u16) -> bool {
    match value {
        serde_json::Value::Object(fields) => fields.iter().any(|(key, value)| {
            (matches!(key.as_str(), "Proxy" | "TCPForward")
                && value
                    .as_str()
                    .and_then(target_port)
                    .is_none_or(|target| target == port))
                || (key == "TCP" && value.get(port.to_string()).is_some())
                || targets_port(value, port)
        }),
        serde_json::Value::Array(values) => values.iter().any(|value| targets_port(value, port)),
        _ => false,
    }
}

#[cfg(test)]
pub(crate) mod tests;
