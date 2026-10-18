//! One bounded writer, with heartbeat priority between data writes.
use super::{timing::Clock, *};
use futures_util::Sink;
/// Complete messages are capped before encoding and again by the WebSocket layer.
pub(super) async fn writer<S: Sink<Message> + Unpin>(
    mut sink: S,
    clock: Arc<dyn Clock>,
    mut priority: mpsc::Receiver<Message>,
    mut data: mpsc::Receiver<ServerFrame>,
) {
    while let Some(frame) = tokio::select! { biased;
            frame = priority.recv() => frame,
            frame = data.recv() => match frame { Some(frame) => encode(frame), None => return },
    } {
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
