//! Scoped local requests and connection-local transport context (ADR-0036 §1a).
//! Each fresh connection authenticates independently; continuations retain the
//! revocable generation, not a connection's past authorization result.
use super::{schema::Request, ApiRequestMessage, ApiRequestSender};
use crate::platform::peer::Credentials;
use crate::structured::{
    identity::{Principal, UidPolicy},
    lease::{Caller, LeaseCredential, SessionLeases},
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
pub(super) fn decode(line: &str) -> Result<(Request, Option<Option<LeaseCredential>>), ()> {
    let value: serde_json::Value = serde_json::from_str(line).map_err(|_| ())?;
    if value.get("request").is_some() || value.get("credential").is_some() {
        let scoped: ScopedRequest = serde_json::from_value(value).map_err(|_| ())?;
        Ok((scoped.request, Some(scoped.credential)))
    } else {
        serde_json::from_value(value)
            .map(|request| (request, None))
            .map_err(|_| ())
    }
}

/// Sealed transport context shared by queued/parked requests and the socket loop.
#[derive(Clone)]
pub(crate) struct Scope(Arc<ScopeData>);
#[derive(Clone)]
struct ScopeData {
    credential: Option<LeaseCredential>,
    peer: Credentials,
    leases: Arc<OnceLock<SessionLeases>>,
    probe: bool,
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
    /// Private App probe: publishes the registry handle only after authentication.
    pub(crate) fn probe_reply(
        &self,
        leases: &SessionLeases,
        policy: &UidPolicy,
    ) -> Option<&'static str> {
        if !self.0.probe {
            return None;
        }
        if policy.classify(self.0.peer.uid) != Principal::Unknown
            && matches!(
                leases.scoped_caller(self.0.credential.as_ref(), self.0.peer),
                Caller::Agent { .. }
            )
        {
            let _ = self.0.leases.set(leases.clone());
            Some("scoped_ok")
        } else {
            Some("scoped_refused")
        }
    }
    /// Authenticate before any fast path; no model-selected request is executed
    /// by the internal probe, including ping or subscription requests.
    pub(super) fn enter(
        credential: Option<LeaseCredential>,
        peer: Credentials,
        request: &Request,
        api_tx: &ApiRequestSender,
    ) -> Result<Guard, ()> {
        let scope = Self(Arc::new(ScopeData {
            credential,
            peer,
            leases: Arc::new(OnceLock::new()),
            probe: true,
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
            .map_err(|_| ())?;
        if response
            .recv_timeout(super::server::APP_RESPONSE_TIMEOUT)
            .map_err(|_| ())?
            != "scoped_ok"
            || !matches!(scope.caller(), Caller::Agent { .. })
        {
            return Err(());
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
