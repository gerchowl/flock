//! Shared authenticated structured admission (ADR-0036 S3a-3).
use super::{spawn::SpawnCaller, App};
use crate::api::{
    agent_state::Lifecycle,
    error_code::ErrorCode,
    schema::{AgentStartParams, ResponseResult},
};
use crate::structured::{
    launch::{LaunchError, LaunchProfile, ProfileConfigDir, VerifiedBinary},
    registry::{Lineage, Record, StructuredRuntime},
    workers::{Admission, Launch},
};
use std::{io, path::PathBuf, time::Duration};
impl App {
    pub(super) fn start_structured(
        &mut self,
        params: AgentStartParams,
        caller: &SpawnCaller,
    ) -> Result<ResponseResult, ErrorCode> {
        self.start_structured_request(params, caller, crate::spawn::SpawnVerb::Fork, None)
    }
    /// Shared funnel; MCP supplies Spawn policy and a validated framed turn.
    pub(super) fn start_structured_request(
        &mut self,
        params: AgentStartParams,
        caller: &SpawnCaller,
        verb: crate::spawn::SpawnVerb,
        opening_turn: Option<crate::spawn::prompt::SpawnPrompt>,
    ) -> Result<ResponseResult, ErrorCode> {
        if params.dry_run
            || params.focus
            || params.active
            || params.here
            || params.split.is_some()
            || params.workspace_id.is_some()
            || params.tab_id.is_some()
            || params.label.is_some()
            || params.questions.is_some()
            || params.unattended
        {
            return Err(ErrorCode::RuntimeUnsupported);
        }
        if params.argv != ["claude"] {
            return Err(ErrorCode::InvalidAgentArgv);
        }
        if params.name == "self" {
            return Err(ErrorCode::InvalidAgentName);
        }
        let existing = self.structured_target(&params.name)?;
        // ADR-0036 §1a: neither history ownership nor a relay grants resume authority.
        if (existing.is_some()
            && (!matches!(caller, SpawnCaller::Operator { .. })
                || self.current_api_peer_uid.is_none_or(|uid| {
                    self.runtime_config.uid_policy.classify(uid)
                        != crate::structured::identity::Principal::Operator
                })))
            || self
                .inbound
                .edge(
                    self.current_api_peer_pid,
                    crate::platform::process_start_time,
                )
                .is_some()
        {
            return Err(ErrorCode::OperatorOnly);
        }
        if existing.as_ref().is_some_and(|(id, _)| {
            self.state
                .structured_agents
                .get(id)
                .is_some_and(|(_, p, _)| p.update.state.lifecycle == Lifecycle::Unknown)
        }) {
            return Err(ErrorCode::StructuredRecoveryRequired);
        }
        self.state
            .config
            .structured
            .run_as
            .admit()
            .map_err(|e| e.code())?;
        let lineage = self
            .admit_spawn(verb, caller, None)
            .map_err(|e| ErrorCode::from_code(e.code()).unwrap_or(ErrorCode::InternalError))?;
        let mcp = self
            .runtime_config
            .structured_mcp
            .clone()
            .ok_or(ErrorCode::StructuredEndpointRestartRequired)?;
        let is_new = existing.is_none();
        let (agent_id, generation) = if let Some((id, generation)) = existing {
            if params.cwd.is_some() || params.model.is_some() {
                return Err(ErrorCode::InvalidParams);
            }
            (id, generation)
        } else {
            if params.name.trim().is_empty() {
                return Err(ErrorCode::InvalidAgentName);
            }
            if !self.agent_name_conflicts(&params.name, "").is_empty() {
                return Err(ErrorCode::AgentNameTaken);
            }
            let cwd = params
                .cwd
                .as_ref()
                .map(PathBuf::from)
                .filter(|p| p.is_absolute() && p.is_dir())
                .ok_or(ErrorCode::InvalidParams)?
                .canonicalize()
                .map_err(|_| ErrorCode::InvalidParams)?;
            if params.model.as_ref().is_some_and(|m| {
                m.is_empty() || m.starts_with('-') || m.chars().any(char::is_control)
            }) {
                return Err(ErrorCode::InvalidParams);
            }
            let id = crate::terminal::AgentId::alloc(&crate::app::short_host_name()).to_string();
            let generation = self
                .state
                .structured_agents
                .register(Record {
                    runtime: StructuredRuntime::Structured,
                    agent_id: id.clone(),
                    name: Some(params.name),
                    harness: "claude".into(),
                    session_id: session_id()?,
                    cwd,
                    model: params.model,
                    lineage: Some(Lineage {
                        parent: lineage.as_ref().map(|l| l.parent_agent_id.clone()),
                        depth: lineage.as_ref().map_or(0, |l| l.child_depth),
                    }),
                })
                .map_err(|_| ErrorCode::AgentNameTaken)?;
            (id, generation)
        };
        let state = crate::session::data_dir();
        let config = self.state.config.structured.clone();
        let prepare_state = state.clone();
        let path = std::env::var_os("PATH").unwrap_or_default();
        let started = self.structured_workers.start(
            &mut self.state.structured_agents,
            Launch {
                opening_turn,
                agent_id: agent_id.clone(),
                generation,
                leases: self.session_leases.clone(),
                state,
                mcp,
                storage: self.state.config.sessions.clone(),
                stall_after: Duration::from_millis(self.state.config.sessions.stall_ms),
                admission: lineage.as_ref().map(|l| Admission {
                    config: self.state.config.fleet,
                    pty: l.pty_census,
                    paused: self.fleet_pause.paused,
                    verb,
                }),
            },
            move |record| {
                let binary = std::env::split_paths(&path)
                    .filter(|p| p.is_absolute())
                    .map(|p| p.join("claude"))
                    .find(|p| p.is_file())
                    .ok_or(LaunchError::UnsupportedHarness)?;
                let directory = ProfileConfigDir::for_operator(&prepare_state, &config.profile)?;
                let binary = VerifiedBinary::check(&binary, |path, args| {
                    let mut command =
                        crate::process::TracedCommand::new(path, "structured-version");
                    command
                        .args(args)
                        .env_clear()
                        .process_group(0)
                        .stdin(std::process::Stdio::null())
                        .env("CLAUDE_CONFIG_DIR", directory.path());
                    for key in [
                        "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TMPDIR",
                    ] {
                        if let Some(value) = std::env::var_os(key) {
                            command.env(key, value);
                        }
                    }
                    let output = command
                        .output_traced_with_timeout_and_limit(Duration::from_secs(10), 4096)?;
                    if !output.status.success() {
                        return Err(io::Error::other("harness version command failed"));
                    }
                    String::from_utf8(output.stdout).map_err(io::Error::other)
                })?;
                LaunchProfile::prepare(&config, &prepare_state, &record.session_id, binary, |key| {
                    std::env::var_os(key)
                })
            },
        );
        if let Err(error) = started {
            if is_new {
                let _ = self.state.structured_agents.remove(&agent_id, generation);
            }
            return Err(error);
        }
        self.structured_info(&agent_id)
            .map(|agent| ResponseResult::StructuredAgentInfo { agent })
    }
}
/// Allocate a version-4 UUID from OS entropy; no process or clock identity is used.
fn session_id() -> Result<String, ErrorCode> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| ErrorCode::InternalError)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}
#[cfg(test)]
mod tests;
