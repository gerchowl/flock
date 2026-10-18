//! Sealed capability for a verified client token, consumed by permission policy.
//! #1746 owns production token verification/revocation and must issue this type
//! from this module only after checking the token store. There is no public
//! constructor from a subject string or deserialized command arguments.

/// Proof supplied by token authentication, never by caller-provided identity.
pub struct VerifiedToken<'a> {
    subject: &'a str,
    operate: bool,
}
impl<'a> VerifiedToken<'a> {
    // Only this authentication module can mint a capability. Production token
    // verification is #1746; its dispatcher cannot mint one from command JSON.
    fn new(subject: &'a str, operate: bool) -> Self {
        Self { subject, operate }
    }
    pub(super) fn decision_subject(&self) -> Option<&str> {
        (self.operate && !self.subject.is_empty()).then_some(self.subject)
    }
}

impl super::Principal<'_> {
    /// Admission authority from authenticated dispatcher context (ADR-0036 §3).
    /// Claimed producer names are never accepted as operator capabilities.
    pub(crate) fn queue_origin(
        &self,
    ) -> Result<crate::structured::queue::admission::Principal, crate::api::error_code::ErrorCode>
    {
        use crate::structured::queue::{admission::Principal as QueuePrincipal, model::Producer};
        match self {
            Self::Operator => Ok(QueuePrincipal::Operator),
            Self::Token(token) if token.decision_subject().is_some() => {
                Ok(QueuePrincipal::Operator)
            }
            Self::Agent(id) | Self::Lease(id) if !id.is_empty() => {
                Ok(QueuePrincipal::Origin(Producer::Agent((*id).into())))
            }
            _ => Err(crate::api::error_code::ErrorCode::NotAuthorized),
        }
    }
}

/// Test-only issuer; absent from production builds.
#[cfg(test)]
pub(crate) fn fixture(subject: &str, operate: bool) -> VerifiedToken<'_> {
    VerifiedToken::new(subject, operate)
}

pub(crate) mod tokens;
