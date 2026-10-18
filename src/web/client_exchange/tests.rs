use super::*;
use crate::api::client_tokens::Bearer;
use crate::structured::launch::SessionConfigDir;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn request(bind: SocketAddr, code: &str, extra: &str, origin: &str) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut stream = tokio::net::TcpStream::connect(bind).await.unwrap();
        let body = serde_json::json!({"code":code}).to_string();
        let request = format!("POST /v1/client/exchange HTTP/1.1\r\nHost: {bind}\r\nOrigin: {origin}\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut bytes = Vec::new(); stream.read_to_end(&mut bytes).await.unwrap();
        String::from_utf8(bytes).unwrap()
    }).await.expect("exchange HTTP producer stayed silent")
}

#[test]
fn actual_http_exchange_is_single_use_uniform_origin_guarded_and_secret_free() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let dir = root.path().join("client-tokens");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let token = "a".repeat(64);
    let code = "b".repeat(64);
    fs::write(dir.join("operator.token"), serde_json::json!({"token":token,"principal":"c".repeat(32),"scopes":["read","send","answer","operate"]}).to_string()).unwrap();
    fs::set_permissions(
        dir.join("operator.token"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let codes = Codes::default();
    let now = Instant::now();
    codes
        .mint(code.clone(), Bearer(token.clone()), now)
        .unwrap();
    codes
        .mint(
            "expired".into(),
            Bearer(token.clone()),
            now - Duration::from_secs(60),
        )
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let logs = crate::logging::capture_logs(|| {
        runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let bind = listener.local_addr().unwrap();
            let origin = format!("http://{bind}");
            let app = router(codes.clone(), root.path().into(), bind).unwrap();
            let (done, stop) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .with_graceful_shutdown(async {
                    let _ = stop.await;
                })
                .await
                .unwrap();
            });
            for (foreign, extra) in [
                ("http://example.test", ""),
                ("null", ""),
                (origin.as_str(), "Origin: http://example.test\r\n"),
                (origin.as_str(), "Host: example.test\r\n"),
                (origin.as_str(), "Sec-Fetch-Site: cross-site\r\n"),
            ] {
                let refused = request(bind, &code, extra, foreign).await;
                assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
                assert!(!refused.contains(&code));
                assert!(!refused.contains(&token));
            }
            let (first, second) = tokio::join!(
                request(bind, &code, "", &origin),
                request(bind, &code, "", &origin),
            );
            let responses = [first, second];
            assert_eq!(
                responses
                    .iter()
                    .filter(|r| r.starts_with("HTTP/1.1 200"))
                    .count(),
                1
            );
            assert_eq!(
                responses
                    .iter()
                    .filter(|r| r.starts_with("HTTP/1.1 401"))
                    .count(),
                1
            );
            let good = responses
                .iter()
                .find(|r| r.starts_with("HTTP/1.1 200"))
                .unwrap();
            assert!(good.contains("cache-control: no-store"));
            let json: serde_json::Value =
                serde_json::from_str(good.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(json, serde_json::json!({"token":token}));
            let mut failures = Vec::new();
            for attempt in [&code, "expired", "unknown"] {
                let response = request(bind, attempt, "", &origin).await;
                assert!(response.starts_with("HTTP/1.1 401"), "{response}");
                assert!(response.contains("cache-control: no-store"));
                failures.push(response.split_once("\r\n\r\n").unwrap().1.to_owned());
            }
            assert!(failures.iter().all(|body| body == &failures[0]));
            codes
                .mint("revoked".into(), Bearer(token.clone()), Instant::now())
                .unwrap();
            fs::remove_file(dir.join("operator.token")).unwrap();
            assert!(request(bind, "revoked", "", &origin)
                .await
                .starts_with("HTTP/1.1 401"));
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, bind.to_string().parse().unwrap());
            headers.insert(header::ORIGIN, origin.parse().unwrap());
            let response = exchange(
                State((codes.clone(), root.path().into(), origin.clone())),
                ConnectInfo("192.0.2.1:1234".parse().unwrap()),
                headers,
                Ok(axum::body::Bytes::new()),
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            tracing::debug!("exchange fixture completed");
            done.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(5), server)
                .await
                .unwrap()
                .unwrap();
        })
    });
    assert!(logs.contains("exchange fixture completed"));
    assert!(!logs.contains(&token));
    assert!(!logs.contains(&code));
}

#[test]
fn router_refuses_non_loopback_and_ephemeral_bind() {
    for bind in ["0.0.0.0:8080", "192.0.2.1:8080", "127.0.0.1:0"] {
        assert!(router(
            Codes::default(),
            "/tmp/exchange-unused".into(),
            bind.parse().unwrap()
        )
        .is_err());
    }
}

#[test]
fn unix_socket_client_open_produces_a_redeemable_code_in_the_mounted_router() {
    use crate::api::{
        schema::{EmptyParams, Method, Request},
        EventHub,
    };
    use crate::structured::identity::UidPolicy;
    use std::{
        io::{BufRead, BufReader, Write},
        os::unix::net::UnixStream,
        sync::{atomic::AtomicBool, Arc, Mutex},
    };
    struct Environment(Vec<(String, Option<std::ffi::OsString>)>);
    impl Drop for Environment {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
    let _lock = crate::config::test_config_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let mut keys: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| k.starts_with("FLOCK_"))
        .collect();
    keys.extend(
        [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_RUNTIME_DIR",
        ]
        .map(str::to_owned),
    );
    let _environment = Environment(
        keys.iter()
            .map(|key| (key.clone(), std::env::var_os(key)))
            .collect(),
    );
    for key in &keys {
        std::env::remove_var(key);
        if !key.starts_with("FLOCK_") {
            let path = root.path().join(key);
            fs::create_dir(&path).unwrap();
            std::env::set_var(key, path);
        }
    }
    let data = crate::session::data_dir();
    fs::create_dir_all(&data).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bind = listener.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = crate::config::Config::default();
        config.client.bind = Some(bind);
        let mut app = crate::app::App::new(
            &config,
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            EventHub::default(),
        );
        app.runtime_config.uid_policy = UidPolicy {
            operator: crate::platform::peer::effective_uid(),
            dedicated: Ok(None).into(),
        };
        let http = router(app.client_codes.clone(), data.clone(), bind).unwrap();
        let (done, stop) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                http.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = stop.await;
            })
            .await
            .unwrap();
        });
        let (socket, mut client) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let worker = std::thread::spawn(move || {
            crate::api::server::handle_connection(
                socket,
                &tx,
                &EventHub::default(),
                &Arc::new(AtomicBool::new(true)),
                None,
                &Mutex::default(),
            )
            .unwrap();
        });
        writeln!(
            client,
            "{}",
            serde_json::to_string(&Request {
                id: "browser-producer-fixture".into(),
                method: Method::ClientOpen(EmptyParams {})
            })
            .unwrap()
        )
        .unwrap();
        let message = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .expect("socket producer closed before client.open");
        app.current_api_peer_pid = message.peer_pid;
        app.current_api_peer_birth = Some(message.peer_birth);
        app.current_api_peer_uid = Some(message.peer_uid);
        let reply = app.handle_api_request(message.request);
        message.respond_to.send(reply).unwrap();
        let mut reply = String::new();
        BufReader::new(client).read_line(&mut reply).unwrap();
        worker.join().unwrap();
        let value: serde_json::Value = serde_json::from_str(&reply).unwrap();
        let url = value["result"]["url"]
            .as_str()
            .expect("operator socket must produce a URL");
        let code = url.strip_prefix(&format!("http://{bind}/#code=")).unwrap();
        let response = request(bind, code, "", &format!("http://{bind}")).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let token: serde_json::Value =
            serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
        let bearer: Bearer = serde_json::from_value(token["token"].clone()).unwrap();
        let session = TokenStore::open(&data)
            .unwrap()
            .authenticate(&bearer)
            .unwrap();
        assert_eq!(session.info.name, "operator");
        assert!(session
            .info
            .scopes
            .contains(&crate::api::client_tokens::Scope::Operate));
        assert!(request(bind, code, "", &format!("http://{bind}"))
            .await
            .starts_with("HTTP/1.1 401"));
        done.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    });
}
