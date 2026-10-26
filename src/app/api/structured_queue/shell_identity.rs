//! Shell-send admission from server-owned evidence (ADR-0036 §1a).
//! Collection stays at the request boundary; the facts reader is the same
//! injectable live-process snapshot interface used by lease authentication.

/// Socket credentials and independently resolved request context.
#[derive(Clone, Copy)]
pub(super) struct Caller {
    pub uid: Option<u32>,
    pub pid: Option<u32>,
    pub birth: Option<u64>,
    pub scoped: bool,
    pub unrelated: bool,
    pub relay: bool,
}

/// One current pane and its owning runtime's original process record.
#[derive(Clone, Copy)]
pub(super) struct Pane {
    pub id: u32,
    pub root_shell: bool,
    pub agent: bool,
    pub default_shell: bool,
    pub pid: Option<u32>,
    pub birth: Option<u64>,
    pub reaped: bool,
}

/// Every independent check fails closed. Missing live facts include dead and
/// zombie processes. Neither an outer policy check nor a claimed name supplies
/// evidence here. The production reader is `platform::live_process_facts`.
pub(super) fn admit(
    caller: Caller,
    operator_uid: u32,
    panes: impl IntoIterator<Item = Pane>,
    mut facts: impl FnMut(u32) -> Option<(u32, u64)>,
) -> Option<u32> {
    if caller.uid != Some(operator_uid) // M1: kernel UID.
        || caller.scoped // M2: scoped requests never become operators.
        || !caller.unrelated
        || caller.relay
    {
        return None;
    }
    let (parent, birth) = facts(caller.pid?)?;
    if caller.birth != Some(birth) {
        // M3: caller PID reuse.
        return None;
    }
    for pane in panes {
        let Some(shell_pid) = pane.pid else { continue };
        if pane.root_shell
            && !pane.agent // M5: agent identity beats shell provenance.
            && pane.default_shell // M6: runtime spawn provenance.
            && shell_pid == parent // M7: immediate parent, not another live process.
            && !pane.reaped // M8: owning watcher has not reaped this child.
            && facts(shell_pid).is_some_and(|(_, birth)| Some(birth) == pane.birth)
        // M9: root PID reuse.
        {
            return Some(pane.id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fictional process snapshots, never used to signal or inspect ambient PIDs.
    fn facts(pid: u32) -> Option<(u32, u64)> {
        match pid {
            101 => Some((100, 9001)),
            100 => Some((1, 9000)),
            200 => Some((1, 8000)),
            _ => None,
        }
    }
    fn caller() -> Caller {
        Caller {
            uid: Some(500),
            pid: Some(101),
            birth: Some(9001),
            scoped: false,
            unrelated: true,
            relay: false,
        }
    }
    fn pane() -> Pane {
        Pane {
            id: 42,
            root_shell: true,
            agent: false,
            default_shell: true,
            pid: Some(100),
            birth: Some(9000),
            reaped: false,
        }
    }
    fn refused(caller: Caller, pane: Pane) {
        assert_eq!(admit(caller, 500, [pane], facts), None);
    }
    #[test]
    fn shell2494_admission_positive_snapshot() {
        assert_eq!(admit(caller(), 500, [pane()], facts), Some(42));
    }
    #[test]
    fn shell2494_admission_uid() {
        for uid in [Some(501), None] {
            refused(Caller { uid, ..caller() }, pane());
        }
    }
    #[test]
    fn shell2494_admission_scope() {
        refused(
            Caller {
                scoped: true,
                ..caller()
            },
            pane(),
        );
    }
    #[test]
    fn shell2494_admission_caller_birth() {
        for birth in [Some(9002), None] {
            refused(Caller { birth, ..caller() }, pane());
        }
    }
    #[test]
    fn shell2494_admission_agent_terminal() {
        refused(
            caller(),
            Pane {
                agent: true,
                ..pane()
            },
        );
    }
    #[test]
    fn shell2494_admission_default_shell() {
        refused(
            caller(),
            Pane {
                default_shell: false,
                ..pane()
            },
        );
    }
    #[test]
    fn shell2494_admission_parent() {
        // Both the caller's real parent and the different pane child are alive
        // with their own correct births. Only the parent equality must refuse.
        refused(
            caller(),
            Pane {
                pid: Some(200),
                birth: Some(8000),
                ..pane()
            },
        );
    }
    #[test]
    fn shell2494_admission_reaped() {
        // Isolate the watcher latch from an OS snapshot still reporting alive.
        refused(
            caller(),
            Pane {
                reaped: true,
                ..pane()
            },
        );
    }
    #[test]
    fn shell2494_admission_shell_birth() {
        for birth in [Some(9002), None] {
            refused(caller(), Pane { birth, ..pane() });
        }
    }
    #[test]
    fn shell2494_admission_other_boundaries() {
        refused(
            Caller {
                unrelated: false,
                ..caller()
            },
            pane(),
        );
        refused(
            Caller {
                relay: true,
                ..caller()
            },
            pane(),
        );
        refused(
            caller(),
            Pane {
                root_shell: false,
                ..pane()
            },
        );
        refused(
            Caller {
                pid: None,
                ..caller()
            },
            pane(),
        );
        refused(
            caller(),
            Pane {
                pid: None,
                ..pane()
            },
        );
        assert_eq!(admit(caller(), 500, [pane()], |_| None), None);
        assert_eq!(
            admit(caller(), 500, [pane()], |pid| {
                (pid != 100).then(|| facts(pid)).flatten()
            }),
            None
        );
    }
}
