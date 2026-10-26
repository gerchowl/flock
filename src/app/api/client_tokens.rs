//! Unix-socket-only local-operator credential management and browser handoff.
use super::responses::{encode_error, encode_success};
use crate::api::schema::{Method, ResponseResult};
use crate::app::App;
use crate::structured::permissions::auth::tokens::{Bearer, TokenError, TokenStore};
use std::time::Instant;

impl App {
    pub(super) fn handle_client_token(&mut self, id: String, method: Method) -> String {
        if let Method::ClientAppMint(params) = &method {
            if !params.fields.is_empty() {
                return encode_error(
                    id,
                    "invalid_params",
                    "app mint takes no parameters; scopes and peer identity are server-chosen",
                );
            }
            if self
                .runtime_config
                .uid_policy
                .classify(self.current_api_peer_uid.flatten())
                != crate::structured::identity::Principal::Operator
                || self.current_api_peer_pid.is_none()
            {
                return encode_error(
                    id,
                    "operator_only",
                    "app mint requires a kernel-attested local operator",
                );
            }
            let peer = match self.admit_local_peer(self.api_peer_credentials()) {
                Ok(peer) => peer,
                Err(code) => {
                    return encode_error(
                        id,
                        code.as_str(),
                        "app mint requires a live local operator",
                    )
                }
            };
            let owner = self.client_auth.clone();
            let authority = match self.client_token_authority() {
                Ok(a) => a,
                Err(code) => {
                    return encode_error(id, code.as_str(), "app mint requires the local operator")
                }
            };
            let result = owner
                .get(
                    &crate::session::data_dir(),
                    std::sync::Arc::new(
                        crate::structured::permissions::auth::tokens::app_sessions::SystemClock,
                    ),
                )
                .map_err(
                    crate::structured::permissions::auth::tokens::app_sessions::IssueError::Store,
                )
                .and_then(|store| store.issue(&authority, peer.process().clone()));
            return match result {
                Ok(issued) => encode_success(id, ResponseResult::ClientTokenIssued { issued }),
                Err(e) => {
                    use crate::structured::permissions::auth::tokens::app_sessions::IssueError;
                    let code = e.peer_code().unwrap_or(match &e {
                        IssueError::Store(TokenError::Busy) => {
                            crate::api::error_code::ErrorCode::SessionBusy
                        }
                        _ => crate::api::error_code::ErrorCode::InternalError,
                    });
                    let detail = match e {
                        IssueError::Store(e) => e.to_string(),
                        IssueError::PeerProcessChanged(e) => e.to_string(),
                    };
                    encode_error(
                        id,
                        code.as_str(),
                        format!("app credential refused: {detail}"),
                    )
                }
            };
        }
        let codes = self.client_codes.clone();
        let endpoint =
            crate::client_open::endpoint(codes.bound_endpoint().or(self.state.config.client.bind));
        let authority = match self.client_token_authority() {
            Ok(authority) => authority,
            Err(code) => {
                return encode_error(
                    id,
                    code.as_str(),
                    "client credentials require the local operator",
                )
            }
        };
        let result = (|| {
            let store = TokenStore::open(&crate::session::data_dir())?;
            match method {
                Method::ClientTokenAdd(params) => store
                    .issue(&authority, &params.name, params.scopes)
                    .map(|issued| ResponseResult::ClientTokenIssued { issued }),
                Method::ClientTokenRemove(params) => {
                    match (params.name, params.entry) {
                        (Some(name), None) => store.revoke(&authority, &name)?,
                        (None, Some(entry)) => store.revoke_entry(&authority, &entry)?,
                        _ => return Err(TokenError::Unsafe),
                    }
                    Ok(ResponseResult::Ok {})
                }
                Method::ClientTokenList(_) => store
                    .list(&authority)
                    .map(|entries| ResponseResult::ClientTokenList { entries }),
                Method::ClientOpen(_) => {
                    let endpoint = endpoint.ok_or(TokenError::Unsafe)?;
                    let token = store.operator_token(&authority)?;
                    let code = store.random::<32>()?;
                    let url = Bearer(format!("{endpoint}#code={code}"));
                    codes.mint(code, token, Instant::now())?;
                    Ok(ResponseResult::ClientOpen { url })
                }
                _ => Err(TokenError::Unsafe),
            }
        })();
        match result {
            Ok(result) => encode_success(id, result),
            Err(TokenError::Unsafe) => encode_error(
                id,
                "invalid_params",
                "invalid client name, scopes, unsafe storage, or missing [client] loopback bind",
            ),
            Err(_) => encode_error(
                id,
                "internal_error",
                "client credential operation unavailable; no credential was disclosed",
            ),
        }
    }
}
