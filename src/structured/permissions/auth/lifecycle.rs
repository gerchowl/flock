//! Typed lifecycle authority; a remote grant never becomes a local session.
use super::tokens::{server::Session, Scope};
use crate::api::{error_code::ErrorCode, schema::AgentStartParams};

/// Remote attribution is a pair; the ancestry book stores its tagged encoding.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteSubject {
    pub home: String,
    pub principal: String,
}
impl RemoteSubject {
    pub(crate) fn parent_key(&self) -> String {
        serde_json::json!(["mesh-grant-v1", self]).to_string()
    }
    pub(crate) fn from_parent_key(value: &str) -> Option<Self> {
        let (kind, subject): (String, Self) = serde_json::from_str(value).ok()?;
        (kind == "mesh-grant-v1" && !subject.home.is_empty() && !subject.principal.is_empty())
            .then_some(subject)
    }
}

pub(crate) enum Authority<'a> {
    Local(&'a Session),
    #[cfg(feature = "web")]
    Grant(Box<crate::web::wire::GrantAdmission>),
}
impl Authority<'_> {
    pub(crate) fn app_operator(&self) -> bool {
        match self {
            Self::Local(s) => s.app().is_some() && s.info.scopes.contains(&Scope::Operate),
            #[cfg(feature = "web")]
            Self::Grant(_) => false,
        }
    }
    pub(crate) fn remote_subject(&self) -> Option<RemoteSubject> {
        match self {
            Self::Local(_) => None,
            #[cfg(feature = "web")]
            Self::Grant(g) => Some(g.subject()),
        }
    }
    pub(crate) fn start_workspace(
        &self,
        node: Option<&str>,
        params: &AgentStartParams,
    ) -> Result<Option<&str>, ErrorCode> {
        match self {
            Self::Local(_) => {
                let _ = (node, params);
                Ok(None)
            }
            #[cfg(feature = "web")]
            Self::Grant(g) => g.start(node, params).map(Some),
        }
    }
    pub(crate) fn grant_workspace(&self) -> Option<&str> {
        match self {
            Self::Local(_) => None,
            #[cfg(feature = "web")]
            Self::Grant(g) => Some(g.workspace()),
        }
    }
    pub(crate) fn permits_resume(&self) -> bool {
        match self {
            Self::Local(_) => false,
            #[cfg(feature = "web")]
            Self::Grant(g) => g.permits_resume(),
        }
    }
    pub(crate) fn grants_stop(&self, node: Option<&str>, target: &str) -> bool {
        match self {
            Self::Local(_) => {
                let _ = (node, target);
                false
            }
            #[cfg(feature = "web")]
            Self::Grant(g) => g.stop(node, target),
        }
    }
}
pub(crate) fn restart_start(target: &str) -> Result<AgentStartParams, ErrorCode> {
    serde_json::from_value(serde_json::json!({
        "runtime":"structured", "name":target, "argv":["claude"],
    }))
    .map_err(|_| ErrorCode::SerializationError)
}

#[cfg(test)]
mod tests;
