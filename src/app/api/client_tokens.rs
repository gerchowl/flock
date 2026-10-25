//! Unix-socket-only local-operator credential management and browser handoff.
use super::responses::{encode_error, encode_success};
use crate::api::schema::{Method, ResponseResult};
use crate::app::App;
use crate::structured::permissions::auth::tokens::{Bearer, TokenError, TokenStore};
use std::time::Instant;

impl App {
    pub(super) fn handle_client_token(&mut self, id: String, method: Method) -> String {
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
