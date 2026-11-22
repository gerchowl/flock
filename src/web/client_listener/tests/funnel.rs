use super::*;
use crate::platform::tailscale::Locations;
use std::{os::unix::fs::PermissionsExt, path::Path};

fn paths(root: &Path) -> Locations {
    Locations {
        path: Vec::new(),
        bundles: vec![root.join("App/Contents/MacOS/Tailscale")],
        sockets: vec![root.join("tailscaled.socket"), root.join("tailscaled.sock")],
    }
}
fn cli(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"fixture, never executed").unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}
#[test]
fn positively_absent_tailscale_starts_listener_and_logs_checked_paths() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let root = crate::test_support::TempDir::socket();
    let evidence = paths(root.path());
    let expected_paths = evidence.checked_paths();
    let logs = crate::logging::capture_logs_across_threads(|| {
        runtime.block_on(async {
            let core = Core::new(root.path().to_owned(), Arc::new(tokio::sync::Notify::new()))
                .await
                .unwrap();
            let mut listener = bounded(Listener::prepare_with_funnel(
                Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
                Codes::default(),
                root.path().to_owned(),
                None,
                (root.path().join("native.sock"), None),
                (Health::default(), core),
                move |port| check_funnel_with(port, &evidence, |_| panic!("absence ran a CLI")),
            ))
            .await
            .unwrap()
            .unwrap();
            let address = listener.socket.as_ref().unwrap().local_addr().unwrap();
            listener.activate();
            let mut connection = bounded(tokio::net::TcpStream::connect(address))
                .await
                .unwrap();
            let request = format!(
                "GET /v1/absence-probe HTTP/1.1\r\nHost: {address}\r\nOrigin: http://{address}\r\nConnection: close\r\n\r\n"
            );
            bounded(connection.write_all(request.as_bytes()))
                .await
                .unwrap();
            let mut response = String::new();
            bounded(connection.read_to_string(&mut response))
                .await
                .unwrap();
            assert!(response.starts_with("HTTP/1.1 404"), "{response}");
            assert_eq!(listener.health.status(), Some(Status::Listening));
            bounded(listener.shutdown()).await.unwrap();
        })
    });
    let lines: Vec<_> = logs
        .lines()
        .filter(|line| line.contains("client.listener.tailscale_not_installed"))
        .collect();
    assert_eq!(lines.len(), 1, "{logs}");
    assert!(lines[0].contains("INFO"), "{logs}");
    assert!(lines[0].contains("not installed"), "{logs}");
    assert!(lines[0].contains(&expected_paths), "{logs}");
}
#[test]
fn bundle_only_cli_is_used_and_its_serve_rule_still_refuses() {
    let root = crate::test_support::TempDir::socket();
    let evidence = paths(root.path());
    cli(&evidence.bundles[0]);
    let error = check_funnel_with(45678, &evidence, |actual| {
        assert_eq!(actual, evidence.bundles[0]);
        Ok(probe_output(r#"{"TCP":{"45678":{"HTTPS":true}}}"#, 0))
    })
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert!(error.to_string().contains("serve/funnel"));
}
#[test]
fn socket_without_cli_is_not_absence_and_names_the_evidence() {
    let root = crate::test_support::TempDir::socket();
    for index in 0..2 {
        let evidence = paths(root.path());
        let socket = std::os::unix::net::UnixListener::bind(&evidence.sockets[index]).unwrap();
        let error = check_funnel_with(45678, &evidence, |_| panic!("no CLI exists")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&evidence.sockets[index].display().to_string()),
            "{error}"
        );
        assert!(error.to_string().contains("but no CLI"), "{error}");
        drop(socket);
        std::fs::remove_file(&evidence.sockets[index]).unwrap();
    }
}
#[test]
fn installed_cli_execution_json_and_exit_failures_remain_refusals() {
    let root = crate::test_support::TempDir::socket();
    let mut evidence = paths(root.path());
    evidence.path.push(root.path().join("bin/tailscale"));
    cli(&evidence.path[0]);
    cli(&evidence.bundles[0]);
    for (output, expected) in [
        (probe_output("invalid", 7), "serve status failed"),
        (probe_output("invalid", 0), "invalid tailscale status JSON"),
    ] {
        let error = check_funnel_with(45678, &evidence, |actual| {
            assert_eq!(actual, evidence.path[0]);
            Ok(output)
        })
        .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(
            error
                .to_string()
                .contains(&evidence.path[0].display().to_string()),
            "{error}"
        );
    }
    let error = check_funnel_with(45678, &evidence, |_| {
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    })
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("present but serve status unavailable"),
        "{error}"
    );
    assert!(
        error
            .to_string()
            .contains(&evidence.path[0].display().to_string()),
        "{error}"
    );
}
#[test]
fn unreadable_or_dangling_installation_evidence_is_not_absence() {
    let root = crate::test_support::TempDir::socket();
    let mut evidence = paths(root.path());
    let parent_file = root.path().join("not-a-directory");
    std::fs::write(&parent_file, b"fixture").unwrap();
    evidence.path.push(parent_file.join("tailscale"));
    let error = check_funnel_with(45678, &evidence, |_| panic!("lookup failed")).unwrap_err();
    assert!(
        error.to_string().contains("cannot inspect Tailscale CLI"),
        "{error}"
    );
    evidence.path.clear();
    std::fs::create_dir_all(evidence.bundles[0].parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(root.path().join("missing"), &evidence.bundles[0]).unwrap();
    let error =
        check_funnel_with(45678, &evidence, |_| Err(io::ErrorKind::NotFound.into())).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("present but serve status unavailable"),
        "{error}"
    );
}
