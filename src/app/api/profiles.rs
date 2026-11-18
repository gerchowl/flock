//! Authenticated local-operator binding preparation; execution remains disabled.
use super::responses;
use crate::{
    api::{
        error_code::ErrorCode,
        schema::{Method, ResponseResult},
    },
    app::App,
};

impl App {
    /// Require the authenticated local operator before any preparation effect.
    pub(super) fn handle_profile_mutation(&mut self, id: String, method: Method) -> String {
        let Some(principal) = self
            .current_api_peer_uid
            .flatten()
            .filter(|_| self.caller_is_local_operator())
        else {
            return responses::encode_error(
                id,
                ErrorCode::OperatorOnly.as_str(),
                "profile preparation requires the local operator",
            );
        };
        let result = match method {
            Method::ProfilePlan(request) => self.structured_profiles.plan_bind(principal, request),
            Method::ProfileBind(request) => {
                self.structured_profiles
                    .bind(principal, &request.plan, &request.expect)
            }
            Method::ProfileRevoke(request) => self.structured_profiles.revoke(principal, request),
            _ => Err(ErrorCode::InvalidRequest),
        };
        match result {
            Ok(data) => responses::encode_success(id, ResponseResult::ProfileTransaction { data }),
            Err(code) => responses::encode_error(id, code.as_str(), "profile transaction refused"),
        }
    }
}
