//! Operate-scoped wire admission preserves ADR-0036 §1a's socket boundary.
use super::App;
use crate::{api::error_code::ErrorCode, structured_wire::AgentLifecycleCommand};
use serde_json::Value;

impl App {
    /// Called by the wire owner inside the live token's Operate admission fence.
    pub(crate) fn wire_agent_lifecycle(
        &mut self,
        command: AgentLifecycleCommand,
    ) -> Result<Value, ErrorCode> {
        // Socket attribution must never leak from a previously serviced request.
        let previous = super::super::ApiCaller {
            scope: self.current_api_scope.take(),
            pid: self.current_api_peer_pid.take(),
            birth: self.current_api_peer_birth.take(),
            uid: self.current_api_peer_uid.take(),
        };
        let result = (|| {
            let response = match command {
                AgentLifecycleCommand::Start(params) => {
                    if params.runtime == crate::api::schema::AgentRuntime::Structured
                        && self.structured_target(&params.name)?.is_some()
                    {
                        return Err(ErrorCode::OperatorOnly);
                    }
                    self.handle_agent_start(String::new(), params)
                }
                AgentLifecycleCommand::Stop(target) => {
                    if self.structured_target(&target.target)?.is_some() {
                        return Err(ErrorCode::OperatorOnly);
                    }
                    self.handle_structured_stop(String::new(), target)
                }
                AgentLifecycleCommand::Restart(params) => {
                    self.handle_agent_restart(String::new(), params)
                }
            };
            let response: Value =
                serde_json::from_str(&response).map_err(|_| ErrorCode::SerializationError)?;
            if let Some(error) = response.get("error") {
                return Err(error["code"]
                    .as_str()
                    .and_then(ErrorCode::from_code)
                    .unwrap_or(ErrorCode::InternalError));
            }
            response
                .get("result")
                .cloned()
                .ok_or(ErrorCode::SerializationError)
        })();
        self.current_api_scope = previous.scope;
        self.current_api_peer_pid = previous.pid;
        self.current_api_peer_birth = previous.birth;
        self.current_api_peer_uid = previous.uid;
        result
    }
}
