//! One read-only source task per view. App receives immutable, generation-fenced frames.
use super::{contracts::Snapshot, stream::Stream, Target};
use crate::api::{error_code::ErrorCode, structured_agent::StructuredAgentInfo};
use std::sync::Arc;
use tokio::{
    sync::{watch, Notify},
    task::JoinHandle,
};
pub(crate) type Published = Result<Arc<Snapshot>, ErrorCode>;
#[derive(Clone, PartialEq)]
pub(crate) struct Facts {
    pub target: Target,
    pub agent: StructuredAgentInfo,
}
pub(crate) struct Driver {
    pub target: Target,
    facts: watch::Sender<Option<Facts>>,
    frames: watch::Receiver<Arc<Snapshot>>,
    error: watch::Receiver<Option<ErrorCode>>,
    task: JoinHandle<()>,
}
struct Wake(Arc<Notify>);
impl Drop for Wake {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}
fn publish(
    s: &mut Stream,
    facts: &watch::Receiver<Option<Facts>>,
    tx: &watch::Sender<Arc<Snapshot>>,
    wake: &Notify,
) -> bool {
    let Some(current) = facts.borrow().clone() else {
        return false;
    };
    if current.target != s.model.snapshot.target {
        return false;
    }
    if s.model.snapshot.agent != current.agent {
        s.model.snapshot.agent = current.agent;
    }
    // Do not hold this read lock while copy-on-write materializes a frame.
    // Recheck at publication; App also validates the canonical epoch on presentation.
    let guard = facts.borrow();
    if !guard
        .as_ref()
        .is_some_and(|f| f.target == s.model.snapshot.target)
    {
        return false;
    }
    let frame = Arc::clone(&s.model.snapshot.0);
    if tx.send_if_modified(|old| {
        if Arc::ptr_eq(old, &frame) {
            false
        } else {
            *old = frame;
            true
        }
    }) {
        wake.notify_one();
    }
    true
}
impl Driver {
    pub fn new(mut stream: Stream, wake: Arc<Notify>) -> Self {
        let target = stream.model.snapshot.target.clone();
        let (facts, mut updates) = watch::channel(Some(Facts {
            target: target.clone(),
            agent: stream.model.snapshot.agent.clone(),
        }));
        let (tx, frames) = watch::channel(Arc::clone(&stream.model.snapshot.0));
        let (errors, error) = watch::channel(None);
        let task = tokio::spawn(async move {
            let _exit = Wake(wake.clone());
            let mut ended = false;
            loop {
                let result = tokio::select! { biased;
                    changed = updates.changed() => { if changed.is_err() { break; } None },
                    step = stream.next(), if !ended => Some(step),
                };
                if let Some(result) = result {
                    match result {
                        Ok(alive) => ended = !alive,
                        Err(err) => {
                            let code = err
                                .get_ref()
                                .and_then(|e| e.downcast_ref::<ErrorCode>())
                                .copied()
                                .unwrap_or_else(|| crate::cli::output::io_error_code(&err));
                            errors.send_replace(Some(code));
                            ended = true;
                            wake.notify_one();
                        }
                    }
                }
                if !publish(&mut stream, &updates, &tx, &wake) {
                    errors.send_replace(Some(ErrorCode::AgentNotFound));
                    break;
                }
            }
        });
        Self {
            target,
            facts,
            frames,
            error,
            task,
        }
    }
    pub fn refresh(&self, facts: Facts) {
        self.facts.send_if_modified(|old| {
            if old.as_ref() == Some(&facts) {
                false
            } else {
                *old = Some(facts);
                true
            }
        });
    }
    pub fn frame(&self) -> Published {
        if let Some(code) = *self.error.borrow() {
            return Err(code);
        }
        if self.task.is_finished() {
            return Err(ErrorCode::InternalError);
        }
        Ok(Arc::clone(&self.frames.borrow()))
    }
    #[cfg(test)]
    pub fn id(&self) -> tokio::task::Id {
        self.task.id()
    }
}
impl Drop for Driver {
    fn drop(&mut self) {
        self.task.abort();
    }
}
