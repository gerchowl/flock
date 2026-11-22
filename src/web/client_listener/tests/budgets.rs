use super::*;
use std::{path::PathBuf, sync::Arc};
use tokio::io::{AsyncRead, AsyncWrite};
trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
type Stream = Box<dyn Io>;
struct Pool {
    tcp: Option<SocketAddr>,
    path: PathBuf,
    clock: Arc<TestClock>,
    completed: Arc<Semaphore>,
    task: JoinHandle<io::Result<()>>,
    _root: crate::test_support::TempDir,
}
impl Drop for Pool {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Pool {
    async fn new(tcp: bool) -> Self {
        let root = crate::test_support::TempDir::socket();
        let path = root.path().join("http.sock");
        let clock = Arc::new(TestClock::new());
        let completed = Arc::new(Semaphore::new(0));
        let transport_clock = Arc::new(CompletionClock {
            clock: clock.clone(),
            completed: completed.clone(),
        });
        let router = Router::new().route("/", axum::routing::get(|| async { "alive" }));
        let (tcp, task) = if tcp {
            let socket = Arc::new(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap());
            let address = socket.local_addr().unwrap();
            (
                Some(address),
                tokio::spawn(serve(
                    move || {
                        let socket = socket.clone();
                        async move {
                            socket
                                .accept()
                                .await
                                .map(|(s, _)| (Box::new(s) as Stream, ()))
                        }
                    },
                    router,
                    transport_clock.clone(),
                    Health::default(),
                )),
            )
        } else {
            let socket = Arc::new(tokio::net::UnixListener::bind(&path).unwrap());
            (
                None,
                tokio::spawn(serve(
                    move || {
                        let socket = socket.clone();
                        async move {
                            socket
                                .accept()
                                .await
                                .map(|(s, _)| (Box::new(s) as Stream, ()))
                        }
                    },
                    router,
                    transport_clock.clone(),
                    Health::default(),
                )),
            )
        };
        Self {
            tcp,
            path,
            clock,
            completed,
            task,
            _root: root,
        }
    }
    async fn connect(&self) -> Stream {
        match self.tcp {
            Some(address) => Box::new(
                bounded(tokio::net::TcpStream::connect(address))
                    .await
                    .unwrap(),
            ),
            None => Box::new(
                bounded(tokio::net::UnixStream::connect(&self.path))
                    .await
                    .unwrap(),
            ),
        }
    }
    async fn response(&self) -> String {
        let mut socket = self.connect().await;
        bounded(
            socket
                .write_all(b"GET / HTTP/1.1\r\nHost: fixture.example\r\nConnection: close\r\n\r\n"),
        )
        .await
        .unwrap();
        let mut header = Vec::new();
        bounded(async {
            while !header.ends_with(b"\r\n\r\n") {
                header.push(socket.read_u8().await.unwrap());
                assert!(header.len() < 8192);
            }
        })
        .await;
        let mut text = String::from_utf8(header).unwrap();
        let len: usize = text
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().unwrap())
            })
            .unwrap();
        let mut body = vec![0; len];
        bounded(socket.read_exact(&mut body)).await.unwrap();
        text.push_str(std::str::from_utf8(&body).unwrap());
        assert_eq!(bounded(socket.read(&mut [0; 1])).await.unwrap(), 0);
        text
    }
}
#[tokio::test]
async fn http_pools_are_independent_bounded_and_release_at_the_injected_deadline() {
    for tcp_first in [true, false] {
        let full = Pool::new(tcp_first).await;
        let other = Pool::new(!tcp_first).await;
        let mut pending = Vec::new();
        for _ in 0..HTTP_CONNECTIONS {
            let mut stream = full.connect().await;
            stream.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
            pending.push(stream);
        }
        full.clock.deadline_count(5_000, HTTP_CONNECTIONS).await;
        let response = full.response().await;
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();
        assert!(headers.starts_with("HTTP/1.1 503"), "{response}");
        assert!(headers.contains("Content-Type: application/json"));
        assert!(headers.contains("Cache-Control: no-store"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).unwrap(),
            serde_json::json!({"error":{"code":"preauth_capacity","message":"Pre-authentication capacity exhausted"}})
        );
        assert!(other.response().await.ends_with("alive"));
        full.clock.advance(5_000);
        for mut stream in pending {
            let mut bytes = Vec::new();
            // A rejected partial HTTP header may be reset by the kernel after close.
            let result = bounded(stream.read_to_end(&mut bytes)).await;
            assert!(result.is_ok() || result.unwrap_err().kind() == io::ErrorKind::ConnectionReset);
        }
        assert!(full.response().await.ends_with("alive"));
    }
}

#[tokio::test]
async fn unclosed_over_cap_refusals_cannot_stall_legitimate_acceptance() {
    for tcp in [true, false] {
        let pool = Pool::new(tcp).await;
        let initial = pool.clock.now();
        let mut pending = Vec::new();
        for _ in 0..HTTP_CONNECTIONS {
            pending.push(pool.connect().await);
        }
        pool.clock.deadline_count(5_000, HTTP_CONNECTIONS).await;
        let mut refused = Vec::new();
        // Hold each client's write side open even after the server half-closes its reply.
        for _ in 0..HTTP_REFUSALS {
            let mut socket = pool.connect().await;
            let mut response = String::new();
            bounded(socket.read_to_string(&mut response)).await.unwrap();
            assert!(response.starts_with("HTTP/1.1 503"), "{response}");
            refused.push(socket);
        }
        // A saturated refusal budget cannot allocate another writer or stall acceptance.
        let mut excess = pool.connect().await;
        let closed = bounded(excess.read(&mut [0; 1])).await;
        assert!(
            matches!(closed, Ok(0))
                || closed.is_err_and(|e| e.kind() == io::ErrorKind::ConnectionReset)
        );
        drop(excess);
        // Real EOF releases the normal slots without releasing a single refusal writer.
        drop(pending);
        let released = bounded(pool.completed.acquire_many(HTTP_CONNECTIONS as u32))
            .await
            .unwrap();
        released.forget();
        assert!(pool.response().await.ends_with("alive"));
        assert_eq!(pool.clock.now(), initial);
        drop(refused);
    }
}

struct CompletionClock {
    clock: Arc<TestClock>,
    completed: Arc<Semaphore>,
}
struct Completed(Arc<Semaphore>);
impl Drop for Completed {
    fn drop(&mut self) {
        self.0.add_permits(1);
    }
}
impl Clock for CompletionClock {
    fn now(&self) -> std::time::Instant {
        self.clock.now()
    }
    fn until(&self, end: std::time::Instant) -> futures_util::future::BoxFuture<'_, ()> {
        let wait = self.clock.until(end);
        let completed = self.completed.clone();
        Box::pin(async move {
            let _completed = Completed(completed);
            wait.await
        })
    }
}
