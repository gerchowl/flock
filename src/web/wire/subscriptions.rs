//! Per-socket identities and owned agent replay (ADR-0036 §5a).
use super::{output::Delivery, *};
use crate::structured::history::{owner, Item, Resume};
use std::{collections::HashSet, sync::atomic::AtomicBool};
use tokio::{
    sync::{oneshot, Notify},
    task::JoinHandle,
};
/// Per connection: one outstanding delivery for each of eight active cuts.
const ACTIVE: usize = 8;
/// Lifetime identity budget. Exhaustion ends the socket without forgetting IDs.
const IDS: usize = 1024;
/// Global queued/running page jobs. Cancelled waiters retain their job's permit.
pub(super) static IO: Semaphore = Semaphore::const_new(32);
pub(super) struct Hint {
    cursor: u64,
    checkpoint: Resume,
    identity: Arc<Notify>,
}
pub(super) type Hints = Arc<Mutex<HashMap<String, Hint>>>;
struct Subscription {
    live: Arc<AtomicBool>,
    task: JoinHandle<()>,
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.live.store(false, Ordering::Release);
        self.task.abort();
    }
}
pub(super) struct Subscriptions {
    used: HashSet<String>,
    active: HashMap<String, Subscription>,
    hints: Hints,
    owner: owner::Client,
    data: mpsc::Sender<Delivery>,
    shared: Arc<Shared>,
    session: Session,
    priority: mpsc::Sender<Message>,
}
impl Subscriptions {
    pub(super) fn new(
        shared: Arc<Shared>,
        data: mpsc::Sender<Delivery>,
        session: Session,
        priority: mpsc::Sender<Message>,
    ) -> Self {
        let owner = shared.histories.clone();
        Self {
            used: HashSet::new(),
            active: HashMap::new(),
            hints: Arc::default(),
            owner,
            data,
            shared,
            session,
            priority,
        }
    }
    pub(super) fn unsubscribe(&mut self, id: &str) {
        self.active.remove(id);
    }
    /// False requests a resource close. Refusals consume the ID too.
    pub(super) fn subscribe(&mut self, id: String, stream: String, after: Option<u64>) -> bool {
        if id.len() > 128 || stream.len() > 256 {
            return false;
        }
        self.active.retain(|_, sub| !sub.task.is_finished());
        let refusal = if self.used.contains(&id) {
            self.unsubscribe(&id);
            Some(SubfailCode::SubIdReused)
        } else if self.used.len() == IDS {
            return false;
        } else {
            self.used.insert(id.clone());
            None
        };
        if self.active.len() == ACTIVE {
            return false;
        }
        let live = Arc::new(AtomicBool::new(true));
        let (owner, data, hints, fence) = (
            self.owner.clone(),
            self.data.clone(),
            self.hints.clone(),
            live.clone(),
        );
        let key = id.clone();
        let (shared, session, priority) = (
            self.shared.clone(),
            self.session.clone(),
            self.priority.clone(),
        );
        let task = tokio::spawn(async move {
            let result = if let Some(code) = refusal {
                Err(Some(code))
            } else {
                use crate::structured::permissions::auth::tokens::{Scope, TokenError};
                let store = shared.store.clone();
                match super::super::store_io::shared()
                    .run(move || store.admit(&session, Scope::Read, |_| ()))
                    .await
                {
                    Some(Ok(())) => {
                        if stream.starts_with("agent/") {
                            replay(&owner, &data, &fence, &hints, &id, &stream, after).await
                        } else {
                            Err(Some(SubfailCode::UnknownStream))
                        }
                    }
                    Some(Err(TokenError::Scope)) => Err(Some(SubfailCode::Scope)),
                    Some(Err(TokenError::Unauthorized | TokenError::Revoked)) => {
                        let _ = priority.send(close(AUTH_REVOKED_CLOSE)).await;
                        return;
                    }
                    Some(Err(TokenError::Busy | TokenError::Unsafe | TokenError::Io(_))) | None => {
                        Err(None)
                    }
                }
            };
            if result == Err(None) {
                let _ = priority.send(close(GOING_AWAY_CLOSE)).await;
                return;
            }
            if let Err(Some(code)) = result {
                let frame = ServerFrame::Subfail {
                    sub_id: id,
                    code,
                    message: "stream unavailable".into(),
                };
                let _ = deliver(&data, &fence, frame).await;
            }
        });
        self.active.insert(key, Subscription { live, task });
        true
    }
}
async fn deliver(
    data: &mpsc::Sender<Delivery>,
    live: &Arc<AtomicBool>,
    frame: ServerFrame,
) -> bool {
    let (tx, rx) = oneshot::channel();
    if data
        .send(Delivery {
            frame,
            active: Some(live.clone()),
            delivered: Some(tx),
        })
        .await
        .is_err()
    {
        return false;
    }
    rx.await == Ok(true)
}
pub(super) async fn replay(
    owner: &owner::Client,
    data: &mpsc::Sender<Delivery>,
    live: &Arc<AtomicBool>,
    hints: &Hints,
    id: &str,
    stream: &str,
    after: Option<u64>,
) -> Result<(), Option<SubfailCode>> {
    let source = owner
        .lookup(stream[6..].into())
        .await
        .map_err(|error| {
            (error.kind() == io::ErrorKind::InvalidInput).then_some(SubfailCode::InvalidAgent)
        })?
        .ok_or(SubfailCode::UnknownStream)?;
    let wake = source.waker();
    let mut cut = {
        let cache = hints.lock().map_err(|_| None)?;
        if let Some(hint) = cache
            .get(stream)
            .filter(|hint| Some(hint.cursor) == after && Arc::ptr_eq(&hint.identity, &wake))
        {
            source.resume(hint.cursor, &hint.checkpoint)
        } else {
            source.subscribe(after)
        }
    }
    .map_err(|error| {
        if error.kind() == io::ErrorKind::InvalidInput {
            Some(SubfailCode::CursorAhead)
        } else {
            None
        }
    })?;
    let mut cursor = after.unwrap_or(0);
    loop {
        let notified = wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let permit = IO.acquire().await.map_err(|_| None)?;
        let (next, item) = tokio::task::spawn_blocking(move || {
            let item = cut.page();
            drop(permit);
            (cut, item)
        })
        .await
        .map_err(|_| None)?;
        cut = next;
        let item = item.map_err(|_| None)?;
        let frame = match item {
            Item::Record(record) => {
                cursor = record.seq;
                let ev = if record.known == Some(false) {
                    Some(WireEvent::Unknown(UnknownEvent {
                        r#type: UnknownTag::Unknown,
                        raw: record.raw.clone(),
                        shape: record
                            .shape
                            .as_ref()
                            .ok_or(SubfailCode::UnknownStream)?
                            .0
                            .clone(),
                    }))
                } else {
                    serde_json::from_str(&record.raw)
                        .ok()
                        .map(|event| WireEvent::Session(Box::new(event)))
                };
                match ev {
                    Some(ev) => ServerFrame::Ev {
                        sub_id: id.into(),
                        stream: stream.into(),
                        seq: Some(cursor),
                        ts_ms: u64::try_from(record.received_at_ms).map_err(|_| None)?,
                        ev,
                    },
                    None => ServerFrame::Gap {
                        sub_id: id.into(),
                        from: cursor,
                        to: cursor,
                        snapshot_to: None,
                    },
                }
            }
            Item::Gap { from, to, .. } => {
                cursor = to;
                ServerFrame::Gap {
                    sub_id: id.into(),
                    from,
                    to,
                    snapshot_to: None,
                }
            }
            Item::Synced(seq) => ServerFrame::Synced {
                sub_id: id.into(),
                seq,
            },
            Item::Lagged(skipped_to) => ServerFrame::Lagged {
                sub_id: id.into(),
                skipped_to,
            },
            Item::End => return Ok(()),
            Item::Pending => {
                notified.await;
                continue;
            }
            Item::Progress => {
                tokio::task::yield_now().await;
                continue;
            }
        };
        let frame = if super::output::fits(&frame) {
            frame
        } else {
            ServerFrame::Lagged {
                sub_id: id.into(),
                skipped_to: cursor,
            }
        };
        let ended = matches!(frame, ServerFrame::Lagged { .. });
        if !deliver(data, live, frame).await || ended {
            return Ok(());
        }
        if let Some(checkpoint) = cut.checkpoint() {
            let mut hints = hints.lock().map_err(|_| None)?;
            if hints.len() < ACTIVE || hints.contains_key(stream) {
                hints.insert(
                    stream.into(),
                    Hint {
                        cursor,
                        checkpoint,
                        identity: wake.clone(),
                    },
                );
            }
        }
    }
}
