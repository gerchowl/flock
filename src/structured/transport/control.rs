//! The stdout owner serializes permission facts, responses and terminal closes.
use super::*;
use permissions::{CloseReason, Closing};

#[derive(Default)]
struct Admission {
    open: Option<Value>,
    answer: Option<permissions::Answer>,
    refusal: Option<permissions::Answer>,
}

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
    fn record(
        &mut self,
        raw: &[u8],
        count: u64,
        synthetic: bool,
        grant_request_id: Option<&str>,
        request_seq: Option<u64>,
    ) -> io::Result<Received> {
        persist(
            self.file,
            self.seq,
            raw,
            count,
            self.version,
            self.busy,
            self.uncertain,
            synthetic,
            grant_request_id,
            request_seq,
        )
    }
    fn audit(&mut self, value: &Value) -> io::Result<Received> {
        let mut raw = encode(value)?;
        raw.pop();
        self.record(&raw, raw.len() as u64, true, None, None)
    }
    /// Drain a host admission before recording any later stdout evidence.
    pub fn accepted(&mut self) -> io::Result<()> {
        while let Ok(accepted) = self.controls.accepted.try_recv() {
            let mut event = self.audit(&serde_json::json!({
                "type": "flk_input_accepted", "at_ms": accepted.at_ms,
            }))?;
            event.accepted = Some(accepted);
            publish(event, self.tx, self.cancel)?;
        }
        Ok(())
    }
    pub fn command(&mut self, command: Command) -> io::Result<()> {
        match command {
            Command::Decide(control) => self.answer(control),
            Command::Close(closings) => self.close(closings),
        }
    }
    fn answer(&mut self, control: Control) -> io::Result<()> {
        // A cancellation reserved before the reader claims this answer wins.
        if !self
            .controls
            .book
            .lock()
            .begin_answer(&control.answer.id, control.answer.request_seq)
        {
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
    fn close_records(&mut self, closings: Vec<Closing>) -> io::Result<Vec<Received>> {
        let mut events = Vec::with_capacity(closings.len());
        for closing in closings {
            events.push(self.audit(&closing.audit)?);
            self.controls
                .book
                .lock()
                .closed(&closing.id, closing.request_seq);
        }
        Ok(events)
    }
    fn close(&mut self, closings: Vec<Closing>) -> io::Result<()> {
        // Persist every close before any notification can block or be cancelled.
        for event in self.close_records(closings)? {
            publish(event, self.tx, self.cancel)?;
        }
        Ok(())
    }
    pub fn finish(&mut self) -> io::Result<()> {
        let closings = self.controls.book.lock().terminal();
        self.close(closings)
    }
    fn permission(
        &self,
        id: &str,
        request: super::super::claude::ToolRequest,
        oversized: bool,
    ) -> io::Result<Admission> {
        let mut book = self.controls.book.lock();
        book.resume_after(*self.seq);
        let result = if oversized {
            book.oversized(id, request)
                .map(|(audit, answer)| (audit, Some(answer)))
        } else {
            book.observe(id, request).and_then(|audit| {
                // Reserve automatic approval before open becomes visible.
                book.automatic(id).map(|answer| (audit, answer))
            })
        };
        match result {
            Ok((open, answer)) => Ok(Admission {
                open: Some(open),
                answer,
                refusal: None,
            }),
            Err(ErrorCode::PermissionRequestLimit) => Ok(Admission {
                refusal: Some(book.capacity_refusal(id).map_err(io::Error::other)?),
                ..Admission::default()
            }),
            Err(code) => Err(io::Error::other(code)),
        }
    }
    pub fn observed(
        &mut self,
        line: &[u8],
        count: u64,
        metadata: metadata::Line,
    ) -> io::Result<()> {
        self.accepted()?;
        let mut admission = Admission::default();
        let mut closings = Vec::new();
        if count > MAX_LINE_BYTES as u64 {
            if let Some((id, request)) = metadata.oversized_request(count)? {
                admission = self.permission(&id, request, true)?;
            }
        } else if let Ok(raw) = std::str::from_utf8(line) {
            match SessionEvent::parse(raw, self.version) {
                SessionEvent::CanUseTool(event) => {
                    if encode(&permissions::open_record(
                        &event.request_id,
                        u64::MAX,
                        &event.request,
                    ))
                    .is_err()
                    {
                        let (id, request) = metadata
                            .oversized_request(count)?
                            .ok_or_else(|| io::Error::other(ErrorCode::InvalidRequest))?;
                        admission = self.permission(&id, request, true)?;
                    } else {
                        admission = self.permission(&event.request_id, event.request, false)?;
                    }
                }
                SessionEvent::ControlCancel(event) => {
                    match self
                        .controls
                        .book
                        .lock()
                        .close(Some(&event.request_id), CloseReason::Cancelled)
                    {
                        Ok(closed) => closings = closed,
                        // Repeated or unrelated cancels do not invent audit facts.
                        Err(
                            ErrorCode::PermissionRequestUnknown
                            | ErrorCode::PermissionRequestClosed,
                        ) => (),
                        Err(code) => return Err(io::Error::other(code)),
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
        let grant_request_id = admission
            .answer
            .as_ref()
            .and_then(|answer| answer.audit.get("grant_request_id"))
            .and_then(Value::as_str);
        let request_seq = admission
            .open
            .as_ref()
            .and_then(|audit| audit.get("request_seq"))
            .and_then(Value::as_u64);
        let raw = self.record(line, count, false, grant_request_id, request_seq)?;
        // Persist open before publishing raw: cancellation at either notification
        // must still leave an open record for the terminal close to match.
        let opened = admission
            .open
            .as_ref()
            .map(|audit| self.audit(audit))
            .transpose()?;
        // Result/cancel retire requests before their raw notification is visible.
        // close() persists every close before attempting its first notification.
        let closed = self.close_records(closings)?;
        // A capacity refusal has no live slot. Its close must be durable before
        // either notification or the deny reaches the harness.
        let refused = if let Some(answer) = &admission.refusal {
            let event = self.audit(&answer.audit)?;
            self.controls
                .book
                .lock()
                .closed(&answer.id, answer.request_seq);
            Some(event)
        } else {
            None
        };
        publish(raw, self.tx, self.cancel)?;
        if let Some(event) = opened {
            publish(event, self.tx, self.cancel)?;
        }
        for event in closed {
            publish(event, self.tx, self.cancel)?;
        }
        if let Some(event) = refused {
            publish(event, self.tx, self.cancel)?;
        }
        if let Some(answer) = admission.answer {
            let bytes = encode(&answer.response)?;
            self.answer(Control { answer, bytes })?;
        }
        if let Some(answer) = admission.refusal {
            // Reuse cancel-aware bounded delivery: a slow stdin delays this deny
            // instead of turning the request-capacity refusal into session loss.
            publish(
                Input::Refusal(encode(&answer.response)?),
                &self.controls.input,
                self.cancel,
            )?;
        }
        Ok(())
    }
}
