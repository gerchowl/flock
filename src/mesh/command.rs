//! Dormant M1 command transport. M2 owns destination admission, M3 return binding.
//! The home signature covers the immutable budget, never the forwarding header.
//! Authenticated trusted hops accumulate residence, including fleet pause and
//! suspend. A compromised trusted hop can under-report elapsed time: ADR-0026
//! does not promise Byzantine-safe authorization. Commands never enter mail custody.
use super::{
    hello,
    identity::NodeIdentity,
    key::MessageKey,
    sign,
    store::{Envelope, Kind, ReturnBinding},
    version_gate,
};
use crate::{api::client_tokens::Scope, platform::suspend_clock, terminal::AgentId};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, time::Duration};

mod provenance;
pub(crate) use provenance::Provenance;

pub(crate) const BUDGET_MS: u64 = 30_000;

/// Authenticated principal class: App is a process-bound user, Agent a live lease.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TokenSource {
    App,
    Agent,
    File,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Target {
    Agent(String),
    Workspace(String),
}

/// Home-attested facts, supplied only after authenticated admission.
/// Digests commit to arguments without transporting raw secrets or credentials.
/// Source records provenance; it does not grant remote lifecycle authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Body {
    pub home_node_id: String,
    pub principal: String,
    pub token_source: TokenSource,
    pub scopes: BTreeSet<Scope>,
    pub destination_node_id: String,
    pub target: Target,
    pub cmd_id: String,
    pub verb: String,
    pub args_digest: [u8; 32],
    pub requested_scope: Scope,
    pub budget_ms: u64,
}

/// Only elapsed residence crosses nodes, never local clock readings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Frame {
    envelope: Envelope,
    elapsed_ms: u64,
}

fn enabled(next_version: u32) -> Result<(), &'static str> {
    let minimum = version_gate::introduced("command").ok_or("unsupported_for_agent")?;
    if hello::window().1 < minimum || next_version < minimum {
        return Err("unsupported_for_agent");
    }
    Ok(())
}

fn valid_node(node: &str) -> bool {
    node.len() == 64
        && node
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

impl Body {
    fn validate(&self) -> Result<(), &'static str> {
        let target = match &self.target {
            Target::Agent(id) => AgentId::is_well_formed(id),
            Target::Workspace(id) => {
                self.verb == "agent.start"
                    && crate::workspace::public_workspace_number(id).is_some()
            }
        };
        if !valid_node(&self.home_node_id)
            || !valid_node(&self.destination_node_id)
            || !target
            || (self.verb == "agent.start" && !matches!(self.target, Target::Workspace(_)))
            || self.principal.is_empty()
            || self.principal.len() > 256
            || self.principal.chars().any(char::is_control)
            || self.cmd_id.is_empty()
            || self.cmd_id.len() > 128
            || self.verb.is_empty()
            || self.verb.len() > 128
            || self.budget_ms != BUDGET_MS
        {
            return Err("invalid_envelope");
        }
        if self.token_source == TokenSource::Agent && !AgentId::is_well_formed(&self.principal) {
            return Err("invalid_envelope");
        }
        if self.token_source == TokenSource::File
            && matches!(
                self.verb.as_str(),
                "agent.start" | "agent.stop" | "agent.restart" | "agent.resume"
            )
        {
            return Err("not_authorized");
        }
        if !self.scopes.contains(&self.requested_scope) {
            return Err("not_authorized");
        }
        Ok(())
    }
}

impl Frame {
    /// Refuse before minting an envelope when either endpoint cannot use VERSION 6.
    pub(crate) fn originate(
        mut body: Body,
        identity: &NodeIdentity,
        next_version: u32,
        provenance: &Provenance<'_>,
    ) -> Result<Self, &'static str> {
        enabled(next_version)?;
        provenance.bind(&mut body)?;
        body.validate()?;
        if body.home_node_id != identity.node_id() {
            return Err("origin_mismatch");
        }
        let key = MessageKey::mint(identity.node_id(), 0).map_err(|_| "internal_error")?;
        let mut envelope = Envelope {
            kind: Kind::Command,
            origin_key: vec![],
            signature: vec![],
            key: key.clone(),
            sender: body.principal.clone(),
            target_agent: match &body.target {
                Target::Agent(id) => id.clone(),
                Target::Workspace(_) => String::new(),
            },
            target_session: String::new(),
            correlation_id: body.cmd_id.clone(),
            in_reply_to: None,
            request_key: None,
            return_binding: ReturnBinding {
                request: key,
                recipient_node: body.destination_node_id.clone(),
                collection_token: vec![],
                collection_peers: vec![],
            },
            intent: body.verb.clone(),
            body: serde_json::to_vec(&body).map_err(|_| "invalid_envelope")?,
        };
        sign::seal(&mut envelope, identity);
        Ok(Self {
            envelope,
            elapsed_ms: 0,
        })
    }

    /// The signer establishes the home, never the identity of the last hop.
    /// This verifies attestation and expiry only; M2 must enforce local grants.
    pub(crate) fn verify(&self) -> Result<Body, &'static str> {
        enabled(hello::window().1)?;
        sign::verify(&self.envelope)?;
        let body: Body =
            serde_json::from_slice(&self.envelope.body).map_err(|_| "invalid_envelope")?;
        body.validate()?;
        let agent = match &body.target {
            Target::Agent(id) => id.as_str(),
            Target::Workspace(_) => "",
        };
        if self.envelope.sender != body.principal
            || self.envelope.target_agent != agent
            || !self.envelope.target_session.is_empty()
            || self.envelope.correlation_id != body.cmd_id
            || self.envelope.intent != body.verb
            || self.envelope.in_reply_to.is_some()
            || self.envelope.request_key.is_some()
        {
            return Err("invalid_envelope");
        }
        if !self.envelope.key.is_valid()
            || self.envelope.kind != Kind::Command
            || body.home_node_id != self.envelope.key.origin_node
            || body.destination_node_id != self.envelope.return_binding.recipient_node
            || self.envelope.return_binding.request != self.envelope.key
            || !self.envelope.return_binding.collection_token.is_empty()
            || !self.envelope.return_binding.collection_peers.is_empty()
        {
            return Err("origin_mismatch");
        }
        if self.elapsed_ms >= body.budget_ms {
            return Err("message_expired");
        }
        Ok(body)
    }

    /// A signed hub refusal, never an admission/result ack. M3 binds its return.
    /// No custody row or reusable collection credential is created.
    pub(crate) fn refused(
        &self,
        identity: &NodeIdentity,
        code: &'static str,
    ) -> Result<Envelope, &'static str> {
        let mut receipt = self.envelope.clone();
        receipt.kind = Kind::Receipt;
        receipt.key = MessageKey::mint(identity.node_id(), 0).map_err(|_| "internal_error")?;
        receipt.request_key = Some(self.envelope.key.clone());
        receipt.return_binding.request = receipt.key.clone();
        receipt.return_binding.recipient_node = self.envelope.key.origin_node.clone();
        receipt.body = serde_json::to_vec(&serde_json::json!({"state":"refused","detail":code}))
            .map_err(|_| "internal_error")?;
        sign::seal(&mut receipt, identity);
        Ok(receipt)
    }
}

/// Local residence evidence is deliberately not serializable. Recovery without
/// evidence refuses; deserializing a frame must never create a fresh deadline.
pub(crate) struct Hop {
    frame: Frame,
    since: Option<Duration>,
}

/// A hub sends either the command or its terminal signed refusal, never custody.
#[derive(Debug)]
pub(crate) enum Prepared {
    Send(Frame),
    Refused(Envelope),
}

impl Hop {
    /// Call at live authenticated ingress or origin, using the suspend clock.
    /// Recovery must pass None rather than a new clock reading.
    pub(crate) fn receive(frame: Frame, since: Option<Duration>) -> Result<Self, &'static str> {
        frame.verify()?;
        Ok(Self { frame, since })
    }

    /// Refusal is constructed before returning to the transport; an old next hop
    /// receives no command bytes and the hub keeps no deferred command row.
    pub(crate) fn prepare(
        &mut self,
        next_version: u32,
        clock: &impl suspend_clock::Read,
        identity: &NodeIdentity,
    ) -> Result<Prepared, &'static str> {
        match self.forward(next_version, clock) {
            Ok(frame) => Ok(Prepared::Send(frame)),
            Err(code) => self.frame.refused(identity, code).map(Prepared::Refused),
        }
    }

    /// Recheck at every dequeue/reroute, even after a peer renegotiates lower.
    /// The caller reads `suspend_clock::now` at ingress and every forward.
    /// A failed clock or backward reading permanently loses residence evidence.
    fn forward(
        &mut self,
        next_version: u32,
        clock: &impl suspend_clock::Read,
    ) -> Result<Frame, &'static str> {
        let since = self.since.take().ok_or("message_expired")?;
        let now = suspend_clock::now(clock).map_err(|_| "message_expired")?;
        let residence = now.checked_sub(since).ok_or("message_expired")?;
        let ms = residence
            .as_nanos()
            .div_ceil(1_000_000)
            .min(u128::from(u64::MAX)) as u64;
        self.frame.elapsed_ms = self.frame.elapsed_ms.saturating_add(ms);
        self.since = Some(now);
        self.frame.verify()?;
        if let Err(code) = enabled(next_version) {
            self.since = None;
            return Err(code);
        }
        Ok(self.frame.clone())
    }
}

// VERSION 6 is reachable only through debug-only negotiation overrides in M1.
#[cfg(all(test, debug_assertions))]
pub(crate) mod tests;
