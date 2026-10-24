//! Serialized fleet recovery with off-executor persistence and frame preparation.
use super::{
    subscriptions::{deliver, IO},
    *,
};
use crate::structured::{
    fleet_lite::{Delta, Provider, Store, StructuredAgent},
    registry::{Record, Registry},
    workers::Workers,
};
use std::{
    collections::VecDeque,
    path::Path,
    sync::atomic::AtomicBool,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{broadcast, oneshot, Notify};
#[derive(Clone, PartialEq, Eq)]
struct Agent(Record, crate::api::agent_state::AgentState, Vec<String>);
impl Provider for Vec<Agent> {
    fn agents(&self) -> io::Result<Vec<StructuredAgent<'_>>> {
        Ok(self
            .iter()
            .map(|a| StructuredAgent {
                record: &a.0,
                state: &a.1,
                activity: None,
                waiting_on_human: &a.2,
            })
            .collect())
    }
}
type Observation = (
    PathBuf,
    Result<Vec<Agent>, crate::api::error_code::ErrorCode>,
);
type Request = (Option<u64>, oneshot::Sender<io::Result<Cut>>);
// Shared ring: sixteen frames, each within the existing 64 KiB writer limit.
const LIVE: usize = 16;
type Frame = Option<Arc<ServerFrame>>;
#[derive(Clone)]
pub(super) struct Client {
    updates: watch::Sender<Option<Arc<Observation>>>,
    requests: mpsc::Sender<Request>,
    wake: Arc<Notify>,
    done: watch::Receiver<bool>,
}
impl Client {
    pub(super) fn new(wake: Arc<Notify>, stop: watch::Receiver<bool>) -> Self {
        let (updates, observations) = watch::channel(None);
        let (requests, rx) = mpsc::channel(32);
        let (done, closed) = watch::channel(false);
        tokio::spawn(async move {
            if let Err(error) = run(observations, rx, stop).await {
                crate::logging::structured_fleet_stopped(&error);
            }
            done.send_replace(true);
        });
        Self {
            updates,
            requests,
            wake,
            done: closed,
        }
    }
    /// Called only on the registry owner. No unavailable permission read becomes empty.
    pub(super) fn observe(&self, registry: &Registry, workers: &Workers, root: &Path) {
        let agents = registry
            .records()
            .into_iter()
            .map(|record| {
                let (_, view, generation) = registry
                    .get(&record.agent_id)
                    .ok_or(crate::api::error_code::ErrorCode::AgentNotFound)?;
                let waiting = match workers.runtime(&record.agent_id, generation) {
                    Some(runtime) => runtime
                        .waiting_on_human()?
                        .into_iter()
                        .map(|(id, _)| id)
                        .collect(),
                    // Only a quiescent producer proves that an absent book is empty.
                    None if workers.history_recovery_allowed(&record.agent_id) => Vec::new(),
                    None => return Err(crate::api::error_code::ErrorCode::SessionBusy),
                };
                Ok(Agent(record, view.update.state.clone(), waiting))
            })
            .collect();
        let observation = Arc::new((root.join("fleet.json"), agents));
        self.updates.send_if_modified(|old| {
            if old.as_ref() == Some(&observation) {
                return false;
            }
            *old = Some(observation);
            true
        });
    }
    pub(super) async fn subscribe(&self, after: Option<u64>) -> io::Result<Cut> {
        let (tx, rx) = oneshot::channel();
        self.requests
            .try_send((after, tx))
            .map_err(|_| io::Error::from(io::ErrorKind::WouldBlock))?;
        self.wake.notify_one();
        rx.await.map_err(|_| io::ErrorKind::BrokenPipe)?
    }
    pub(super) async fn closed(&self) {
        let _ = self.done.clone().wait_for(|done| *done).await;
    }
}
fn event(delta: Delta, ts_ms: u64, head: u64) -> io::Result<Arc<ServerFrame>> {
    let mut frame = ServerFrame::Ev {
        // Reserve the largest JSON-escaped ID before admitting a frame to shared storage.
        sub_id: "\0".repeat(super::subscriptions::ID_BYTES),
        stream: "fleet".into(),
        seq: Some(delta.seq),
        ts_ms,
        ev: WireEvent::Json(serde_json::to_value(delta.event)?),
    };
    if !super::output::fits(&frame) {
        frame = ServerFrame::Lagged {
            sub_id: String::new(),
            skipped_to: head,
        };
    }
    if let ServerFrame::Ev { sub_id, .. } = &mut frame {
        sub_id.clear();
    }
    Ok(Arc::new(frame))
}
fn checkpoint(
    snapshot: crate::structured::fleet_lite::Snapshot,
    ts: u64,
) -> io::Result<(u64, VecDeque<Arc<ServerFrame>>)> {
    let end = snapshot.snapshot_to;
    let mut frames = VecDeque::from([Arc::new(ServerFrame::Gap {
        sub_id: String::new(),
        from: 0,
        to: snapshot.to,
        snapshot_to: Some(end),
    })]);
    for delta in snapshot.rows {
        frames.push_back(event(delta, ts, end)?);
    }
    Ok((end, frames))
}
async fn run(
    mut updates: watch::Receiver<Option<Arc<Observation>>>,
    mut requests: mpsc::Receiver<Request>,
    mut stop: watch::Receiver<bool>,
) -> io::Result<()> {
    let mut store: Option<Store> = None;
    let (frames, _) = broadcast::channel(LIVE);
    let head = Arc::new(AtomicU64::new(0));
    loop {
        let request = tokio::select! { biased;
            _ = stop.changed() => return Ok(()),
            request = requests.recv() => match request { Some(request) => Some(request), None => return Ok(()) },
            changed = updates.changed() => { if changed.is_err() { return Ok(()); } None },
        };
        if request.as_ref().is_some_and(|(_, reply)| reply.is_closed()) {
            continue;
        }
        while updates.borrow().is_none() {
            tokio::select! { biased; _ = stop.changed() => return Ok(()), result = updates.changed() => { if result.is_err() { return Ok(()); } } }
        }
        let observation = updates
            .borrow_and_update()
            .clone()
            .ok_or(io::ErrorKind::BrokenPipe)?;
        if let Err(code) = &observation.1 {
            tracing::warn!(code = code.as_str(), "fleet provider unavailable");
            let _ = frames.send(None);
            if let Some((_, reply)) = request {
                let _ = reply.send(Err(io::ErrorKind::WouldBlock.into()));
            }
            continue;
        }
        let wants_cut = request.as_ref().map(|(after, _)| *after);
        let path = observation.0.clone();
        let permit = IO.acquire().await.map_err(|_| io::ErrorKind::BrokenPipe)?;
        let (next, deltas, snapshot) = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut store = match store {
                Some(store) => store,
                None => {
                    let store = Store::open(path)?;
                    for diagnostic in store.diagnostics() {
                        crate::logging::structured_fleet_recovered(diagnostic);
                    }
                    store
                }
            };
            let deltas = store.refresh(
                observation
                    .1
                    .as_ref()
                    .map_err(|_| io::ErrorKind::WouldBlock)?,
            )?;
            let snapshot = wants_cut
                .map(|after| {
                    if after.is_some_and(|after| after > store.high_water()) {
                        return Err(io::ErrorKind::InvalidInput.into());
                    }
                    store.snapshot()
                })
                .transpose();
            let ts = u64::try_from(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(io::Error::other)?
                    .as_millis(),
            )
            .map_err(io::Error::other)?;
            let floor = store.high_water();
            let deltas = deltas
                .into_iter()
                .map(|d| event(d, ts, floor))
                .collect::<io::Result<Vec<_>>>()?;
            let snapshot =
                snapshot.and_then(|snapshot| snapshot.map(|s| checkpoint(s, ts)).transpose());
            Ok::<_, io::Error>((store, deltas, snapshot))
        })
        .await
        .map_err(io::Error::other)??;
        head.store(next.high_water(), Ordering::Release);
        store = Some(next);
        for delta in deltas {
            let _ = frames.send(Some(delta));
            tokio::task::yield_now().await;
        }
        if let Some((after, reply)) = request {
            match snapshot {
                Ok(Some((end, mut initial))) => {
                    // Only this cut owns its checkpoint. The ring carries live deltas.
                    initial.push_back(Arc::new(ServerFrame::Synced {
                        sub_id: String::new(),
                        seq: end,
                    }));
                    let _ = reply.send(Ok(Cut {
                        initial,
                        frames: frames.subscribe(),
                        head: head.clone(),
                        cursor: after.unwrap_or(0),
                    }));
                }
                Ok(None) => return Err(io::ErrorKind::InvalidData.into()),
                Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                    let _ = reply.send(Err(error));
                }
                Err(error) => return Err(error),
            }
        }
    }
}
pub(super) struct Cut {
    initial: VecDeque<Arc<ServerFrame>>,
    frames: broadcast::Receiver<Frame>,
    head: Arc<AtomicU64>,
    cursor: u64,
}
impl Cut {
    pub(super) async fn next(&mut self, id: &str) -> io::Result<ServerFrame> {
        let lagged = || {
            Arc::new(ServerFrame::Lagged {
                sub_id: String::new(),
                skipped_to: self.head.load(Ordering::Acquire),
            })
        };
        let mut frame = if self.frames.len() > LIVE {
            lagged()
        } else if let Some(frame) = self.initial.pop_front() {
            frame
        } else {
            match self.frames.recv().await {
                Ok(Some(frame)) => frame,
                Err(broadcast::error::RecvError::Lagged(_)) => lagged(),
                _ => return Err(io::ErrorKind::BrokenPipe.into()),
            }
        };
        let frame = Arc::make_mut(&mut frame);
        match frame {
            ServerFrame::Ev {
                sub_id,
                seq: Some(seq),
                ..
            } => {
                *sub_id = id.into();
                self.cursor = *seq;
            }
            ServerFrame::Gap {
                sub_id,
                from,
                to,
                snapshot_to,
                ..
            } => {
                *sub_id = id.into();
                *from = self
                    .cursor
                    .checked_add(1)
                    .ok_or(io::ErrorKind::InvalidData)?;
                if *snapshot_to == Some(*to) {
                    self.cursor = *to;
                }
            }
            ServerFrame::Synced { sub_id, .. } | ServerFrame::Lagged { sub_id, .. } => {
                *sub_id = id.into()
            }
            _ => return Err(io::ErrorKind::InvalidData.into()),
        }
        Ok(frame.clone())
    }
}
pub(super) async fn replay(
    client: &Client,
    data: &mpsc::Sender<output::Delivery>,
    live: &Arc<AtomicBool>,
    id: &str,
    after: Option<u64>,
) -> Result<(), Option<SubfailCode>> {
    let mut cut = client.subscribe(after).await.map_err(|e| {
        (e.kind() == io::ErrorKind::InvalidInput).then_some(SubfailCode::CursorAhead)
    })?;
    loop {
        let frame = cut.next(id).await.map_err(|_| None)?;
        let ended = matches!(frame, ServerFrame::Lagged { .. });
        if !deliver(data, live, frame).await || ended {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests;
