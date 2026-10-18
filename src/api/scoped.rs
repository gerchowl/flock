//! Scoped local requests and connection-local transport context (ADR-0036 §1a).
//! Each fresh connection authenticates independently; continuations retain the
//! revocable generation, not a connection's past authorization result.
use super::error_code::ErrorCode;
use super::{schema::Request, ApiRequestMessage, ApiRequestSender};
use crate::platform::peer::Credentials;
use crate::structured::{
    identity::{Principal, UidPolicy},
    lease::{Caller, ContinuationProof, LeaseCredential, ScopedRefusal, SessionLeases},
};
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    sync::{Arc, OnceLock},
};

/// Wire wrapper. Missing credentials remain scoped and are refused, never legacy.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScopedRequest {
    /// Existing local API request, unchanged.
    pub request: Request,
    /// A reservation credential, not a claimed agent id.
    pub credential: Option<LeaseCredential>,
}

/// Decode without letting a malformed scoped envelope fall through to legacy.
pub(super) fn decode(
    line: &str,
) -> Result<(Request, Option<Option<LeaseCredential>>), Option<serde_json::Error>> {
    let value = super::secret_json::SecretValue::parse(line).map_err(Some)?;
    if value.0.get("request").is_some() || value.0.get("credential").is_some() {
        let scoped: ScopedRequest = ScopedRequest::deserialize(&value.0).map_err(|_| None)?;
        Ok((scoped.request, Some(scoped.credential)))
    } else {
        Request::deserialize(&value.0)
            .map(|request| (request, None))
            .map_err(Some)
    }
}

#[cfg(test)]
type FactReader = fn(u32) -> Option<(u32, u64)>;

/// Sealed transport context shared by queued/parked requests and the socket loop.
#[derive(Clone)]
pub(crate) struct Scope(Arc<ScopeData>);
#[derive(Clone)]
struct ScopeData {
    /// Retained for queued re-admission and zeroized on the final scope drop.
    credential: Option<LeaseCredential>,
    peer: Credentials,
    leases: Arc<OnceLock<SessionLeases>>,
    principal: Arc<OnceLock<(String, u32)>>,
    proof: Arc<OnceLock<ContinuationProof>>,
    probe: bool,
    continuation: bool,
    #[cfg(test)]
    fact_reader: Option<FactReader>,
}
impl std::fmt::Debug for Scope {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("Scope([redacted])")
    }
}
thread_local! { static CURRENT: RefCell<Option<Scope>> = const { RefCell::new(None) }; }

/// Restores the enclosing context even when the connection returns early.
pub(super) struct Guard(Option<Scope>);
impl Drop for Guard {
    fn drop(&mut self) {
        CURRENT.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}
impl Scope {
    /// Context for server-generated continuations on this socket worker only.
    pub(crate) fn current() -> Option<Self> {
        CURRENT.with(|slot| slot.borrow().clone())
    }
    /// Mark only server-generated polls of an already admitted operation.
    /// The marker is private transport state and cannot be supplied on the wire.
    pub(super) fn enter_continuation() -> Guard {
        Guard(CURRENT.with(|slot| {
            let mut slot = slot.borrow_mut();
            let next = slot.as_ref().map(|scope| {
                Self(Arc::new(ScopeData {
                    continuation: true,
                    ..scope.0.as_ref().clone()
                }))
            });
            std::mem::replace(&mut *slot, next)
        }))
    }
    /// Original kernel credentials for server-generated continuations.
    pub(super) fn peer(&self) -> Credentials {
        self.0.peer
    }
    /// Every check consults the current registry and current kernel ancestry.
    pub(crate) fn caller(&self) -> Caller {
        self.0.leases.get().map_or(Caller::Uncertain, |leases| {
            leases.scoped_caller(self.0.credential.as_ref(), self.0.peer)
        })
    }
    /// Sealed agent identity for work past a successful admission boundary.
    /// Admission itself always calls `refusal` afresh. Policy helpers must not
    /// perform a second registry walk and lose a typed busy result mid-dispatch.
    pub(crate) fn principal(&self) -> Caller {
        self.0
            .principal
            .get()
            .map_or(Caller::Uncertain, |(agent, depth)| Caller::Agent {
                agent: agent.clone(),
                depth: *depth,
            })
    }
    /// Fresh admissions consult the table; marked internal polls verify the live
    /// cell and fresh ancestry. A continuation never fails merely on contention.
    pub(crate) fn refusal(&self) -> Option<ErrorCode> {
        if self.0.continuation {
            return self.continuation_refusal();
        }

        match self.caller() {
            Caller::Agent { .. } => None,
            Caller::Busy => Some(ErrorCode::LeaseBusy),
            Caller::AncestryLimit => Some(ErrorCode::LeaseAncestryLimit),
            _ => Some(ErrorCode::NotAuthorized),
        }
    }
    /// Established effects require a live generation and fresh kernel ancestry.
    pub(crate) fn continuation_refusal(&self) -> Option<ErrorCode> {
        let caller = self.0.proof.get().map(|proof| {
            #[cfg(test)]
            if let Some(read) = self.0.fact_reader {
                return proof.verify_with(read).unwrap_or(Caller::Uncertain);
            }
            proof.caller()
        });
        match caller {
            Some(Caller::Agent { .. }) => None,
            _ => Some(ErrorCode::NotAuthorized),
        }
    }
    /// Private App probe: publishes the registry handle only after authentication.
    pub(crate) fn probe_reply(
        &self,
        leases: &SessionLeases,
        policy: &UidPolicy,
    ) -> Option<&'static str> {
        if !self.0.probe {
            return None;
        }
        if policy.classify(self.0.peer.uid) == Principal::Unknown {
            return Some("scoped_refused");
        }
        match leases.admit(self.0.credential.as_ref(), self.0.peer) {
            Ok((Caller::Agent { agent, depth }, proof)) => {
                let _ = self.0.proof.set(proof);
                let _ = self.0.principal.set((agent, depth));
                let _ = self.0.leases.set(leases.clone());
                Some("scoped_ok")
            }
            Err(ScopedRefusal::RegistryContended) => Some("scoped_busy"),
            Err(ScopedRefusal::AncestryBudget) => Some("scoped_ancestry_limit"),
            _ => Some("scoped_refused"),
        }
    }

    /// Authenticate before any fast path; no model-selected request is executed
    /// by the internal probe, including ping or subscription requests.
    pub(super) fn enter(
        credential: Option<LeaseCredential>,
        peer: Credentials,
        request: &Request,
        api_tx: &ApiRequestSender,
    ) -> Result<Guard, ErrorCode> {
        let scope = Self(Arc::new(ScopeData {
            credential,
            peer,
            leases: Arc::new(OnceLock::new()),
            principal: Arc::new(OnceLock::new()),
            proof: Arc::new(OnceLock::new()),
            #[cfg(test)]
            fact_reader: None,
            probe: true,
            continuation: false,
        }));
        let (respond_to, response) = std::sync::mpsc::channel();
        api_tx
            .send(ApiRequestMessage {
                request: request.clone(),
                respond_to,
                peer_pid: peer.pid,
                peer_birth: peer.birth,
                peer_uid: peer.uid,
                scope: Some(scope.clone()),
            })
            .map_err(|_| ErrorCode::NotAuthorized)?;
        match response
            .recv_timeout(super::server::APP_RESPONSE_TIMEOUT)
            .map_err(|_| ErrorCode::NotAuthorized)?
            .as_str()
        {
            "scoped_busy" => return Err(ErrorCode::LeaseBusy),
            "scoped_ancestry_limit" => return Err(ErrorCode::LeaseAncestryLimit),
            "scoped_ok" => {}
            _ => return Err(ErrorCode::NotAuthorized),
        }
        if let Some(code) = scope.refusal() {
            return Err(code);
        }
        let scope = Self(Arc::new(ScopeData {
            probe: false,
            ..scope.0.as_ref().clone()
        }));
        Ok(Guard(CURRENT.with(|slot| slot.borrow_mut().replace(scope))))
    }
}

#[cfg(test)]
pub(crate) mod tests;
