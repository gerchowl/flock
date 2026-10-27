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
                    clock.clone(),
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
                    clock.clone(),
                    Health::default(),
                )),
            )
        };
        Self {
            tcp,
            path,
            clock,
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
