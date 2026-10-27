//! Unix HTTP listener foundation for ADR-0036, not mounted by the server yet.
//! PR3b of #2500 will atomically activate owner admission and TCP app-session refusal.
// Staged implementation exercised by real sockets below, awaiting #2500 activation.
#![allow(dead_code)]
use super::wire::timing::SystemClock;
use crate::{
    ipc::{SocketFileIdentity, SocketOwner},
    platform::peer::Credentials,
};
use axum::Router;
use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    net::{UnixListener, UnixStream},
    task::JoinHandle,
};
/// Preserve per-instance API overrides, including multiple sockets in one directory.
pub(crate) fn socket_path(api: &Path) -> PathBuf {
    let mut stem = api
        .file_stem()
        .unwrap_or(std::ffi::OsStr::new("flock"))
        .to_os_string();
    stem.push("-ws.sock");
    api.with_file_name(stem)
}
/// Retain pathname ownership even after the accept task aborts or across handoff.
pub(super) struct Listener {
    path: PathBuf,
    identity: SocketFileIdentity,
    owner: SocketOwner,
    prepared: Option<(UnixListener, Router)>,
    task: Option<JoinHandle<io::Result<()>>>,
}
impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
        if let Err(error) = self.unlink() {
            crate::logging::api_socket_remove_failed(&self.path, &error.to_string());
        }
    }
}
impl Listener {
    /// Prepare without accepting; importers activate only after handoff commit.
    pub(super) fn prepare(
        path: PathBuf,
        inherited: Option<SocketOwner>,
        router: Router,
    ) -> io::Result<Self> {
        crate::platform::socket::check_listener_parent(&path)?;
        let owner = match inherited {
            Some(owner) => {
                owner.prepare(&path)?;
                owner
            }
            None => crate::ipc::prepare_socket_path(&path, |_| {
                "local WebSocket listener is busy".into()
            })?,
        };
        let socket = std::os::unix::net::UnixListener::bind(&path)?;
        crate::ipc::restrict_socket_permissions(&path, crate::api::server::SOCKET_PERMISSION_MODE)?;
        let identity = crate::ipc::socket_file_identity(&path)?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            path,
            identity,
            owner,
            prepared: Some((UnixListener::from_std(socket)?, router)),
            task: None,
        })
    }
    /// Share the same locked open description with rollback or an importer.
    pub(super) fn owner(&self) -> SocketOwner {
        self.owner.clone()
    }
    /// Remove only this binding's inode, keeping ownership for rebind or rollback.
    pub(super) fn unlink(&self) -> io::Result<()> {
        crate::ipc::remove_socket_file_if_owned(&self.path, self.identity)
    }
    /// The task keeps ownership until its acceptor and all HTTP tasks are dropped.
    pub(super) fn activate(&mut self) {
        if let Some((socket, router)) = self.prepared.take() {
            let owner = self.owner.clone();
            self.task = Some(tokio::spawn(async move {
                let _owner = owner;
                super::client_listener::serve(
                    || accept(&socket),
                    router.layer(axum::Extension(_owner.clone())),
                    Arc::new(SystemClock),
                    Default::default(),
                )
                .await
            }));
        }
    }
    /// Join cancellation before releasing the listener's ownership guard.
    pub(super) async fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}
async fn accept(socket: &UnixListener) -> io::Result<(UnixStream, Credentials)> {
    let (stream, _) = socket.accept().await?;
    let stream = stream.into_std()?;
    let peer = crate::platform::peer::credentials(&stream);
    Ok((UnixStream::from_std(stream)?, peer))
}

#[cfg(test)]
mod tests;
