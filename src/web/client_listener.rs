//! Real structured-client listener ownership (ADR-0036 §5/§5a).
use super::listener_foundation::accept_next;
use super::wire::timing::{Clock, SystemClock};
use super::{client_exchange, gui, wire::Core};
use crate::api::client_listener::{Health, Status};
use crate::client_open::Codes;
use crate::platform::client_listener::InvalidListener;
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
/// Independent refusal writers per listener; overload never stalls normal acceptance.
/// Once these are full, additional over-cap sockets close without a response.
const HTTP_REFUSALS: usize = 4;
/// Absolute HTTP lifetime, including headers, body, writes and keepalive before upgrade.
const HTTP_ADMISSION_DEADLINE: Duration = Duration::from_secs(5);
/// Maximum time for the startup public-funnel probe.
const FUNNEL_DEADLINE: Duration = Duration::from_secs(5);
/// Maximum status JSON bytes accepted from the public-funnel probe.
const FUNNEL_STATUS_BYTES: usize = 65_536;
/// Own every HTTP task; acceptance and its monotonic clock are injectable without test sleeps.
pub(super) async fn serve<S, P, A, F>(
    mut accept: A,
    router: Router,
    clock: Arc<dyn Clock>,
    health: Health,
) -> io::Result<()>
where
    A: FnMut() -> F,
    F: Future<Output = io::Result<(S, P)>>,
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    P: Clone + Send + Sync + 'static,
{
    let slots = Arc::new(Semaphore::new(HTTP_CONNECTIONS));
    let refusals = Arc::new(Semaphore::new(HTTP_REFUSALS));
    let mut clients = JoinSet::new();
    loop {
        while clients.try_join_next().is_some() {}
        let (socket, peer) = accept_next(&mut accept, clock.as_ref(), &health).await?;
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            if let Ok(permit) = refusals.clone().try_acquire_owned() {
                let clock = clock.clone();
                clients.spawn(async move {
                    let _permit = permit;
                    // Refuse without reading another header outside the incomplete-HTTP cap.
                    let response =
                        super::local_admission::refusal(super::local_admission::Refusal::Capacity);
                    if let Ok(body) = axum::body::to_bytes(response.into_body(), 512).await {
                        use tokio::io::AsyncWriteExt;
                        let mut socket = socket;
                        let header = format!("HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n", body.len());
                        let write = async {
                            socket.write_all(header.as_bytes()).await?;
                            socket.write_all(&body).await?;
                            socket.shutdown().await?;
                            // Drain after half-close so unread request bytes cannot reset the reply.
                            tokio::io::copy(&mut socket, &mut tokio::io::sink()).await
                        };
                        tokio::select! { biased;
                            _ = clock.until(clock.now() + HTTP_ADMISSION_DEADLINE) => (),
                            _ = write => (),
                        }
                    }
                });
            }
            continue;
        };
        let service = router.clone().layer(Extension(ConnectInfo(peer)));
        let clock = clock.clone();
        clients.spawn(async move {
            let _permit = permit;
            http(socket, service, clock).await;
        });
    }
}
async fn http<S>(socket: S, service: Router, clock: Arc<dyn Clock>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .half_close(true)
        .header_read_timeout(HTTP_ADMISSION_DEADLINE)
        .timer(TokioTimer::new());
    tokio::select! { biased;
        _ = clock.until(clock.now() + HTTP_ADMISSION_DEADLINE) => (),
        _ = builder.serve_connection(TokioIo::new(socket), TowerToHyperService::new(service)).with_upgrades() => (),
    }
}
/// Prepared before handoff ready; only run activates acceptance, after commit.
pub(crate) struct Listener {
    pub(crate) socket: Option<TcpListener>,
    local: super::local_listener::Listener,
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
        local: (PathBuf, Option<crate::ipc::SocketOwner>),
        health: Health,
        core: Core,
    ) -> io::Result<Option<Self>> {
        Self::prepare_with_funnel(
            bind,
            codes,
            data,
            inherited,
            local,
            (health, core),
            check_funnel,
        )
        .await
    }
    /// Use the same startup path with an injected, off-executor forwarding check.
    async fn prepare_with_funnel(
        bind: Option<SocketAddr>,
        codes: Codes,
        data: PathBuf,
        inherited: Option<TcpListener>,
        local: (PathBuf, Option<crate::ipc::SocketOwner>),
        state: (Health, Core),
        probe: impl FnOnce(u16) -> io::Result<()> + Send + 'static,
    ) -> io::Result<Option<Self>> {
        let (health, core) = state;
        let local =
            super::local_listener::Listener::prepare(local.0, local.1, core.local_router())?;
        let Some(bind) = bind else {
            if inherited.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    InvalidListener::Unconfigured,
                ));
            }
            return Ok(Some(Self {
                health,
                socket: None,
                local,
                core,
                prepared: None,
                task: None,
            }));
        };
        crate::platform::client_listener::require_loopback(bind)?;
        if bind.ip() != Ipv4Addr::LOCALHOST {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                InvalidListener::NonLoopback,
            ));
        }
        let socket = match inherited {
            Some(socket) => socket,
            None => TcpListener::bind(bind)?,
        };
        let actual = socket.local_addr()?;
        crate::platform::client_listener::require_loopback(actual)?;
        if actual.ip() != bind.ip() || (bind.port() != 0 && actual.port() != bind.port()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                InvalidListener::BindMismatch {
                    expected: bind,
                    actual,
                },
            ));
        }
        tokio::task::spawn_blocking(move || probe(actual.port()))
            .await
            .map_err(io::Error::other)??;
        socket.set_nonblocking(true)?;
        let acceptor = tokio::net::TcpListener::from_std(socket.try_clone()?)?;

        let router = core
            .router()
            .merge(client_exchange::router(codes, data, actual).map_err(io::Error::other)?)
            .merge(gui::router(actual)?)
            .layer(middleware::from_fn_with_state(actual, guard));
        Ok(Some(Self {
            health,
            socket: Some(socket),
            local,
            core,
            prepared: Some((acceptor, router)),
            task: None,
        }))
    }
    /// Transfer ownership without ever releasing the permanent local listener lock.
    pub(crate) fn local_owner(&self) -> crate::ipc::SocketOwner {
        self.local.owner()
    }
    /// Retain ownership while the importer binds its replacement pathname.
    pub(crate) fn unlink_local(&self) -> io::Result<()> {
        self.local.unlink()
    }
    /// Rollback rebinds under the same guard; the old inode cannot remove the new one.
    pub(crate) fn restore_local(&mut self, path: PathBuf) -> io::Result<()> {
        self.local = super::local_listener::Listener::prepare(
            path,
            Some(self.local.owner()),
            self.core.local_router(),
        )?;
        self.local.activate();
        Ok(())
    }
    /// Run history admission only on the application owner.
    pub(crate) fn service_local(
        &mut self,
        registry: &crate::structured::registry::Registry,
        workers: &mut crate::structured::workers::Workers,
        root: &std::path::Path,
        home: Option<&str>,
    ) {
        self.core.service_local(registry, workers, root, home);
    }
    /// Service authenticated commands on the application owner.
    pub(crate) fn service_commands(&mut self, app: &mut crate::app::App) {
        self.core.service_commands(app);
    }
    /// Start only inside the owning server loop, after handoff commit if importing.
    pub(crate) fn activate(&mut self) {
        self.local.activate();
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
        self.local.stop().await;
        self.core.shutdown().await.map_err(io::Error::other)
    }
}
async fn guard(
    State(bind): State<SocketAddr>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    let authority = bind.to_string();
    let headers = request.headers();
    let mut response = if peer.ip() != Ipv4Addr::LOCALHOST
        || !(gui::origin_allowed(headers, &authority)
            || (request.uri().path() == "/v1/ws" && gui::shell_origin_allowed(headers, &authority)))
        || (request.uri().path().starts_with("/v1/") && !headers.contains_key(header::ORIGIN))
    {
        StatusCode::FORBIDDEN.into_response()
    } else {
        next.run(request).await
    };
    // The GUI entry document carries its own policy (#1848); nothing else
    // this listener returns is a page that may load or embed anything.
    response
        .headers_mut()
        .entry(header::CONTENT_SECURITY_POLICY)
        .or_insert(HeaderValue::from_static(gui::RESOURCE_POLICY));
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
/// Absence is distinct from an installed but unreadable forwarding configuration.
enum FunnelProbe {
    NotInstalled,
    Status(FunnelStatus),
    Unavailable(String),
}
fn check_funnel(port: u16) -> io::Result<()> {
    check_funnel_with(
        port,
        &crate::platform::tailscale::Locations::current(),
        |cli| {
            crate::process::TracedCommand::new(cli, "client-wire")
                .args(["serve", "status", "--json"])
                .output_traced_with_timeout_and_limit(FUNNEL_DEADLINE, FUNNEL_STATUS_BYTES)
        },
    )
}
fn check_funnel_with(
    port: u16,
    paths: &crate::platform::tailscale::Locations,
    run: impl FnOnce(&std::path::Path) -> io::Result<std::process::Output>,
) -> io::Result<()> {
    let probe = match paths.discover() {
        Ok(None) => FunnelProbe::NotInstalled,
        Ok(Some(cli)) => match run(&cli).and_then(parse_funnel) {
            Ok(status) => FunnelProbe::Status(status),
            Err(error) => FunnelProbe::Unavailable(format!(
                "Tailscale CLI {} present but serve status unavailable: {error}",
                cli.display()
            )),
        },
        Err(error) => FunnelProbe::Unavailable(error.to_string()),
    };
    match probe {
        FunnelProbe::NotInstalled => {
            crate::logging::client_tailscale_not_installed(&paths.checked_paths());
            Ok(())
        }
        FunnelProbe::Status(status) => validate_status(status, port),
        FunnelProbe::Unavailable(reason) => Err(io::Error::other(reason)),
    }
}
/// Validate successful probe output before any request can reach admission.
#[cfg(test)]
fn validate_funnel(output: std::process::Output, port: u16) -> io::Result<()> {
    validate_status(parse_funnel(output)?, port)
}
fn parse_funnel(output: std::process::Output) -> io::Result<FunnelStatus> {
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "tailscale serve status failed: {}",
            output.status
        )));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|_| io::Error::other("invalid tailscale status JSON"))
}
fn validate_status(status: FunnelStatus, port: u16) -> io::Result<()> {
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

/// Preserve the event-loop failure when listener cleanup also fails.
pub(crate) fn finish_shutdown(primary: io::Result<()>, shutdown: io::Result<()>) -> io::Result<()> {
    if let Err(error) = shutdown {
        if primary.is_ok() {
            return Err(error);
        }
        crate::logging::client_listener_shutdown_failed(&error);
    }
    primary
}

#[cfg(test)]
pub(crate) mod tests;
