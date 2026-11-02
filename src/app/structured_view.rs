//! Canonical metadata/generation adapter for the native subscription owner.
use crate::api::error_code::ErrorCode;
use crate::structured::{
    launch::PermissionMode,
    view::{model::Model, stream::Stream, Target},
};
use std::io;
impl super::App {
    /// Warm sources only; cold recovery belongs to its owner. Mode is launch policy.
    pub(crate) fn open_structured_view(
        &self,
        target: Target,
        mode: PermissionMode,
    ) -> io::Result<Stream> {
        let invalid = |_| io::Error::other(ErrorCode::AgentNotFound);
        target.validate(&self.state).map_err(invalid)?;
        let id = &target.agent_id;
        let agent = self.structured_info(id).map_err(io::Error::other)?;
        let workers = &self.structured_workers;
        let registry = &self.state.structured_agents;
        let missing = || io::Error::other(ErrorCode::SessionBusy);
        let history = workers.history(registry, id).cloned().ok_or_else(missing)?;
        let live = workers.live(registry, id).ok_or_else(missing)?;
        let queue = workers
            .queue_source(registry, id, target.generation)
            .map_err(io::Error::other)?;
        let model = Model::new(target, agent, mode, vec![]);
        Stream::new(model, history, live, queue)
    }
    /// Must precede snapshot presentation and intents. Never reuse stale prompts.
    #[cfg(test)]
    pub(crate) fn refresh_structured_view(&self, stream: &mut Stream) -> Result<(), ErrorCode> {
        if stream.model.snapshot.target.validate(&self.state).is_err() {
            stream.retire();
            return Err(ErrorCode::AgentNotFound);
        }
        stream.model.snapshot.agent =
            self.structured_info(&stream.model.snapshot.target.agent_id)?;
        Ok(())
    }
}
