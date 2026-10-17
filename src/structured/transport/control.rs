//! The stdout owner serializes permission facts, responses and terminal closes.
use super::*;
use permissions::{CloseReason, Closing};

pub(super) struct Recorder<'a> {
    file: &'a mut File,
    seq: &'a mut u64,
    version: &'a str,
    tx: &'a mpsc::SyncSender<Received>,
    busy: &'a AtomicBool,
    uncertain: &'a AtomicBool,
    cancel: &'a AtomicBool,
    controls: &'a ControlPlane,
}
impl<'a> Recorder<'a> {
    pub fn new(
        file: &'a mut File,
        seq: &'a mut u64,
        version: &'a str,
        tx: &'a mpsc::SyncSender<Received>,
        busy: &'a AtomicBool,
        uncertain: &'a AtomicBool,
        cancel: &'a AtomicBool,
        controls: &'a ControlPlane,
    ) -> Self {
        Self {
            file,
            seq,
            version,
            tx,
            busy,
            uncertain,
            cancel,
            controls,
        }
    }
    fn record(&mut self, raw: &[u8], count: u64, synthetic: bool) -> io::Result<Received> {
        persist(
            self.file,
            self.seq,
            raw,
            count,
            self.version,
            self.busy,
            self.uncertain,
            synthetic,
        )
    }
    fn audit(&mut self, value: &Value) -> io::Result<Received> {
        let mut raw = encode(value)?;
        raw.pop();
        self.record(&raw, raw.len() as u64, true)
    }
    pub fn command(&mut self, command: Command) -> io::Result<()> {
        match command {
            Command::Decide(control) => self.answer(control),
            Command::Close(closings) => self.close(closings),
        }
    }
    fn answer(&mut self, control: Control) -> io::Result<()> {
        // A cancellation reserved before the reader claims this answer wins.
        if !self.controls.book.lock().begin_answer(&control.answer.id) {
            return Ok(());
        }
        let event = self.audit(&control.answer.audit)?;
        self.controls.book.lock().audited(&control.answer);
        publish(event, self.tx, self.cancel)?;
        self.controls
            .input
            .try_send(Input::Decision(control))
            .map_err(|_| io::Error::other("permission writer queue unavailable"))
    }
    fn close(&mut self, closings: Vec<Closing>) -> io::Result<()> {
        let mut events = Vec::with_capacity(closings.len());
        for closing in closings {
            events.push(self.audit(&closing.audit)?);
            self.controls.book.lock().closed(&closing.id);
        }
        // Durability and gate updates precede notification, so a full queue or
        // cancelled receiver cannot leave a replay with half its requests open.
        for event in events {
            publish(event, self.tx, self.cancel)?;
        }
        Ok(())
    }
    pub fn finish(&mut self) -> io::Result<()> {
        let closings = self.controls.book.lock().terminal();
        self.close(closings)
    }
    pub fn observed(
        &mut self,
        line: &[u8],
        count: u64,
        metadata: metadata::Line,
    ) -> io::Result<()> {
        let mut open = None;
        let mut denial = None;
        let mut closings = Vec::new();
        if count > MAX_LINE_BYTES as u64 {
            if let Some((id, request)) = metadata.oversized_request(count)? {
                let (audit, answer) = self
                    .controls
                    .book
                    .lock()
                    .oversized(&id, request)
                    .map_err(io::Error::other)?;
                open = Some(audit);
                denial = Some(answer);
            }
        } else if let Ok(raw) = std::str::from_utf8(line) {
            match SessionEvent::parse(raw, self.version) {
                SessionEvent::CanUseTool(event) => {
                    if encode(&permissions::open_record(&event.request_id, &event.request)).is_err()
                    {
                        let (id, request) = metadata
                            .oversized_request(count)?
                            .ok_or_else(|| io::Error::other(ErrorCode::InvalidRequest))?;
                        let (audit, answer) = self
                            .controls
                            .book
                            .lock()
                            .oversized(&id, request)
                            .map_err(io::Error::other)?;
                        open = Some(audit);
                        denial = Some(answer);
                    } else {
                        open = Some(
                            self.controls
                                .book
                                .lock()
                                .observe(&event.request_id, event.request)
                                .map_err(io::Error::other)?,
                        );
                    }
                }
                SessionEvent::Success(event)
                    if event
                        .parent_tool_use_id
                        .as_ref()
                        .and_then(Option::as_ref)
                        .is_none() =>
                {
                    closings = self
                        .controls
                        .book
                        .lock()
                        .close(None, CloseReason::Result)
                        .map_err(io::Error::other)?;
                }
                SessionEvent::ExecutionError(event)
                    if event
                        .parent_tool_use_id
                        .as_ref()
                        .and_then(Option::as_ref)
                        .is_none() =>
                {
                    closings = self
                        .controls
                        .book
                        .lock()
                        .close(None, CloseReason::Result)
                        .map_err(io::Error::other)?;
                }
                _ => (),
            }
        }
        let raw = self.record(line, count, false)?;
        // Persist open before publishing raw: cancellation at either notification
        // must still leave an open record for the terminal close to match.
        let opened = open.as_ref().map(|audit| self.audit(audit)).transpose()?;
        // Result/cancel retire requests before their raw notification is visible.
        // close() persists every close before attempting its first notification.
        let mut closed = Vec::new();
        for closing in closings {
            closed.push(self.audit(&closing.audit)?);
            self.controls.book.lock().closed(&closing.id);
        }
        publish(raw, self.tx, self.cancel)?;
        if let Some(event) = opened {
            publish(event, self.tx, self.cancel)?;
        }
        for event in closed {
            publish(event, self.tx, self.cancel)?;
        }
        if let Some(answer) = denial {
            let bytes = encode(&answer.response)?;
            self.answer(Control { answer, bytes })?;
        }
        Ok(())
    }
}
