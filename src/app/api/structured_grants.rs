//! Local operator surface for S6 live grants; no WS/MCP management dispatch.
use super::responses::{encode_error, encode_success};
use crate::{
    api::{
        error_code::ErrorCode,
        schema::{EventData, EventEnvelope, EventKind, ResponseResult},
    },
    app::App,
    structured::permissions::{grants::GrantHandle, Principal},
};

impl App {
    pub(super) fn handle_structured_grants(
        &mut self,
        id: String,
        agent_id: String,
        generation: Option<u64>,
        revoke: Option<GrantHandle>,
    ) -> String {
        let principal = match (self.caller_is_local_operator(), self.current_api_peer_pid) {
            (true, Some(pid)) => format!("operator:pid:{pid}"),
            _ => {
                return encode_error(
                    id,
                    ErrorCode::OperatorOnly.as_str(),
                    "grant management requires the local operator",
                )
            }
        };
        let result = (|| {
            let (_, _, current) = self
                .state
                .structured_agents
                .get(&agent_id)
                .ok_or(ErrorCode::AgentNotFound)?;
            if generation.is_some_and(|g| g != current) {
                return Err(ErrorCode::PermissionGrantStaleGeneration);
            }
            let runtime = self
                .structured_workers
                .runtime(&agent_id, current)
                .ok_or(ErrorCode::AgentNotFound)?;
            let grants = runtime.session_grants(&Principal::Operator, revoke.as_ref())?;
            Ok((current, grants))
        })();
        match result {
            Ok((generation, grants)) => {
                if let Some(handle) = &revoke {
                    self.emit_event(EventEnvelope {
                        event: EventKind::StructuredGrantRevoked,
                        data: EventData::StructuredGrantRevoked {
                            agent_id: agent_id.clone(),
                            generation,
                            request_id: handle.request_id.clone(),
                            request_seq: handle.request_seq,
                            principal,
                        },
                    });
                }
                encode_success(
                    id,
                    ResponseResult::StructuredGrants {
                        agent_id,
                        generation,
                        grants,
                        revoked: revoke,
                    },
                )
            }
            Err(code) => encode_error(
                id,
                code.as_str(),
                "session grant operation refused; refresh the live grant list",
            ),
        }
    }
}
