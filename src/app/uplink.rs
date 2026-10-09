//! Single-hub relay process binding for mesh and fleet requests.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SettleOnDelivery {
    pub(crate) pane: String,
    pub(crate) correlation_id: String,
}

#[derive(Debug, Default)]
pub(crate) struct Uplink {
    relay: Option<AttachedRelay>,
}

#[derive(Debug, Clone)]
struct AttachedRelay {
    pid: u32,
    /// The process's start time. A pid alone names a slot the OS reuses; the
    /// pair names one process, so a new process that inherits a dead relay's
    /// pid is not the relay.
    started: u64,
    /// The locally pinned name established by the verified mesh handshake.
    /// A later frame cannot rename the attachment.
    hub: Option<String>,
}

impl Uplink {
    /// Bind the uplink to the relay process `(pid, started)`. Refused while a
    /// DIFFERENT relay that is still alive holds it — a live relay is never
    /// displaced, so taking over the uplink means outliving the real one
    /// first. `start_of` reads a pid's start time now; a bound relay whose
    /// pid no longer reports its recorded start time is dead, whatever
    /// process holds the pid today.
    pub(crate) fn attach_relay(
        &mut self,
        pid: u32,
        started: u64,
        start_of: impl Fn(u32) -> Option<u64>,
    ) -> Result<(), u32> {
        match &self.relay {
            Some(current) if current.pid == pid && current.started == started => Ok(()),
            Some(current) if start_of(current.pid) == Some(current.started) => Err(current.pid),
            _ => {
                self.relay = Some(AttachedRelay {
                    pid,
                    started,
                    hub: None,
                });
                Ok(())
            }
        }
    }

    /// Whether `pid` is the attached relay, re-validated now: the bound relay
    /// must still be the same process. One that has died is unbound on the
    /// spot, so its successor can attach and a pid reuser inherits nothing.
    pub(crate) fn is_relay(
        &mut self,
        pid: Option<u32>,
        start_of: impl Fn(u32) -> Option<u64>,
    ) -> bool {
        let Some(relay) = &self.relay else {
            return false;
        };
        if start_of(relay.pid) != Some(relay.started) {
            self.relay = None;
            return false;
        }
        pid == Some(relay.pid)
    }

    /// Only the verified handshake may bind an attachment to an enrolled name.
    pub(crate) fn enroll_hub(&mut self, peer: String) {
        if let Some(relay) = self.relay.as_mut() {
            relay.hub = Some(peer);
        }
    }

    pub(crate) fn enrolled_hub(&self) -> Option<String> {
        self.relay.as_ref()?.hub.clone()
    }

    pub(crate) fn reset_enrollment(&mut self, peer: &str) {
        if let Some(relay) = self.relay.as_mut() {
            if relay.hub.as_deref() == Some(peer) {
                relay.hub = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_live_relay_is_never_displaced_and_the_hub_name_is_fixed() {
        let alive = |pid: u32| (pid == 100).then_some(7);
        let mut uplink = Uplink::default();
        assert!(uplink.attach_relay(100, 7, alive).is_ok());
        assert_eq!(
            uplink.attach_relay(200, 9, alive),
            Err(100),
            "a second process cannot take the uplink from a live relay"
        );
        assert!(uplink.is_relay(Some(100), alive));
        assert!(!uplink.is_relay(Some(200), alive));
        assert!(!uplink.is_relay(None, alive), "an unreadable pid is nobody");

        assert!(uplink.enrolled_hub().is_none());
        uplink.enroll_hub("hopper".into());
        assert_eq!(uplink.enrolled_hub().as_deref(), Some("hopper"));
        uplink.reset_enrollment("other.test");
        assert_eq!(uplink.enrolled_hub().as_deref(), Some("hopper"));
        uplink.reset_enrollment("hopper");
        assert!(uplink.enrolled_hub().is_none());

        // The real relay died (the hub reconnected): its successor attaches,
        // and names its hub afresh.
        let dead = |_: u32| None;
        assert!(uplink.attach_relay(300, 11, dead).is_ok());
        assert!(uplink.is_relay(Some(300), |pid| (pid == 300).then_some(11)));
        assert!(uplink.enrolled_hub().is_none());
        uplink.enroll_hub("hopper".into());
        assert_eq!(uplink.enrolled_hub().as_deref(), Some("hopper"));
    }

    #[test]
    fn a_process_that_reuses_a_dead_relays_pid_is_not_the_relay() {
        // Re-review of #416: bound by pid alone, whatever process the OS hands
        // the dead relay's pid to next would BE the relay.
        let mut uplink = Uplink::default();
        assert!(uplink.attach_relay(100, 7, |_| Some(7)).is_ok());
        let reused = |pid: u32| (pid == 100).then_some(99);
        assert!(
            !uplink.is_relay(Some(100), reused),
            "same pid, later start time: a different process"
        );
        assert!(
            uplink.attach_relay(400, 12, reused).is_ok(),
            "and the dead relay's binding is gone, so its successor attaches"
        );
    }
}
