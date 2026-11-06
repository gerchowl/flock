use super::*;
use crate::{
    client_open::Codes,
    web::{client_listener::Listener, wire::Core},
};

#[tokio::test]
async fn import_bind_waits_for_the_exporters_native_socket_unlink() {
    let root = crate::test_support::TempDir::socket();
    let api = root.path().join("api.sock");
    let terminal = root.path().join("term.sock");
    let path = crate::web::local_listener::socket_path(&api);
    let core = Core::new(root.path().into(), Arc::new(tokio::sync::Notify::new()))
        .await
        .unwrap();
    let exporter = Listener::prepare(
        None,
        Codes::default(),
        root.path().into(),
        None,
        (path.clone(), None),
        Default::default(),
        core,
    )
    .await
    .unwrap()
    .unwrap();
    let inherited = exporter.local_owner();
    let mut unlinked = false;
    wait_for_old_public_sockets_at(api, terminal, Duration::from_secs(5), || {
        assert!(
            !unlinked,
            "the next check must observe the exporter's unlink"
        );
        assert_eq!(
            inherited.prepare(&path).unwrap_err().kind(),
            io::ErrorKind::AddrInUse
        );
        exporter.unlink_local().unwrap();
        unlinked = true;
    })
    .unwrap();
    let core = Core::new(root.path().into(), Arc::new(tokio::sync::Notify::new()))
        .await
        .unwrap();
    let importer = Listener::prepare(
        None,
        Codes::default(),
        root.path().into(),
        None,
        (path.clone(), Some(inherited)),
        Default::default(),
        core,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        unlinked,
        "import binding must wait even when API and terminal paths are absent"
    );
    exporter.shutdown().await.unwrap();
    assert!(
        path.exists(),
        "the old exporter cannot unlink the imported binding"
    );
    importer.shutdown().await.unwrap();
}
