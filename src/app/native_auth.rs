//! Connection-bound TUI capabilities, ADR-0036 S10e and ADR-0038 §6.
use super::{App, LocalPeer};
use crate::{
    api::error_code::ErrorCode,
    raw_input::RawInputEvent,
    structured::permissions::auth::{
        tokens::{app_sessions, server, Scope, TokenError},
        VerifiedToken,
    },
};
use std::{collections::HashMap, sync::Arc};

pub(super) type Clients = HashMap<u64, Result<ClientSession, ErrorCode>>;
/// Server-private credential and its accepted connection's sealed process proof.
#[derive(Clone)]
pub(super) struct ClientSession {
    store: Arc<server::Store>,
    session: server::Session,
    peer: LocalPeer,
}
fn token_code(error: TokenError) -> ErrorCode {
    match error {
        TokenError::Revoked | TokenError::Unauthorized => ErrorCode::AuthRevoked,
        TokenError::Scope => ErrorCode::Scope,
        TokenError::Busy => ErrorCode::SessionBusy,
        TokenError::Unsafe | TokenError::Io(_) => ErrorCode::InternalError,
    }
}
impl ClientSession {
    /// Revalidate the connection identity and required Operate grant at mutation time.
    pub(super) fn decide(
        &self,
        dispatch: impl for<'a> FnOnce(VerifiedToken<'a>) -> Result<(), ErrorCode>,
    ) -> Result<(), ErrorCode> {
        if !self
            .session
            .app()
            .is_some_and(|session| session.process.matches(self.peer.credentials()))
        {
            return Err(ErrorCode::PeerProcessChanged);
        }
        self.store
            .admit(&self.session, Scope::Operate, dispatch)
            .map_err(|error| match error {
                TokenError::Scope => ErrorCode::NotAuthorized,
                error => token_code(error),
            })?
    }
}
impl App {
    /// Mint only for an attested LocalOperator attach. A failed mint grants nothing.
    pub(crate) fn attach_native_client(
        &mut self,
        client: u64,
        credentials: crate::platform::peer::Credentials,
    ) {
        let issued: Result<ClientSession, ErrorCode> = (|| {
            let peer = self.admit_local_peer(credentials)?;
            let store = self
                .client_auth
                .get(
                    &crate::session::data_dir(),
                    Arc::new(app_sessions::SystemClock::new()),
                )
                .map_err(token_code)?;
            let issued = store
                .issue(&self.native_client_authority(&peer), peer.process().clone())
                .map_err(|error| match error {
                    app_sessions::IssueError::PeerProcessChanged(_) => {
                        ErrorCode::PeerProcessChanged
                    }
                    app_sessions::IssueError::Store(error) => token_code(error),
                })?;
            let session = store
                .admit_peer(&issued.token, Some(&peer), |session| session)
                .map_err(|error| match error {
                    server::PeerAdmissionError::PeerProcessChanged => ErrorCode::PeerProcessChanged,
                    server::PeerAdmissionError::Token(error) => token_code(error),
                })?;
            Ok(ClientSession {
                store,
                session,
                peer,
            })
        })();
        if let Err(code) = &issued {
            tracing::warn!(
                client,
                code = code.as_str(),
                "TUI permission capability refused"
            );
        }
        self.native_clients.insert(client, issued);
    }
    /// Detach removes the input capability before fenced retirement can fail or wait.
    pub(crate) fn detach_native_client(&mut self, client: u64) {
        if let Some(Ok(authority)) = self.native_clients.remove(&client) {
            if let Err(error) = authority.store.retire(&authority.session) {
                tracing::warn!(
                    client,
                    code = token_code(error).as_str(),
                    "TUI credential retirement failed; connection disabled"
                );
            }
        }
    }
    /// Only this connection's parsed input may use its server-private credential.
    pub(crate) fn route_native_client_events(
        &mut self,
        client: u64,
        events: Vec<RawInputEvent>,
        theme: bool,
    ) {
        self.native_input_client = Some(client);
        self.route_client_events(events, theme);
        self.native_input_client = None;
    }
    pub(super) fn native_decision_authority(&self) -> Result<ClientSession, ErrorCode> {
        let client = self.native_input_client.ok_or(ErrorCode::NotAuthorized)?;
        self.native_clients
            .get(&client)
            .ok_or(ErrorCode::NotAuthorized)?
            .clone()
    }
}

#[cfg(test)]
impl App {
    pub(crate) fn native_test_probe(&self, client: u64) -> impl Fn() -> Result<(), ErrorCode> {
        let authority = self.native_clients[&client].clone().unwrap();
        move || authority.decide(|_| Ok(()))
    }
    pub(crate) fn native_test_swap_session(&mut self, destination: u64, source: u64) {
        let credential = self.native_clients[&source]
            .as_ref()
            .unwrap()
            .session
            .clone();
        self.native_clients
            .get_mut(&destination)
            .unwrap()
            .as_mut()
            .unwrap()
            .session = credential;
    }
}

#[cfg(test)]
impl App {
    pub(crate) fn native_test_answer_only(&mut self, client: u64) {
        let authority = self
            .native_clients
            .get_mut(&client)
            .unwrap()
            .as_mut()
            .unwrap();
        authority
            .store
            .answer_only_app_for_test(&mut authority.session);
    }
}
