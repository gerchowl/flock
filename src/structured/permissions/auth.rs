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
