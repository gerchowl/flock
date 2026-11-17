//! Shared production admission boundary for ADR-0052 profiles.

use crate::api::error_code::ErrorCode;

/// Server-owned binding state, never deserialized from agent-editable config.
///
/// Slice 1 holds this switch in memory only. Slice 2 persists it with binding
/// records, separately from repository and user configuration (ADR-0052 §1).
#[derive(Debug, Default)]
pub(crate) struct ProfileAdmission {
    enabled: bool,
}

/// A profile refusal is terminal for this request, with no legacy-launch fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProfileRefusal {
    /// The operator has not enabled production profiles.
    Disabled,
    /// This build cannot safely execute production profiles.
    NotReady,
}

impl ProfileRefusal {
    /// Stable API error code for a refused admission or enable request.
    pub(crate) fn code(self) -> ErrorCode {
        match self {
            Self::Disabled => ErrorCode::StructuredProfilesDisabled,
            Self::NotReady => ErrorCode::StructuredProfilesNotReady,
        }
    }

    /// Public explanation containing no binding, account or credential details.
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Disabled => "structured profile execution is disabled",
            Self::NotReady => {
                "structured profile execution is unavailable in this incomplete build"
            }
        }
    }
}

/// Production readiness is unconditionally false throughout slices 1–16.
///
/// Slice 17 is the only initial activation path. It must replace this predicate
/// with the complete readiness set, including current bundle/template
/// qualification and context validation, checked on enable and every admission.
pub(crate) fn production_ready() -> bool {
    false
}

impl ProfileAdmission {
    /// Admit a profile operation before any secret, process or mailbox effect.
    ///
    /// Every future profile spawn/resume/reload, migration, GUI and adapter path
    /// must call this guard and propagate refusal without calling legacy launch.
    /// This is a necessary gate, not a grant: future slices must also check
    /// current authority, workflow pins, permits and all operation prerequisites.
    pub(crate) fn admit(&self) -> Result<(), ProfileRefusal> {
        if !self.enabled {
            return Err(ProfileRefusal::Disabled);
        }
        if !production_ready() {
            return Err(ProfileRefusal::NotReady);
        }
        Ok(())
    }

    /// Request enablement after the shared resolver authenticates the operator.
    ///
    /// An incomplete build refuses without changing the switch. Slice 17 adds
    /// the reviewed plan transaction and full readiness/permit checks.
    pub(crate) fn enable(&mut self) -> Result<(), ProfileRefusal> {
        Self { enabled: true }.admit()?;
        self.enabled = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_defaults_off() {
        assert!(!ProfileAdmission::default().enabled);
    }

    #[test]
    fn incomplete_build_is_never_ready() {
        assert!(!production_ready());
    }

    #[test]
    fn disabled_guard_refuses_before_effects_without_legacy_fallback() {
        let gate = ProfileAdmission::default();
        let mut effects = Vec::new();
        let mut activate = || -> Result<(), ProfileRefusal> {
            gate.admit()?;
            effects.extend(["secret", "process", "mailbox", "legacy launch"]);
            Ok(())
        };
        assert_eq!(activate(), Err(ProfileRefusal::Disabled));
        assert!(effects.is_empty());
    }

    #[test]
    fn true_switch_cannot_bypass_build_readiness() {
        let gate = ProfileAdmission { enabled: true };
        assert_eq!(gate.admit(), Err(ProfileRefusal::NotReady));
    }

    #[test]
    fn refused_enable_keeps_switch_off() {
        let mut gate = ProfileAdmission::default();
        assert_eq!(gate.enable(), Err(ProfileRefusal::NotReady));
        assert!(!gate.enabled);
    }
}
