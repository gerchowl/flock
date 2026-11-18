//! Shared production admission boundary for ADR-0052 profiles.

mod bindings;

use crate::api::error_code::ErrorCode;

/// Server-owned binding state, never deserialized from agent-editable config.
///
/// The switch persists with binding records, separately from repository and
/// user configuration (ADR-0052 §1). Production readiness remains false.
#[derive(Debug, Default)]
pub(crate) struct ProfileAdmission {
    store: bindings::BindingStore,
}

/// A profile refusal is terminal for this request, with no legacy-launch fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub(crate) enum ProfileRefusal {
    /// The operator has not enabled production profiles.
    Disabled,
    /// This build cannot safely execute production profiles.
    NotReady,
    /// Durable operator-state replacement failed.
    Persistence,
    /// The document is older than the independently persisted revision floor.
    PolicyRevisionRegressed {
        /// Revision found in the rejected document.
        store: u64,
        /// Highest durable revision observed by this authority.
        floor: u64,
    },
    /// Current policy or its independent floor cannot be established safely.
    PolicyRevisionUnavailable,
}

impl ProfileRefusal {
    /// Stable API error code for a refused admission or enable request.
    pub(crate) fn code(self) -> ErrorCode {
        match self {
            Self::Disabled => ErrorCode::StructuredProfilesDisabled,
            Self::NotReady => ErrorCode::StructuredProfilesNotReady,
            Self::Persistence => ErrorCode::InternalError,
            Self::PolicyRevisionRegressed { .. } => ErrorCode::PolicyRevisionRegressed,
            Self::PolicyRevisionUnavailable => ErrorCode::PolicyRevisionUnavailable,
        }
    }

    /// Public explanation containing no binding, account or credential details.
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Persistence => "profile policy persistence refused",
            Self::PolicyRevisionRegressed { .. } => "persisted profile policy revision regressed",
            Self::PolicyRevisionUnavailable => "current profile policy revision is unavailable",
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
    /// Restore durable authority independently of workspace restoration.
    pub(crate) fn load(path: &std::path::Path) -> Self {
        let mut gate = Self::default();
        gate.store.load(path);
        gate
    }

    /// Return binding metadata only to the authenticated local operator.
    pub(crate) fn status(&self, operator: bool) -> serde_json::Value {
        self.store.status(operator)
    }

    /// Admit a profile operation before any secret, process or mailbox effect.
    ///
    /// Every future profile spawn/resume/reload, migration, GUI and adapter path
    /// must call this guard and propagate refusal without calling legacy launch.
    /// This is a necessary gate, not a grant: future slices must also check
    /// current authority, workflow pins, permits and all operation prerequisites.
    pub(crate) fn admit(&self) -> Result<(), ProfileRefusal> {
        if let Some(fault) = self.store.refusal() {
            return Err(fault);
        }
        if !self.store.execution_switch {
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
        if let Some(fault) = self.store.refusal() {
            return Err(fault);
        }
        if !production_ready() {
            return Err(ProfileRefusal::NotReady);
        }
        self.store.execution_switch = true;
        if self
            .store
            .save(&crate::session::data_dir().join("profile-bindings.json"))
            .is_err()
        {
            self.store.execution_switch = false;
            return Err(ProfileRefusal::Persistence);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_defaults_off() {
        assert!(!ProfileAdmission::default().store.execution_switch);
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
        let mut gate = ProfileAdmission::default();
        gate.store.execution_switch = true;
        assert_eq!(gate.admit(), Err(ProfileRefusal::NotReady));
    }

    #[test]
    fn refused_enable_keeps_switch_off() {
        let mut gate = ProfileAdmission::default();
        assert_eq!(gate.enable(), Err(ProfileRefusal::NotReady));
        assert!(!gate.store.execution_switch);
    }
}
