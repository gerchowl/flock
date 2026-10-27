//! Real hello frames under the production disk subscriber, with fake credentials only.
use super::*;
use crate::structured_wire::WIRE_JSON_VERSION;
use crate::web::wire::tests::{runtime, Received, Server, Socket};
use tokio::io::AsyncWriteExt;

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);
impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl<'a> MakeWriter<'a> for Sink {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
struct RestoreEnv(Option<std::ffi::OsString>);
impl Drop for RestoreEnv {
    fn drop(&mut self) {
        match &self.0 {
            Some(value) => std::env::set_var("FLOCK_LOG", value),
            None => std::env::remove_var("FLOCK_LOG"),
        }
    }
}

#[test]
fn bearer_never_reaches_trace_log_through_real_hello() {
    let _restore = RestoreEnv(std::env::var_os("FLOCK_LOG"));
    for directive in [
        "trace",
        "tungstenite=trace",
        "tungstenite=trace,flk=trace",
        "tungstenite::protocol=trace,tokio_tungstenite=trace,flk=trace",
    ] {
        std::env::set_var("FLOCK_LOG", directive);
        let sink = Sink::default();
        // set_default also installs the log bridge used by tungstenite. Every socket
        // task runs on this thread; the fixture owns its listener and fake store.
        let guard = file_log_subscriber(sink.clone()).set_default();
        runtime(async {
            let server = Server::new().await;
            let mut socket = Socket::open(&server).await;
            let welcome = socket.hello(WIRE_JSON_VERSION, &"a".repeat(64)).await;
            assert_eq!(welcome["principal"], "c".repeat(32));
            tracing::trace!(
                event = "secret_test.trace_control",
                "flk trace remains available"
            );
            // The exact dependency-only directive intentionally excludes flk. This
            // payload-free control proves its permitted DEBUG events still work.
            tracing::debug!(target: "tungstenite", event = "secret_test.debug_control", "dependency debug remains available");
            // Close reasons are peer-controlled payload too, logged at DEBUG upstream.
            let token = "a".repeat(64);
            let mut close = vec![0x88, 0x80 | 66, 0, 0, 0, 0, 3, 232];
            close.extend_from_slice(token.as_bytes());
            socket.0.write_all(&close).await.unwrap();
            // The core may end the stream without flushing tungstenite's auto-reply.
            let ended = socket.receive().await;
            assert!(
                matches!(ended, Received::Close(_) | Received::End),
                "{ended:?}"
            );
            server.finish().await;
        });
        drop(guard);
        let captured = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        assert!(!captured.is_empty(), "empty capture for {directive}");
        if directive == "tungstenite=trace" {
            assert!(captured.contains("secret_test.debug_control"), "{captured}");
        } else {
            assert!(captured.contains("secret_test.trace_control"), "{captured}");
        }
        assert!(
            !captured.contains(&"a".repeat(64)),
            "bearer leaked for {directive}:\n{captured}"
        );
    }
}
