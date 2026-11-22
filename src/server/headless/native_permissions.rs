//! Public thin-client test harness: no caller-supplied peer or permission authority.
use super::*;
use crate::app::App;
impl HeadlessServer {
    pub(crate) fn native_test_server(app: &mut App) -> Self {
        let mut server = super::tests::test_headless_server();
        std::mem::swap(app, &mut server.app);
        server
    }
    pub(crate) fn native_test_swap_app(&mut self, app: &mut App) {
        std::mem::swap(app, &mut self.app);
    }
    pub(crate) fn native_test_app(&mut self) -> &mut App {
        &mut self.app
    }
    pub(crate) async fn native_test_event(&mut self) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = self
                    .server_event_rx
                    .recv()
                    .await
                    .expect("thin-client event producer died");
                let significant = !matches!(event, ServerEvent::ClientWriterDrained { .. });
                self.handle_server_event(event);
                if significant {
                    break;
                }
            }
        })
        .await
        .expect("thin-client event deadline");
    }
    pub(crate) fn native_test_accept(&mut self) {
        crate::server::client_accept::accept_pending_client_connections(
            &self.client_listener,
            &mut self.next_client_id,
            &self.should_quit,
            &self.server_event_tx,
        )
        .unwrap();
    }
    pub(crate) fn native_test_listener_fd(&self) -> std::os::fd::RawFd {
        std::os::fd::AsRawFd::as_raw_fd(&self.client_listener)
    }
    pub(crate) fn native_test_socket(&self) -> &std::path::Path {
        &self.client_socket_path
    }
}
