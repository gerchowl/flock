use super::*;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(RustEmbed)]
#[folder = "tests/fixtures/gui-dist/"]
struct Fixture;

#[derive(RustEmbed)]
#[folder = "tests/fixtures/gui-dist-absent/"]
#[allow_missing = true]
struct Missing;

struct Server {
    address: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn start<A: RustEmbed + 'static>() -> Self {
        Self::with(router_for::<A>).await
    }
    async fn with(make: impl FnOnce(SocketAddr) -> io::Result<Router>) -> Self {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let app = make(address).unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { address, task }
    }
    async fn request(&self, method: &str, path: &str, extra: &str) -> (u16, HeaderMap, String) {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut socket = tokio::net::TcpStream::connect(self.address).await.unwrap();
            socket
                .write_all(
                    format!(
                        "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n{extra}\r\n",
                        self.address
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut bytes = Vec::new();
            socket.read_to_end(&mut bytes).await.unwrap();
            let text = String::from_utf8(bytes).unwrap();
            let (head, body) = text.split_once("\r\n\r\n").expect("HTTP response framing");
            let mut lines = head.split("\r\n");
            let status = lines
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap()
                .parse()
                .unwrap();
            let mut headers = HeaderMap::new();
            for line in lines {
                let (name, value) = line.split_once(':').unwrap();
                headers.append(
                    axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    value.trim().parse().unwrap(),
                );
            }
            (status, headers, body.to_owned())
        })
        .await
        .expect("owned HTTP producer must respond, even if alive and silent")
    }
}

#[tokio::test]
async fn entry_and_assets_are_real_http_with_distinct_cache_policies() {
    let server = Server::start::<Fixture>().await;
    for path in ["/", "/index.html?dogfood=1"] {
        let (status, headers, body) = server.request("GET", path, "").await;
        assert_eq!(status, 200);
        assert!(body.contains("GUI bundle fixture"));
        assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
        assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
        assert!(headers[header::ETAG].to_str().unwrap().starts_with('"'));
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    }
    for (path, mime, expected) in [
        (
            "/assets/main-AbCd-E_1.js",
            "application/javascript; charset=utf-8",
            include_str!("../../../tests/fixtures/gui-dist/assets/main-AbCd-E_1.js"),
        ),
        (
            "/assets/style-1234ABcd.css",
            "text/css; charset=utf-8",
            include_str!("../../../tests/fixtures/gui-dist/assets/style-1234ABcd.css"),
        ),
    ] {
        let (status, headers, body) = server.request("GET", path, "").await;
        assert_eq!(status, 200);
        assert_eq!(body, expected);
        assert_eq!(
            headers[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        assert_eq!(headers[header::CONTENT_TYPE], mime);
        let (_, _, head_body) = server.request("HEAD", path, "").await;
        assert!(head_body.is_empty());
    }
    let (status, headers, _) = server.request("GET", "/manifest.json", "").await;
    assert_eq!(status, 200);
    assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
}

#[tokio::test]
async fn etag_revalidation_returns_304_with_cache_metadata_and_no_body() {
    let server = Server::start::<Fixture>().await;
    for path in ["/", "/assets/main-AbCd-E_1.js"] {
        let (_, first, _) = server.request("GET", path, "").await;
        let etag = first[header::ETAG].to_str().unwrap();
        for condition in [
            etag.to_owned(),
            format!("\"unrelated\", W/{etag}"),
            "*".into(),
        ] {
            let (status, headers, body) = server
                .request("GET", path, &format!("If-None-Match: {condition}\r\n"))
                .await;
            assert_eq!(status, 304);
            assert!(body.is_empty());
            assert_eq!(headers[header::ETAG], first[header::ETAG]);
            assert_eq!(headers[header::CACHE_CONTROL], first[header::CACHE_CONTROL]);
        }
        let (status, _, _) = server
            .request("GET", path, "If-None-Match: \"different\"\r\n")
            .await;
        assert_eq!(status, 200);
    }
}

#[tokio::test]
async fn unknown_assets_navigation_and_reserved_transport_paths_never_get_html() {
    let server = Server::start::<Fixture>().await;
    for path in [
        "/assets/missing.js",
        "/assets/missing.css",
        "/agent/example",
        "/v1/ws",
        "/v1/blob/hash",
        "/v1",
        "/../index.html",
        "/assets/%2e%2e/index.html",
    ] {
        let (status, headers, body) = server
            .request("GET", path, "Accept: text/html\r\nIf-None-Match: *\r\n")
            .await;
        assert_eq!(status, 404, "{path}: {body}");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        assert!(!body.contains("GUI bundle fixture"));
    }
    assert_eq!(
        server.request("POST", "/", "Content-Length: 0\r\n").await.0,
        405
    );
}

#[tokio::test]
async fn missing_bundle_is_a_typed_uncached_refusal() {
    let server = Server::start::<Missing>().await;
    for path in ["/", "/index.html", "/assets/main-AbCd-E_1.js"] {
        let (status, headers, body) = server.request("GET", path, "").await;
        assert_eq!(status, 503);
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        let error: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            error["error"]["code"],
            ErrorCode::ServerUnavailable.as_str()
        );
        assert!(error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("FLOCK_GUI_DIST"));
    }
    assert_eq!(server.request("GET", "/v1/ws", "").await.0, 404);
}

#[tokio::test]
async fn cross_origin_opaque_and_cross_site_requests_are_refused_before_cache_lookup() {
    let server = Server::start::<Fixture>().await;
    for extra in [
        "Origin: https://foreign.example\r\n",
        "Origin: null\r\n",
        "Sec-Fetch-Site: cross-site\r\n",
        "Sec-Fetch-Site: same-site\r\n",
    ] {
        for path in ["/", "/assets/main-AbCd-E_1.js"] {
            let (status, headers, body) = server
                .request("GET", path, &format!("{extra}If-None-Match: *\r\n"))
                .await;
            assert_eq!(status, 403, "{body}");
            assert_eq!(headers[header::CACHE_CONTROL], "no-store");
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&body).unwrap()["error"]["code"],
                "origin_mismatch"
            );
            assert!(!headers.contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
        }
    }
    let origin = format!(
        "Origin: http://{}\r\nSec-Fetch-Site: same-origin\r\n",
        server.address
    );
    assert_eq!(server.request("GET", "/", &origin).await.0, 200);
    assert_eq!(
        server
            .request("GET", "/", "Sec-Fetch-Site: none\r\n")
            .await
            .0,
        200
    );
}

#[test]
fn loopback_only_constructor_does_not_bind_and_rejects_routable_or_wildcard_addresses() {
    for address in ["0.0.0.0:0", "192.0.2.1:80", "[::]:0"] {
        assert_eq!(
            router(address.parse().unwrap()).err().unwrap().kind(),
            io::ErrorKind::InvalidInput
        );
    }
    assert!(router("127.0.0.1:0".parse().unwrap()).is_ok());
    assert!(router("[::1]:0".parse().unwrap()).is_ok());
}

#[tokio::test]
async fn public_router_uses_the_build_selected_embedded_bundle() {
    let server = Server::with(router).await;
    let (status, headers, body) = server.request("GET", "/", "").await;
    match GuiAssets::get("index.html") {
        Some(file) => {
            assert_eq!(status, 200);
            assert_eq!(body.as_bytes(), file.data.as_ref());
            assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
        }
        None => {
            assert_eq!(status, 503);
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&body).unwrap()["error"]["code"],
                "server_unavailable"
            );
        }
    }
}

#[tokio::test]
async fn stateless_router_merges_after_parent_state_and_preserves_transport_routes() {
    let server = Server::with(|address| {
        let parent: Router = Router::new()
            .route(
                "/v1/fixture",
                get(|State(value): State<&'static str>| async move { value }),
            )
            .with_state("transport-owned");
        Ok(parent.merge(router_for::<Missing>(address)?))
    })
    .await;
    let (status, _, body) = server.request("GET", "/v1/fixture", "").await;
    assert_eq!(status, 200);
    assert_eq!(body, "transport-owned");
    assert_eq!(server.request("GET", "/", "").await.0, 503);
}

#[test]
fn default_http_port_has_equivalent_canonical_host_and_origin_forms() {
    for (authority, canonical) in [("127.0.0.1:80", "127.0.0.1"), ("[::1]:80", "[::1]")] {
        for host in [authority, canonical] {
            for origin in [authority, canonical] {
                let mut headers = HeaderMap::new();
                headers.insert(header::HOST, host.parse().unwrap());
                headers.insert(header::ORIGIN, format!("http://{origin}").parse().unwrap());
                assert!(origin_allowed(&headers, authority));
                assert!(!origin_allowed(&headers, "127.0.0.1:81"));
            }
        }
    }
}
