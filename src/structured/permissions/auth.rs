//! Sealed capability for a verified client token, consumed by permission policy.
//! #1746 owns production token verification/revocation and must issue this type
//! from this module only after checking the token store. There is no public
//! constructor from a subject string or deserialized command arguments.

/// Proof supplied by token authentication, never by caller-provided identity.
pub struct VerifiedToken<'a> {
    subject: &'a str,
    operate: bool,
    scopes: std::collections::BTreeSet<crate::api::client_tokens::Scope>,
    app: bool,
}
impl<'a> VerifiedToken<'a> {
    // Only this authentication module can mint a capability. Production token
    // verification is #1746; its dispatcher cannot mint one from command JSON.
    #[cfg(test)]
    fn new(subject: &'a str, operate: bool) -> Self {
        Self::authenticated(
            subject,
            if operate {
                [crate::api::client_tokens::Scope::Operate].into()
            } else {
                [crate::api::client_tokens::Scope::Read].into()
            },
            false,
        )
    }
    fn authenticated(
        subject: &'a str,
        scopes: std::collections::BTreeSet<crate::api::client_tokens::Scope>,
        app: bool,
    ) -> Self {
        Self {
            subject,
            operate: scopes.contains(&crate::api::client_tokens::Scope::Operate),
            scopes,
            app,
        }
    }
    /// Current facts sealed by token-store admission, including the verified source.
    pub(crate) fn command_facts(
        &self,
    ) -> (
        &str,
        bool,
        &std::collections::BTreeSet<crate::api::client_tokens::Scope>,
    ) {
        (self.subject, self.app, &self.scopes)
    }
    /// Current decision authority sealed by token admission, never session metadata.
    pub(crate) fn decision_subject(&self) -> Option<&str> {
        (self.operate && !self.subject.is_empty()).then_some(self.subject)
    }
}

/// Test-only issuer; absent from production builds.
#[cfg(test)]
pub(crate) fn fixture(subject: &str, operate: bool) -> VerifiedToken<'_> {
    VerifiedToken::new(subject, operate)
}

pub(crate) mod tokens;

// Dormant destination authority API, consumed by M2 admission, never local tokens.
#[allow(dead_code)]
pub(crate) mod gateway;
