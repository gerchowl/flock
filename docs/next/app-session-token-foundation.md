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
sliding renewal. Renewal issues an independent bearer and principal. At most
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
Pre-auth UID attribution and revised budgets remain held for an owner decision
in #2500. Existing pending-hello and connection limits are unchanged. There
are no `desktop/` changes in this server slice.
