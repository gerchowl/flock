//! Real structured-client listener ownership (ADR-0036 §5/§5a).
use super::{client_exchange, gui, wire::Core};
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
/// Absolute ceiling for receiving complete HTTP headers, before hello begins.
const HTTP_HEADER_DEADLINE: Duration = Duration::from_secs(5);
/// Maximum time for the startup public-funnel probe.
const FUNNEL_DEADLINE: Duration = Duration::from_secs(5);
/// Maximum status JSON bytes accepted from the public-funnel probe.
const FUNNEL_STATUS_BYTES: usize = 65_536;
/// Own every HTTP task; upgraded sockets transfer ownership to the core.
async fn serve(socket: tokio::net::TcpListener, router: Router) -> io::Result<()> {
    let slots = Arc::new(Semaphore::new(HTTP_CONNECTIONS));
    let mut clients = JoinSet::new();
    loop {
        while clients.try_join_next().is_some() {}
        let (socket, peer) = socket.accept().await?;
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            continue;
        };
        let service = router.clone().layer(Extension(ConnectInfo(peer)));
        clients.spawn(async move {
            let _permit = permit;
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder
                .half_close(true)
                .header_read_timeout(HTTP_HEADER_DEADLINE)
                .timer(TokioTimer::new());
            let _ = builder
                .serve_connection(TokioIo::new(socket), TowerToHyperService::new(service))
                .with_upgrades()
                .await;
        });
    }
}
/// Prepared before handoff ready; only run activates acceptance, after commit.
pub(crate) struct Listener {
    pub(crate) socket: TcpListener,
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
    ) -> io::Result<Option<Self>> {
        let Some(bind) = bind else {
            return if inherited.is_none() {
                Ok(None)
            } else {
                Err(io::ErrorKind::InvalidInput.into())
            };
        };
        if bind.ip() != Ipv4Addr::LOCALHOST {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        tokio::task::spawn_blocking(check_funnel)
            .await
            .map_err(io::Error::other)??;
        let socket = match inherited {
            Some(socket) => socket,
            None => TcpListener::bind(bind)?,
        };
        let actual = socket.local_addr()?;
        if actual.ip() != bind.ip() || (bind.port() != 0 && actual.port() != bind.port()) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
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
            socket,
            core,
            prepared: Some((acceptor, router)),
            task: None,
        }))
    }
    /// Start only inside the owning server loop, after handoff commit if importing.
    pub(crate) fn activate(&mut self) {
        if let Some((socket, router)) = self.prepared.take() {
            self.task = Some(tokio::spawn(serve(socket, router)));
        }
    }
    /// True only if the owning accept task unexpectedly ended.
    pub(crate) fn failed(&self) -> bool {
        self.task.as_ref().is_some_and(JoinHandle::is_finished)
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
    let canonical = authority.strip_suffix(":80").unwrap_or(&authority);
    let headers = request.headers();
    if peer.ip() != Ipv4Addr::LOCALHOST
        || headers.get_all(header::HOST).iter().count() != 1
        || headers
            .get(header::HOST)
            .is_none_or(|v| v != authority.as_str() && v != canonical)
        || headers.get_all(header::ORIGIN).iter().count() > 1
        || headers.get(header::ORIGIN).is_some_and(|v| {
            v != format!("http://{authority}").as_str()
                && v != format!("http://{canonical}").as_str()
        })
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
/// Only the declared funnel map grants a known inactive result; additions are tolerated.
#[derive(serde::Deserialize)]
struct FunnelStatus {
    #[serde(rename = "AllowFunnel", default)]
    enabled: std::collections::BTreeMap<String, bool>,
}
fn check_funnel() -> io::Result<()> {
    let output = crate::process::TracedCommand::new("tailscale", "client-wire")
        .args(["serve", "status", "--json"])
        .output_traced_with_timeout_and_limit(FUNNEL_DEADLINE, FUNNEL_STATUS_BYTES)?;
    let status: FunnelStatus = serde_json::from_slice(&output.stdout).map_err(io::Error::other)?;
    if !output.status.success() || status.enabled.values().any(|active| *active) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "client listener refused: public-funnel status is unsafe or unavailable",
        ));
    }
    Ok(())
}
