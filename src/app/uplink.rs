//! Spoke → hub message uplink (#410).
//!
//! The fleet is hub-and-spoke on purpose: only the hub carries `[[peers]]`, so
//! a spoke holds no key to any other machine and no other machine holds one to
//! it except the hub. That leaves a spoke exactly one channel to the rest of
//! the fleet — the `flk peers relay --push` the hub runs on it over the ssh
//! edge the HUB holds. Summaries already travel up that relay as pushes; this
//! module is the spoke-side half of making a message travel up it too.
//!
//! The shape is a pair of long-polls, both parked here rather than blocking
//! the main loop:
//!
//! - The relay parks a `msg.uplink_take` until there is a frame to carry, or
//!   until the heartbeat window passes and it should ask again. Its asking IS
//!   the liveness signal: no take within two windows means no hub is holding a
//!   relay into this server, and a send that needs one is refused up front
//!   instead of waiting for an answer nobody will carry.
//! - The sender's `msg.send` parks until the hub's `msg.uplink_result` comes
//!   back down the same relay, so the caller gets the real outcome — delivered
//!   `via <hub>`, or the hub's own refusal naming its failed hop — rather than
//!   a hopeful "queued" that the network then contradicts.
//!
//! Pure state, no IO: every call takes `now`, so the timing rules are testable
//! without a clock or a socket. Encoding responses is the caller's job.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::api::schema::UplinkFrame;

/// A response the current handler asked to hold rather than send (#410).
///
/// Set by the handler, consumed by the transport right after it returns — the
/// one place that holds the request's responder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Park {
    /// A `msg.send` waiting for the hub's answer to this frame.
    Send { uplink_id: String },
    /// A relay's `msg.uplink_take` waiting for something to carry.
    Take { request_id: String },
}

/// A send waiting for the hub to answer.
#[derive(Debug)]
pub(crate) struct ParkedSend {
    /// The CALLER's request id, so the eventual answer is addressed to it.
    pub(crate) request_id: String,
    pub(crate) correlation_id: String,
    pub(crate) from_agent: String,
    pub(crate) to_agent: String,
    /// The handed-up message's tier, so a hub's acceptance can be recorded as
    /// a question relayed away (ADR-0018 §1's reply rule). `fyi` until the
    /// caller says otherwise.
    pub(crate) intent: crate::api::schema::MsgIntent,
    deadline: Instant,
    /// `None` until the transport attaches it, and forever when the request
    /// did not come through a transport that can park (a direct in-process
    /// call). Such a send still resolves; its answer just has nowhere to go.
    respond_to: Option<Sender<String>>,
}

impl ParkedSend {
    pub(crate) fn new(
        request_id: String,
        correlation_id: String,
        from_agent: String,
        to_agent: String,
        deadline: Instant,
    ) -> Self {
        Self {
            request_id,
            correlation_id,
            from_agent,
            to_agent,
            intent: crate::api::schema::MsgIntent::Fyi,
            deadline,
            respond_to: None,
        }
    }

    /// Deliver the final answer to whoever is waiting, if anyone is.
    pub(crate) fn answer(self, response: String) {
        if let Some(respond_to) = self.respond_to {
            let _ = respond_to.send(response);
        }
    }
}

/// A relay's take, parked until there is a frame for it.
#[derive(Debug)]
pub(crate) struct ParkedTake {
    pub(crate) request_id: String,
    deadline: Instant,
    respond_to: Sender<String>,
}

impl ParkedTake {
    pub(crate) fn answer(self, response: String) {
        let _ = self.respond_to.send(response);
    }
}

#[derive(Debug)]
struct Outbound {
    frame: UplinkFrame,
    /// When a relay last took this frame. A frame stays here until the relay
    /// ACKS it on its next take, and is re-offered if that ack does not come
    /// within the heartbeat window — the relay may have died between taking
    /// it and writing it up the pipe. The recipient's mailbox dedupes on the
    /// message's `correlation_id`, so a re-offer cannot double-deliver.
    offered_at: Option<Instant>,
}

/// What timed out on one sweep.
#[derive(Debug, Default)]
pub(crate) struct Expired {
    /// Sends the hub never answered, each with whether a relay had even taken
    /// its frame — the difference between "no hub saw it" and "a hub saw it
    /// and went quiet", which a sender needs to decide whether to retry.
    pub(crate) sends: Vec<(ParkedSend, bool)>,
    /// Takes whose heartbeat window ran out: answer empty, so the relay asks
    /// again and in doing so proves it is still there.
    pub(crate) takes: Vec<ParkedTake>,
}

#[derive(Debug, Default)]
pub(crate) struct Uplink {
    outbound: VecDeque<Outbound>,
    sends: HashMap<String, ParkedSend>,
    takes: Vec<ParkedTake>,
    last_take_at: Option<Instant>,
    pending_park: Option<Park>,
    /// The one `flk peers relay` process allowed to speak for the hub.
    relay: Option<AttachedRelay>,
}

/// The relay bound to this server's uplink (#410 review).
///
/// The relay's methods arrive over the local socket, which any same-user
/// process can reach. Binding them to ONE process — the relay the hub's ssh
/// session started, identified by its socket peer pid — means a stray process
/// cannot take a spoke's pending messages, forge the hub's answer to them, or
/// plant fleet rows, unless it first displaces a relay that is still alive.
#[derive(Debug, Clone)]
struct AttachedRelay {
    pid: u32,
    /// The process's start time. A pid alone names a slot the OS reuses; the
    /// pair names one process, so a new process that inherits a dead relay's
    /// pid is not the relay.
    started: u64,
    /// The hub's name, recorded once from the first frame this relay carried
    /// and then fixed for the attachment: a later frame cannot rename it.
    hub: Option<String>,
}

impl Uplink {
    /// Whether a hub is holding a relay into this server right now.
    ///
    /// A live relay re-asks the instant a take is answered, so one heartbeat
    /// since the last take is enough slack; anything longer only makes a send
    /// behind a dead relay wait longer to be told so.
    pub(crate) fn is_attached(&self, now: Instant, heartbeat: Duration) -> bool {
        if !self.takes.is_empty() {
            return true;
        }
        self.last_take_at
            .is_some_and(|at| now.saturating_duration_since(at) <= heartbeat)
    }

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

    /// The hub's name for this attachment: the first `claimed` wins, and every
    /// later claim is ignored in its favour.
    pub(crate) fn record_hub(&mut self, claimed: &str) -> Option<String> {
        let relay = self.relay.as_mut()?;
        Some(relay.hub.get_or_insert_with(|| claimed.to_string()).clone())
    }

    /// Queue a frame nobody waits on — flock's own automatic reply, such as a
    /// mute's deferral (#410). Tracked like any other so the hub's answer and
    /// the timeout still resolve it, but the request that caused it is
    /// answered at once rather than parked.
    pub(crate) fn hand_up_detached(&mut self, frame: UplinkFrame, send: ParkedSend) {
        let uplink_id = frame.uplink_id.clone();
        self.outbound.push_back(Outbound {
            frame,
            offered_at: None,
        });
        self.sends.insert(uplink_id, send);
    }

    /// Queue a frame for the hub and park its sender.
    pub(crate) fn hand_up(&mut self, frame: UplinkFrame, send: ParkedSend) {
        let uplink_id = frame.uplink_id.clone();
        self.outbound.push_back(Outbound {
            frame,
            offered_at: None,
        });
        self.sends.insert(uplink_id.clone(), send);
        self.pending_park = Some(Park::Send { uplink_id });
    }

    /// A relay asks for frames. Returns them now if any are due; otherwise
    /// parks the take (the caller's response is then held, not sent).
    pub(crate) fn take(
        &mut self,
        request_id: String,
        ack: &[String],
        now: Instant,
        heartbeat: Duration,
    ) -> Option<Vec<UplinkFrame>> {
        self.last_take_at = Some(now);
        self.outbound
            .retain(|pending| !ack.contains(&pending.frame.uplink_id));
        let frames = self.offer_due(now, heartbeat);
        if frames.is_empty() {
            self.pending_park = Some(Park::Take { request_id });
            None
        } else {
            Some(frames)
        }
    }

    /// Frames that should go to a relay now: never offered, or offered and
    /// not acknowledged within the heartbeat window.
    fn offer_due(&mut self, now: Instant, heartbeat: Duration) -> Vec<UplinkFrame> {
        let mut frames = Vec::new();
        for pending in &mut self.outbound {
            let due = pending
                .offered_at
                .is_none_or(|at| now.saturating_duration_since(at) > heartbeat);
            if due {
                pending.offered_at = Some(now);
                frames.push(pending.frame.clone());
            }
        }
        frames
    }

    /// Hand due frames to a parked take, if one is waiting. Returns the take
    /// and what it should be answered with.
    pub(crate) fn feed_parked_take(
        &mut self,
        now: Instant,
        heartbeat: Duration,
    ) -> Option<(ParkedTake, Vec<UplinkFrame>)> {
        if self.takes.is_empty() {
            return None;
        }
        let frames = self.offer_due(now, heartbeat);
        if frames.is_empty() {
            return None;
        }
        self.last_take_at = Some(now);
        Some((self.takes.remove(0), frames))
    }

    /// The park the current handler asked for, if any. Consumed by the
    /// transport; cleared at the start of every dispatch so a park set by a
    /// request that never reached a transport cannot capture the next one.
    pub(crate) fn take_pending_park(&mut self) -> Option<Park> {
        self.pending_park.take()
    }

    /// Give a parked request its responder.
    pub(crate) fn attach(
        &mut self,
        park: Park,
        respond_to: Sender<String>,
        now: Instant,
        heartbeat: Duration,
    ) -> Result<(), Sender<String>> {
        match park {
            Park::Send { uplink_id } => match self.sends.get_mut(&uplink_id) {
                Some(send) => {
                    send.respond_to = Some(respond_to);
                    Ok(())
                }
                // Already resolved before the transport got here — cannot
                // happen on the single-threaded loop, but if it did the
                // caller must not be left hanging.
                None => Err(respond_to),
            },
            Park::Take { request_id } => {
                self.takes.push(ParkedTake {
                    request_id,
                    deadline: now + heartbeat,
                    respond_to,
                });
                Ok(())
            }
        }
    }

    /// The hub answered: resolve the send waiting on `uplink_id`. `None` when
    /// nothing is waiting — a re-offered frame answered twice, or one whose
    /// sender already timed out.
    pub(crate) fn complete(&mut self, uplink_id: &str) -> Option<ParkedSend> {
        self.outbound
            .retain(|pending| pending.frame.uplink_id != uplink_id);
        self.sends.remove(uplink_id)
    }

    /// Everything whose deadline has passed — and every send no relay ever
    /// took once no relay is attached any more: nothing will carry it, so
    /// waiting out its full timeout only delays the same answer.
    pub(crate) fn expire(&mut self, now: Instant, heartbeat: Duration) -> Expired {
        let mut expired = Expired::default();
        let (takes, kept): (Vec<ParkedTake>, Vec<ParkedTake>) = std::mem::take(&mut self.takes)
            .into_iter()
            .partition(|take| now >= take.deadline);
        self.takes = kept;
        expired.takes = takes;
        let attached = self.is_attached(now, heartbeat);
        let untaken = |uplink_id: &str, outbound: &VecDeque<Outbound>| {
            outbound
                .iter()
                .any(|pending| pending.frame.uplink_id == uplink_id && pending.offered_at.is_none())
        };
        let due: Vec<String> = self
            .sends
            .iter()
            .filter(|(uplink_id, send)| {
                now >= send.deadline || (!attached && untaken(uplink_id, &self.outbound))
            })
            .map(|(uplink_id, _)| uplink_id.clone())
            .collect();
        for uplink_id in due {
            let taken = self
                .outbound
                .iter()
                .find(|pending| pending.frame.uplink_id == uplink_id)
                .is_some_and(|pending| pending.offered_at.is_some());
            // A frame whose sender gave up must not be carried later: a late
            // delivery the sender was told had failed is worse than none.
            self.outbound
                .retain(|pending| pending.frame.uplink_id != uplink_id);
            if let Some(send) = self.sends.remove(&uplink_id) {
                expired.sends.push((send, taken));
            }
        }
        expired
    }

    #[cfg(test)]
    pub(crate) fn outbound_len(&self) -> usize {
        self.outbound.len()
    }

    /// The uplink id of the oldest frame still waiting, for tests that answer
    /// it without driving a relay.
    #[cfg(test)]
    pub(crate) fn complete_peek_for_test(&self) -> Option<String> {
        self.outbound
            .front()
            .map(|pending| pending.frame.uplink_id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{MessageTarget, MsgIntent, MsgSendParams};

    const HEARTBEAT: Duration = Duration::from_secs(20);

    fn frame(uplink_id: &str) -> UplinkFrame {
        UplinkFrame {
            uplink_id: uplink_id.into(),
            message: MsgSendParams {
                to: MessageTarget::Agent {
                    agent: "agent_c_1".into(),
                },
                body: "hi".into(),
                intent: MsgIntent::Fyi,
                correlation_id: Some("c1".into()),
                in_reply_to: None,
                from_agent: Some("agent_a_1".into()),
                from_host: Some("a".into()),
                intent_unrecognised: None,
            },
        }
    }

    fn parked(deadline: Instant) -> ParkedSend {
        ParkedSend::new(
            "req".into(),
            "c1".into(),
            "agent_a_1".into(),
            "agent_c_1".into(),
            deadline,
        )
    }

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

        assert_eq!(uplink.record_hub("mba22").as_deref(), Some("mba22"));
        assert_eq!(
            uplink.record_hub("attacker").as_deref(),
            Some("mba22"),
            "a later frame cannot rename the hub"
        );

        // The real relay died (the hub reconnected): its successor attaches,
        // and names its hub afresh.
        let dead = |_: u32| None;
        assert!(uplink.attach_relay(300, 11, dead).is_ok());
        assert!(uplink.is_relay(Some(300), |pid| (pid == 300).then_some(11)));
        assert_eq!(uplink.record_hub("mba22").as_deref(), Some("mba22"));
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

    #[test]
    fn a_server_no_relay_has_asked_is_not_attached() {
        // The refusal "no hub holds a relay to this server" depends on this:
        // a spoke must not park a sender behind a relay that does not exist.
        let uplink = Uplink::default();
        assert!(!uplink.is_attached(Instant::now(), HEARTBEAT));
    }

    #[test]
    fn a_relay_that_stops_asking_stops_counting_as_attached() {
        let mut uplink = Uplink::default();
        let then = Instant::now();
        assert_eq!(uplink.take("t".into(), &[], then, HEARTBEAT), None);
        let park = uplink.take_pending_park().expect("the take parked");
        let (tx, _rx) = std::sync::mpsc::channel();
        uplink.attach(park, tx, then, HEARTBEAT).expect("attached");
        // Parked: attached for as long as it is parked.
        assert!(uplink.is_attached(then + HEARTBEAT * 5, HEARTBEAT));

        let expired = uplink.expire(then + HEARTBEAT, HEARTBEAT);
        assert_eq!(expired.takes.len(), 1, "the parked take is answered empty");
        assert!(
            uplink.is_attached(then + HEARTBEAT, HEARTBEAT),
            "and the relay is still counted while it has time to ask again"
        );
        assert!(
            !uplink.is_attached(then + HEARTBEAT * 2, HEARTBEAT),
            "but not once it has gone a window without asking"
        );
    }

    #[test]
    fn a_frame_is_re_offered_until_acked_and_never_after() {
        // Pitfall 3 of #410: a relay that took a frame and died before writing
        // it must not lose the message, and an acked one must not go twice.
        let mut uplink = Uplink::default();
        let now = Instant::now();
        uplink.hand_up(frame("u1"), parked(now + HEARTBEAT * 3));

        let first = uplink
            .take("t1".into(), &[], now, HEARTBEAT)
            .expect("a due frame is returned at once");
        assert_eq!(first.len(), 1);
        assert_eq!(
            uplink.take("t2".into(), &[], now, HEARTBEAT),
            None,
            "an offered, unacked frame is not offered again inside the window"
        );
        let again = uplink
            .take(
                "t3".into(),
                &[],
                now + HEARTBEAT + Duration::from_secs(1),
                HEARTBEAT,
            )
            .expect("re-offered once the window passes with no ack");
        assert_eq!(again[0].uplink_id, "u1");

        let later = now + HEARTBEAT * 2;
        assert_eq!(
            uplink.take("t4".into(), &["u1".into()], later, HEARTBEAT),
            None,
            "an acked frame is gone"
        );
        assert_eq!(uplink.outbound_len(), 0);
    }

    #[test]
    fn a_frame_handed_up_while_a_take_is_parked_goes_straight_to_it() {
        let mut uplink = Uplink::default();
        let now = Instant::now();
        assert_eq!(uplink.take("t".into(), &[], now, HEARTBEAT), None);
        let park = uplink.take_pending_park().expect("the take parked");
        let (tx, _rx) = std::sync::mpsc::channel();
        uplink.attach(park, tx, now, HEARTBEAT).expect("attached");

        uplink.hand_up(frame("u1"), parked(now + HEARTBEAT));
        let (take, frames) = uplink
            .feed_parked_take(now, HEARTBEAT)
            .expect("the waiting relay is fed");
        assert_eq!(take.request_id, "t");
        assert_eq!(frames[0].uplink_id, "u1");
    }

    #[test]
    fn an_answered_send_is_resolved_once() {
        let mut uplink = Uplink::default();
        let now = Instant::now();
        uplink.hand_up(frame("u1"), parked(now + HEARTBEAT));
        assert!(uplink.complete("u1").is_some());
        assert!(
            uplink.complete("u1").is_none(),
            "a re-offered frame answered twice resolves nothing the second time"
        );
        assert_eq!(uplink.outbound_len(), 0, "and is never carried again");
    }

    #[test]
    fn a_send_no_relay_will_ever_take_fails_as_soon_as_the_relay_is_gone() {
        // A relay that died leaves the send with nobody to carry it; the
        // sender should hear that at once, not after the whole timeout.
        let mut uplink = Uplink::default();
        let now = Instant::now();
        let _ = uplink.take("t".into(), &[], now, HEARTBEAT);
        let _ = uplink.take_pending_park();
        uplink.hand_up(frame("u1"), parked(now + HEARTBEAT * 10));
        assert!(
            uplink.expire(now, HEARTBEAT).sends.is_empty(),
            "a relay that is still attached may yet take it"
        );
        let expired = uplink.expire(now + HEARTBEAT * 2, HEARTBEAT).sends;
        assert_eq!(expired.len(), 1, "failed long before its own deadline");
        assert!(!expired[0].1, "and reported as never taken");
    }

    #[test]
    fn a_timed_out_send_says_whether_any_hub_took_it_and_is_withdrawn() {
        let mut uplink = Uplink::default();
        let now = Instant::now();
        let mut taken_send = parked(now + HEARTBEAT);
        taken_send.correlation_id = "taken".into();
        uplink.hand_up(frame("u-taken"), taken_send);
        let _ = uplink.take("t".into(), &[], now, HEARTBEAT);

        let mut untaken_send = parked(now + HEARTBEAT);
        untaken_send.correlation_id = "never".into();
        uplink.hand_up(frame("u-never"), untaken_send);

        let expired: HashMap<String, bool> = uplink
            .expire(now + HEARTBEAT, HEARTBEAT)
            .sends
            .into_iter()
            .map(|(send, taken)| (send.correlation_id, taken))
            .collect();
        assert_eq!(
            expired.get("taken"),
            Some(&true),
            "a hub saw it: {expired:?}"
        );
        assert_eq!(
            expired.get("never"),
            Some(&false),
            "no hub saw it: {expired:?}"
        );
        assert_eq!(
            uplink.outbound_len(),
            0,
            "a send its caller was told had failed must never be carried late"
        );
    }
}
