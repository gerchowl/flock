//! Bounded encoded control delivery, independent of socket write progress.
use super::*;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SendError, SyncSender, TryRecvError};

/// Maximum queued encoded control bytes, excluding the frame being written.
/// This admits a full-size control frame without letting trickling readers grow
/// an unlimited backlog. Render frames retain their separate one-frame queue.
pub(in crate::server) const OBSERVER_CONTROL_BACKLOG_BYTES: usize = 4 * 1024 * 1024;
/// The encoder's 2 MiB payload ceiling plus its four-byte length prefix.
/// An oversized frame disconnects the observer, even if its queue is empty.
const OBSERVER_CONTROL_FRAME_BYTES: usize = MAX_FRAME_SIZE + 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LatestControl {
    WindowTitle,
    MouseCapture,
    ReloadSoundConfig,
}
type Pending = (Option<LatestControl>, Vec<u8>);
#[derive(Debug, Default)]
struct Queue {
    frames: VecDeque<Pending>,
    bytes: usize,
    closed: bool,
}
impl Queue {
    fn pop(&mut self) -> Option<Vec<u8>> {
        let (_, frame) = self.frames.pop_front()?;
        self.bytes -= frame.len().max(1);
        Some(frame)
    }
    fn close(&mut self) {
        self.closed = true;
        self.frames.clear();
        self.bytes = 0;
    }
}

/// Nonblocking control delivery. Overflow closes the socket and discards pending
/// controls, including any `ServerShutdown`. The peer observes transport loss.
#[derive(Clone, Debug)]
pub(in crate::server) struct ObserverControl {
    wake: SyncSender<()>,
    queue: Arc<Mutex<Queue>>,
    socket: Option<Arc<UnixStream>>,
    client_id: u64,
}
impl ObserverControl {
    /// Create a byte-bounded queue with one coalesced wake notification.
    pub(in crate::server) fn channel(
        socket: Option<UnixStream>,
        client_id: u64,
    ) -> (Self, ControlReceiver) {
        let (wake, receiver) = std::sync::mpsc::sync_channel(1);
        let queue = Arc::new(Mutex::new(Queue::default()));
        (
            Self {
                wake,
                queue: queue.clone(),
                socket: socket.map(Arc::new),
                client_id,
            },
            ControlReceiver {
                wake: receiver,
                queue,
            },
        )
    }
    /// Queue an ordered control without blocking the server.
    pub(in crate::server) fn send(&self, frame: Vec<u8>) -> Result<(), SendError<Vec<u8>>> {
        self.enqueue(frame, None)
    }
    /// Coalesce `SetWindowTitle`, `MouseCapture`, and `ReloadSoundConfig`: only
    /// their latest pending value matters. Notifications and clipboard writes
    /// retain order and are never coalesced.
    pub(in crate::server) fn send_message(
        &self,
        message: &ServerMessage,
        frame: Vec<u8>,
    ) -> Result<(), SendError<Vec<u8>>> {
        let kind = match message {
            ServerMessage::SetWindowTitle { .. } => Some(LatestControl::WindowTitle),
            ServerMessage::MouseCapture { .. } => Some(LatestControl::MouseCapture),
            ServerMessage::ReloadSoundConfig => Some(LatestControl::ReloadSoundConfig),
            _ => None,
        };
        self.enqueue(frame, kind)
    }
    fn enqueue(
        &self,
        frame: Vec<u8>,
        kind: Option<LatestControl>,
    ) -> Result<(), SendError<Vec<u8>>> {
        let mut queue = self.queue.lock();
        if queue.closed {
            return Err(SendError(frame));
        }
        if let Some(index) =
            kind.and_then(|kind| queue.frames.iter().position(|(k, _)| *k == Some(kind)))
        {
            if let Some((_, previous)) = queue.frames.remove(index) {
                queue.bytes -= previous.len().max(1);
            }
        }
        let cost = frame.len().max(1);
        if frame.len() <= OBSERVER_CONTROL_FRAME_BYTES
            && cost <= OBSERVER_CONTROL_BACKLOG_BYTES - queue.bytes
        {
            queue.bytes += cost;
            queue.frames.push_back((kind, frame));
            let _ = self.wake.try_send(());
            return Ok(());
        }
        queue.close();
        drop(queue);
        let _ = self.wake.try_send(());
        crate::logging::client_conn_control_overflow(self.client_id);
        if let Some(socket) = &self.socket {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
        Err(SendError(frame))
    }
}

/// Single consumer that releases byte reservations before writing to the socket.
/// The wake channel owns sender-lifetime detection, independently of queue data.
#[derive(Debug)]
pub(in crate::server) struct ControlReceiver {
    wake: Receiver<()>,
    queue: Arc<Mutex<Queue>>,
}
impl ControlReceiver {
    /// Take the next frame and release its byte reservation without waiting.
    pub(in crate::server) fn try_recv(&self) -> Result<Vec<u8>, TryRecvError> {
        loop {
            let mut queue = self.queue.lock();
            if let Some(frame) = queue.pop() {
                return Ok(frame);
            }
            if queue.closed {
                return Err(TryRecvError::Disconnected);
            }
            drop(queue);
            self.wake.try_recv()?;
        }
    }
    /// Wait at most the supplied duration, including stale wake notifications.
    pub(in crate::server) fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<Vec<u8>, RecvTimeoutError> {
        let start = Instant::now();
        loop {
            match self.try_recv() {
                Ok(frame) => return Ok(frame),
                Err(TryRecvError::Disconnected) => return Err(RecvTimeoutError::Disconnected),
                Err(TryRecvError::Empty) => self
                    .wake
                    .recv_timeout(timeout.saturating_sub(start.elapsed()))?,
            }
        }
    }
    /// Test-only blocking receive with normal sender-disconnect semantics.
    #[cfg(test)]
    pub(in crate::server) fn recv(&self) -> Result<Vec<u8>, std::sync::mpsc::RecvError> {
        loop {
            match self.try_recv() {
                Ok(frame) => return Ok(frame),
                Err(TryRecvError::Disconnected) => return Err(std::sync::mpsc::RecvError),
                Err(TryRecvError::Empty) => self.wake.recv()?,
            }
        }
    }
    /// Drain the currently available frames in tests.
    #[cfg(test)]
    pub(in crate::server) fn try_iter(&self) -> impl Iterator<Item = Vec<u8>> + '_ {
        std::iter::from_fn(|| self.try_recv().ok())
    }
}
impl Drop for ControlReceiver {
    fn drop(&mut self) {
        self.queue.lock().close();
    }
}

/// Control sources consumed by the production socket arbitration loop.
pub(in crate::server) trait ControlMessages {
    /// Take a pending encoded frame without waiting.
    fn try_recv(&self) -> Result<Vec<u8>, TryRecvError>;
    /// Wait for an encoded frame or sender closure within the supplied limit.
    fn recv_timeout(&self, timeout: Duration) -> Result<Vec<u8>, RecvTimeoutError>;
}
impl ControlMessages for ControlReceiver {
    fn try_recv(&self) -> Result<Vec<u8>, TryRecvError> {
        self.try_recv()
    }
    fn recv_timeout(&self, timeout: Duration) -> Result<Vec<u8>, RecvTimeoutError> {
        self.recv_timeout(timeout)
    }
}
// Direct channels exercise arbitration independently of the production backlog.
#[cfg(test)]
impl ControlMessages for Receiver<Vec<u8>> {
    fn try_recv(&self) -> Result<Vec<u8>, TryRecvError> {
        self.try_recv()
    }
    fn recv_timeout(&self, timeout: Duration) -> Result<Vec<u8>, RecvTimeoutError> {
        self.recv_timeout(timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dequeuing_releases_bytes_and_last_sender_drop_finishes_after_drain() {
        let (sender, receiver) = ObserverControl::channel(None, 1);
        sender.send(vec![0; MAX_FRAME_SIZE]).unwrap();
        sender.send(vec![1; MAX_FRAME_SIZE]).unwrap();
        assert_eq!(receiver.try_recv().unwrap().len(), MAX_FRAME_SIZE);
        sender.send(vec![2; MAX_FRAME_SIZE]).unwrap();
        drop(sender);
        assert_eq!(receiver.try_iter().count(), 2);
        assert!(matches!(
            receiver.try_recv(),
            Err(TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn latest_controls_coalesce_while_notifications_stay_ordered() {
        let (sender, receiver) = ObserverControl::channel(None, 1);
        for index in 0..12 {
            for message in [
                ServerMessage::SetWindowTitle {
                    title: format!("title-{index}"),
                },
                ServerMessage::MouseCapture {
                    enabled: index % 2 == 1,
                },
                ServerMessage::ReloadSoundConfig,
            ] {
                let mut bytes = Vec::new();
                protocol::write_message(&mut bytes, &message).unwrap();
                sender.send_message(&message, bytes).unwrap();
            }
        }
        let mut messages = receiver.try_iter().map(|bytes| {
            protocol::read_message::<_, ServerMessage>(&mut &bytes[..], MAX_FRAME_SIZE).unwrap()
        });
        assert!(
            matches!(messages.next(), Some(ServerMessage::SetWindowTitle { title }) if title == "title-11")
        );
        assert!(matches!(
            messages.next(),
            Some(ServerMessage::MouseCapture { enabled: true })
        ));
        assert!(matches!(
            messages.next(),
            Some(ServerMessage::ReloadSoundConfig)
        ));
        assert!(messages.next().is_none());
        for index in 0..3 {
            let message = ServerMessage::Notify {
                kind: protocol::NotifyKind::Toast,
                message: index.to_string(),
                body: None,
            };
            let mut bytes = Vec::new();
            protocol::write_message(&mut bytes, &message).unwrap();
            sender.send_message(&message, bytes).unwrap();
        }
        for (index, bytes) in receiver.try_iter().enumerate() {
            assert!(
                matches!(protocol::read_message::<_, ServerMessage>(&mut &bytes[..], MAX_FRAME_SIZE).unwrap(), ServerMessage::Notify { message, .. } if message == index.to_string())
            );
        }
    }
}
