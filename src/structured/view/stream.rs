//! Generation-owned subscriptions (ADR-0036 §4/§5/§11). No authority or renderer I/O.
use super::{contracts::*, model::Model};
use crate::api::error_code::ErrorCode;
use crate::structured::{
    history::{self, Cut, History},
    publication::{Provider, Subscription},
    queue,
    workers::{
        live,
        queue::publication::{Page, Reader, Source},
    },
};
use std::io;
use tokio::sync::Semaphore;
// At most 32 active pages process-wide; saturation returns WouldBlock.
static IO: Semaphore = Semaphore::const_new(32);
/// One controller's replay/live/queue continuation. Drive `next` outside rendering;
/// refresh canonical facts through App before exposing a snapshot or acting on it.
/// Cold sources and storage/admission failures remain explicit errors. No recovery
/// writer, guessed queue state, or automatic mutation retry is introduced here.
pub(crate) struct Stream {
    pub model: Model,
    /// Queue-owner reservation/refusal; independent of composer admission.
    pub queue_status: (Option<queue::scheduler::Pending>, Option<ErrorCode>),
    history: History,
    cut: Option<Cut>,
    live: live::Subscription,
    source: Source,
    queue: Reader,
    queue_cursor: u64,
    // Replacement contents stay private until their exact completion high-water.
    replacement: Option<(u64, Vec<queue::Item>)>,
    ended: [bool; 3],
}
impl Stream {
    /// Bind the three canonical sources without starting a writer or producer.
    pub fn new(
        model: Model,
        history: History,
        live: live::Subscription,
        source: Source,
    ) -> io::Result<Self> {
        let queue = source.subscribe(None).map_err(io::Error::other)?;
        Ok(Self {
            model,
            queue_status: (None, None),
            history,
            cut: None,
            live,
            source,
            queue,
            queue_cursor: 0,
            replacement: None,
            ended: [false; 3],
        })
    }
    /// Retire prompts, draft and cursors; stop paging the old subscriptions.
    pub fn retire(&mut self) {
        self.ended = [true; 3];
        self.cut = None;
        self.replacement = None;
        self.queue_cursor = 0;
        self.queue_status = (None, None);
        let s = &self.model.snapshot;
        let target = s.target.clone();
        let agent = s.agent.clone();
        self.model = Model::new(target, agent, s.requested_permissions, vec![]);
        self.model.snapshot.replay = Replay::Ended;
    }
    fn apply_live(&mut self, item: live::Item) -> bool {
        if matches!(item, live::Item::End(live::End::Retired)) {
            self.retire();
            return false;
        }
        self.ended[1] = matches!(item, live::Item::End(_));
        if matches!(item, live::Item::Gap { .. }) {
            self.cut = None;
            self.ended[0] = false;
        }
        self.model.live(item);
        true
    }
    /// One bounded step; false means all producers ended. Callers bound stage waits.
    /// Cancelled blocking pages retain their I/O permits through completion.
    pub async fn next(&mut self) -> io::Result<bool> {
        if self.ended == [true; 3] {
            return Ok(false);
        }
        let hw = self.history.waker();
        let qw = self.queue.waker();
        let (hn, qn) = (hw.notified(), qw.notified());
        tokio::pin!(hn, qn);
        hn.as_mut().enable();
        qn.as_mut().enable();
        if !self.ended[2] {
            let page = self.queue.page();
            let pending = matches!(page, Page::Pending);
            match page {
                Page::Gap(g) => {
                    self.replacement = Some((g.snapshot_to, vec![]));
                    self.queue_cursor = g.to;
                }
                Page::Record(v) => {
                    let r: queue::Record = serde_json::from_value(v)?;
                    if r.seq > self.queue_cursor {
                        let items = self
                            .replacement
                            .as_mut()
                            .map_or(&mut self.model.snapshot.queue, |(_, items)| items);
                        r.change.apply(items)?;
                        self.queue_cursor = r.seq;
                    }
                }
                Page::Status(p, e) => self.queue_status = (p, e),
                Page::Refused(e) => return Err(io::Error::other(e)),
                Page::Lagged(_) => {
                    self.queue = self
                        .source
                        .subscribe(Some(self.queue_cursor))
                        .map_err(io::Error::other)?
                }
                Page::End if self.replacement.is_some() => {
                    return Err(io::ErrorKind::UnexpectedEof.into())
                }
                Page::End => self.ended[2] = true,
                Page::Pending => (),
                _ => return Ok(true),
            }
            if let Some((_, items)) = self
                .replacement
                .take_if(|(through, _)| self.queue_cursor >= *through)
            {
                self.model.snapshot.queue = items;
            }
            if !pending {
                return Ok(true);
            }
        }
        if !self.ended[0] {
            let permit = IO.try_acquire().map_err(|_| io::ErrorKind::WouldBlock)?;
            // Static semaphore ownership survives cancellation through the blocking closure.
            let (cut, item) = (self.cut.take(), self.model.snapshot.applied_cursor);
            let source = self.history.clone();
            let (cut, item) = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let mut cut = match cut {
                    Some(c) => c,
                    None => source.subscribe(Some(item))?,
                };
                let item = cut.page()?;
                Ok::<_, io::Error>((cut, item))
            })
            .await
            .map_err(io::Error::other)??;
            self.cut = Some(cut);
            self.ended[0] = matches!(item, history::Item::End);
            let pending = matches!(item, history::Item::Pending);
            if matches!(item, history::Item::Lagged(_)) {
                self.cut = None;
            }
            self.model.history(item)?;
            if !pending {
                return Ok(true);
            }
        }
        tokio::select! {
            _ = &mut hn, if !self.ended[0] => {},
            _ = &mut qn, if !self.ended[2] => {},
            item = self.live.recv(), if !self.ended[1] => {
                if !self.apply_live(item) { return Ok(false); }
            },
            else => return Ok(false),
        }
        Ok(true)
    }
}
#[cfg(test)]
mod tests;
