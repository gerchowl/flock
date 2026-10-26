# App-session credentials (#2442)

ADR-0038 §6 and the ADR-0036 "App-session tokens and shell origins (#1601)"
amendment define ephemeral credentials for a native shell. The socket method
`client.app.mint` takes an empty `params` object and returns the existing
`client_token_issued` result (`issued.info` and the secret `issued.token`).
The native process must keep that connection's process alive: invoking a
short-lived CLI subprocess would bind the credential to that subprocess.

Only LocalOperator may mint: a kernel-attested operator UID and PID, no flk
pane in its ancestry (including shell panes), and no inbound mesh edge or
agent lease. Unknown credentials refuse with `operator_only`. The server arms
the exit watch before rereading the accepted process birth or checking ancestry.
Missing/changed birth evidence or a failed watch refuses with
`peer_process_changed` (`retry: never`, CLI exit 2); establish a fresh connection.
The watch remains held through authority checking and issuance, and cannot be
substituted with a watch for another peer. Caller-supplied scopes, PID, origin
port or any other parameter refuse with `invalid_params`.

Scopes are exactly `read`, `send`, `answer`, `operate`. The bearer grants the
operator's console authority while valid. The existing same-UID memory-reading
risk remains tracked by #1177; this is not an agent-isolation boundary.

Socket minting and WebSocket hello/command/subscription admission share one
server-owned memory domain. `authenticate` checks both app credentials and
persistent file credentials. App credentials, digests and watches never enter
`client-tokens/`, file-token listings or handoff snapshots. Restart/handoff
successors create a fresh domain; graceful transport shutdown exclusively
revokes every app credential before stopping its sockets. Persistent file
credentials remain valid across these boundaries.

Each credential expires exactly 12 monotonic hours after issuance, with no
sliding renewal. Renewal issues an independent bearer and token name with the same operator
principal, `app:operator:<uid>` (decimal kernel-attested UID). Identity survives
restart and handoff while credentials do not. Revocation fanout is per
credential; each mint has four registered-socket slots keyed by its bearer
digest, including pending welcome. A fully occupied old mint cannot starve a
fresh mint's socket budget. The four unauthenticated hello slots and sixteen
upgraded sockets remain global. At most
128 app entries live. Issuance first reclaims expired/exited entries; capacity
or admission contention refuses with `session_busy`. No failed mint discloses
a credential. Storage/entropy failures return `internal_error`.

Admission rechecks its original source under the shared filesystem lock, held
through bounded dispatch. Revocation and shutdown publish the existing intent
before waiting for exclusive ownership, preserving file-token admission rules
and the existing two-second acquisition ceiling. Already-admitted effects are
not claimed cancelled. Callbacks cannot recursively manage credentials or wait
for a model.

Idle sessions await the injected monotonic deadline and kernel exit readiness
(kqueue NOTE_EXIT on macOS, pidfd on Linux); no PID liveness polling occurs.
Each reactor waiter owns a separate descriptor for the same persistent watch.
Expiry, process exit and explicit revocation use the existing close 4403,
with no new authentication error names. An in-flight command that detects revocation
flushes its `auth_revoked` error acknowledgement before the close. This drain
uses the existing bounded transport-write window. During server shutdown,
unsettled commands remain uncertain and reconnect through normal recovery.
Failed watch registration or bounded retirement work closes fail-closed with
1001; expired/exited entries still cannot admit and the next
issuance reclaims them. Revocation notifications happen after earlier shared
admissions drain.

The captured shell Origin/header pair is admitted only on `/v1/ws` (#2442).
The sibling listener, pre-auth UID attribution and revised transport budgets
belong to the next approved #2500 slice. Existing pending-hello and connection
limits are unchanged here. There are no `desktop/` changes in this server slice.

The server's transport-neutral `App::admit_local_peer` checks explicit kernel
credentials rather than borrowing the current CLI request's caller context.
It requires a known operator UID, the original live PID/birth, and no pane,
structured-agent or mesh-edge origin. Complete ancestry is supported through
sixteen stable process links, matching the pane-origin resolver; unavailable,
reused or truncated ancestry cannot establish operator authority. The proof owns
its exit watch, which admission rechecks before publishing any registration,
including an exit during the ancestry walk. The watch stays private; `process()`
lends a shared reference so callers cannot replace it while retaining the peer's
credentials.

`server::Store::admit_peer` holds the existing shared admission/revocation fence
through its registration callback. Local app credentials must match the minting
PID and birth; a missing peer proof cannot admit one. Persistent credentials
retain exactly their current grants, including send-only credentials. Watch or
store I/O failures remain distinct from process-instance refusal. This primitive
is staged for the sibling-listener integration in #2500; the TCP hello behavior
switches together with that listener, not in this preparatory slice.

The next preparatory slice supplies `local_listener::Listener`, reusing the
existing HTTP server over Unix streams and capturing credentials from the
accepted kernel socket. `prepare` binds without accepting, `activate` starts
serving only when explicitly called, and `stop` cancels the acceptor and its
owned HTTP tasks. Production startup does not call these methods yet, so this
slice creates no new reachable endpoint and does not change TCP authentication.
Owner admission and the TCP app-session refusal will activate together in the
following #2500 slice.

`socket_path` derives `flock-ws.sock` beside `flock.sock`; an API override such
as `custom.sock` instead derives `custom-ws.sock`, keeping separate instances
in one directory distinct. Preparation requires an existing, real,
operator-owned parent directory without group/other write permission. Callers
must provide a trusted ancestor chain, as for the API socket namespace. It
refuses unsafe parents without creating or repairing them. The socket uses the
API's `0600` mode constant and permanent `SocketOwner` lock. `owner` retains a
rollback reference; its duplicate can pass through the existing SCM_RIGHTS
handoff machinery and be adopted before `prepare`. `unlink` and destruction
remove only the recorded binding's inode. Exporter/importer orchestration and
its manifest extension belong to the activation slice; no new descriptor is
exported by production handoff in this foundation.
