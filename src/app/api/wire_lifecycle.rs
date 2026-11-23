//! Shared local-session and claim-bound lifecycle dispatch (ADR-0043 Q9).
use super::App;
use crate::structured::permissions::auth::lifecycle::{restart_start, Authority};
use crate::{api::error_code::ErrorCode, structured_wire::AgentLifecycleCommand};
use serde_json::Value;

impl App {
    /// Resolve only stable local membership and apply the handler's spawn policy.
    /// The destination transaction retains exclusive App ownership after this cut.
    pub(crate) fn gateway_lifecycle_member(
        &mut self,
        body: &crate::mesh::command::Body,
    ) -> Result<String, crate::structured::permissions::auth::gateway::lifecycle::Refusal> {
        use crate::{
            mesh::command::Target, structured::permissions::auth::gateway::lifecycle::Refusal,
        };
        let workspace = match &body.target {
            Target::Workspace(id) if body.verb == "agent.start" => {
                self.granted_destination(id, None)
                    .map_err(|_| Refusal::Target)?;
                id.clone()
            }
            Target::Agent(id) if body.verb != "agent.start" => {
                if self.state.structured_agents.get(id).is_none() {
                    return Err(Refusal::Target);
                }
                self.state
                    .workspaces
                    .iter()
                    .find(|w| self.granted_destination(&w.id, Some(id)).is_ok())
                    .map(|w| w.id.clone())
                    .ok_or(Refusal::Target)?
            }
            _ => return Err(Refusal::Target),
        };
        if matches!(body.verb.as_str(), "agent.start" | "agent.restart") {
            let caller = super::spawn::SpawnCaller::Structured {
                agent_id: Some(
                    crate::structured::permissions::auth::lifecycle::RemoteSubject {
                        home: body.home_node_id.clone(),
                        principal: body.principal.clone(),
                    }
                    .parent_key(),
                ),
                depth: 0,
            };
            self.admit_spawn(crate::spawn::SpawnVerb::Fork, &caller, None)
                .map_err(|_| Refusal::Verb)?;
        }
        Ok(workspace)
    }

    /// Enter with a fenced local session or a newly committed grant capability.
    pub(crate) fn wire_agent_lifecycle(
        &mut self,
        command: AgentLifecycleCommand,
        authority: Authority<'_>,
    ) -> Result<Value, ErrorCode> {
        if let Authority::Grant(grant) = &authority {
            grant.check(self.node_id.as_deref(), &command)?;
        }
        let remote = authority.remote_subject();
        let caller = remote
            .as_ref()
            .map(|subject| super::spawn::SpawnCaller::Structured {
                agent_id: Some(subject.parent_key()),
                depth: 0,
            });
        // Socket attribution must never leak from a previously serviced request.
        let previous = super::super::ApiCaller {
            scope: self.current_api_scope.take(),
            pid: self.current_api_peer_pid.take(),
            birth: self.current_api_peer_birth.take(),
            uid: self.current_api_peer_uid.take(),
        };
        let app_operator = authority.app_operator();
        let result = (|| {
            let response = match command {
                AgentLifecycleCommand::Start(params) => {
                    if params.runtime == crate::api::schema::AgentRuntime::Structured
                        && self.structured_target(&params.name)?.is_some()
                    {
                        return Err(ErrorCode::OperatorOnly);
                    }
                    if let Some(caller) = &caller {
                        if params.runtime != crate::api::schema::AgentRuntime::Structured {
                            return Err(ErrorCode::RuntimeUnsupported);
                        }
                        let result = self.start_structured_request_authenticated(
                            params,
                            caller,
                            crate::spawn::SpawnVerb::Fork,
                            None,
                            Some(&authority),
                        )?;
                        super::responses::encode_success(String::new(), result)
                    } else {
                        self.handle_agent_start(String::new(), params)
                    }
                }
                AgentLifecycleCommand::Stop(target) => {
                    if self.structured_target(&target.target)?.is_some() {
                        if !app_operator && remote.is_none() {
                            return Err(ErrorCode::OperatorOnly);
                        }
                        self.handle_structured_stop_authorized(
                            String::new(),
                            target,
                            caller.unwrap_or(super::spawn::SpawnCaller::Operator { pane: None }),
                            Some(&authority),
                        )
                    } else {
                        if remote.is_some() {
                            return Err(ErrorCode::AgentNotFound);
                        }
                        self.handle_structured_stop(String::new(), target)
                    }
                }
                AgentLifecycleCommand::Restart(params) => {
                    if (app_operator || remote.is_some())
                        && self.structured_target(&params.target)?.is_some()
                    {
                        if params.when != "after_turn"
                            || params.reason.trim().is_empty()
                            || params.reason.len() > 1024
                            || params.reason.chars().any(char::is_control)
                        {
                            return Err(ErrorCode::InvalidParams);
                        }
                        if params.continue_with.is_some() {
                            return Err(ErrorCode::RuntimeUnsupported);
                        }
                        let start = restart_start(&params.target)?;
                        let result = self.start_structured_request_authenticated(
                            start,
                            &caller.unwrap_or(super::spawn::SpawnCaller::Operator { pane: None }),
                            crate::spawn::SpawnVerb::Fork,
                            None,
                            Some(&authority),
                        )?;
                        super::responses::encode_success(String::new(), result)
                    } else {
                        if remote.is_some() {
                            return Err(ErrorCode::AgentNotFound);
                        }
                        self.handle_agent_restart(String::new(), params)
                    }
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

#[cfg(test)]
mod tests;
