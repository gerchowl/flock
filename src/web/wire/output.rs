//! One bounded writer, with heartbeat priority between data writes.
use super::{timing::Clock, *};
use futures_util::Sink;
/// A queued message's fence is checked immediately before starting its write.
/// Already-started writes cannot be recalled. Closing a fence discards queued
/// messages only; the client still drops late frames for a closed ID (§5a).
#[derive(Debug)]
pub(super) struct Delivery {
    /// A complete bounded wire message.
    pub(super) frame: ServerFrame,
    /// Independent per-ID fence; None denotes unscoped data.
    pub(super) active: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// True after flush, false when fenced out; channel closure means writer failure.
    pub(super) delivered: Option<tokio::sync::oneshot::Sender<bool>>,
}
impl From<ServerFrame> for Delivery {
    fn from(frame: ServerFrame) -> Self {
        Self {
            frame,
            active: None,
            delivered: None,
        }
    }
}
/// Complete messages are capped before encoding and again by the WebSocket layer.
pub(super) async fn writer<S: Sink<Message> + Unpin>(
    mut sink: S,
    clock: Arc<dyn Clock>,
    mut priority: mpsc::Receiver<Message>,
    mut data: mpsc::Receiver<Delivery>,
) {
    loop {
        let (frame, delivered) = tokio::select! { biased;
            frame = priority.recv() => match frame { Some(frame) => (frame, None), None => return },
            next = data.recv() => {
                let Some(next) = next else { return };
                if next.active.as_ref().is_some_and(|active| !active.load(Ordering::Acquire)) {
                    if let Some(done) = next.delivered { let _ = done.send(false); }
                    continue;
                }
                let Some(frame) = encode(next.frame) else { return };
                (frame, next.delivered)
            },
        };
        let closing = matches!(frame, Message::Close(_));
        if !matches!(
            before(
                clock.as_ref(),
                clock.now() + Duration::from_millis(LIVENESS_WINDOW_MS),
                sink.send(frame)
            )
            .await,
            Some(Ok(()))
        ) || closing
        {
            return;
        }
        if let Some(done) = delivered {
            let _ = done.send(true);
        }
    }
}
struct Limited(Vec<u8>);
impl io::Write for Limited {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_MESSAGE - self.0.len() {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub(super) fn encode(frame: ServerFrame) -> Option<Message> {
    let mut bytes = Limited(Vec::new());
    serde_json::to_writer(&mut bytes, &frame).ok()?;
    Some(Message::Text(String::from_utf8(bytes.0).ok()?.into()))
}
