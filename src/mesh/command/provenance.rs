//! Home authentication adapters. Neither identity nor ceilings come from command JSON.
use super::*;
use crate::structured::{
    permissions::auth::VerifiedToken, profiles::ProfileAdmission, registry::Registry,
};

/// Sealed authenticated facts, never deserialized from requests or mesh frames.
#[derive(Debug)]
pub(crate) struct Provenance<'a> {
    principal: String,
    source: TokenSource,
    scopes: BTreeSet<Scope>,
    scope: Option<&'a crate::api::scoped::Scope>,
    admission: std::marker::PhantomData<&'a VerifiedToken<'a>>,
}
impl<'a> Provenance<'a> {
    /// Call only inside the token store's revocation-fenced admission callback.
    pub(crate) fn from_token(token: &'a VerifiedToken<'_>) -> Self {
        let (principal, app, scopes) = token.command_facts();
        Self {
            principal: principal.into(),
            source: if app {
                TokenSource::App
            } else {
                TokenSource::File
            },
            scopes: scopes.clone(),
            scope: None,
            admission: std::marker::PhantomData,
        }
    }
    /// Reauthenticate the live scoped lease and resolve its recorded profile pin.
    /// Profile-selection grants are never exported as destination authority.
    pub(crate) fn from_scoped(
        scope: &'a crate::api::scoped::Scope,
        profiles: &mut ProfileAdmission,
        registry: &Registry,
    ) -> Result<Self, &'static str> {
        let crate::structured::lease::Caller::Agent { agent, .. } = scope.caller() else {
            return Err("not_authorized");
        };
        let (record, _, _) = registry.get(&agent).ok_or("not_authorized")?;
        let scopes = profiles.mesh_scopes(record.profile_identity.as_ref())?;
        if !matches!(scope.caller(), crate::structured::lease::Caller::Agent { agent: current, .. } if current == agent)
        {
            return Err("not_authorized");
        }
        Ok(Self {
            principal: agent,
            source: TokenSource::Agent,
            scopes,
            scope: Some(scope),
            admission: std::marker::PhantomData,
        })
    }
    pub(super) fn bind(&self, body: &mut Body) -> Result<(), &'static str> {
        if self.scope.is_some_and(|scope| !matches!(scope.caller(), crate::structured::lease::Caller::Agent { agent, .. } if agent == self.principal)) {
            return Err("not_authorized");
        }
        if body.principal != self.principal
            || body.token_source != self.source
            || body.scopes != self.scopes
        {
            return Err("not_authorized");
        }
        body.principal.clone_from(&self.principal);
        body.token_source = self.source.clone();
        body.scopes.clone_from(&self.scopes);
        Ok(())
    }
    #[cfg(test)]
    pub(super) fn fixture(body: &Body) -> Self {
        Self {
            principal: body.principal.clone(),
            source: body.token_source.clone(),
            scopes: body.scopes.clone(),
            scope: None,
            admission: std::marker::PhantomData,
        }
    }
}
