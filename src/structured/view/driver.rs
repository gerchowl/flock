//! One read-only source task per view. App receives immutable, generation-fenced frames.
use super::{
    contracts::{Action, Snapshot},
    stream::Stream,
    Target,
};
use crate::api::{error_code::ErrorCode, structured_agent::StructuredAgentInfo};
use std::sync::Arc;
use tokio::{
    sync::{mpsc, watch, Notify},
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
    commands: mpsc::Sender<Action>,
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
        let (commands, mut input) = mpsc::channel(32);
        let task = tokio::spawn(async move {
            let _exit = Wake(wake.clone());
            let mut ended = false;
            loop {
                let result = tokio::select! { biased;
                    command = input.recv() => {
                        let Some(action) = command else { break; };
                        let s = &mut stream.model.snapshot;
                        match action {
                            Action::ToggleFold(id) => { if !s.collapsed.remove(&id) { s.collapsed.insert(id); } },
                            Action::InspectCard(id) => { s.collapsed.remove(&id); s.scroll_anchor = Some(id.seq); },
                            Action::SelectPermission(h) => s.selected_permission = Some(h),
                            Action::FollowTail => { s.scroll_anchor = None; s.new_output = false; },
                            Action::ScrollLines(step) if step != 0 && (step < 0 || s.scroll_anchor.is_some()) => {
                                let at = s.scroll_anchor.and_then(|seq| s.transcript.iter().position(|c| c.seq >= seq))
                                    .unwrap_or_else(|| s.transcript.len().saturating_sub(1));
                                let i = at.saturating_add_signed(isize::from(step)).min(s.transcript.len().saturating_sub(1));
                                s.scroll_anchor = s.transcript.get(i).map(|c| c.seq);
                            },
                            Action::ScrollLines(_) => {},
                            _ => { debug_assert!(false, "non-presentation action bypassed driver whitelist"); },
                        }
                        None
                    },
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
            commands,
        }
    }
    /// Bounded view-only input. Runtime effects never enter the source task.
    pub fn control(&self, action: Action) -> Result<(), ErrorCode> {
        if !matches!(
            action,
            Action::ToggleFold(_)
                | Action::InspectCard(_)
                | Action::SelectPermission(_)
                | Action::FollowTail
                | Action::ScrollLines(_)
        ) {
            return Err(ErrorCode::RuntimeUnsupported);
        }
        self.commands
            .try_send(action)
            .map_err(|_| ErrorCode::SessionBusy)
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
