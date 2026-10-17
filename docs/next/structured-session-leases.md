# Structured session leases and caller identity

ADR-0036 §1, §1a and §3 require one process per session id and a trusted identity
for its tool calls. The server shares a lease table with its structured runtime
workers. Reserve the session before launch preparation and pass the reservation
to the runtime. Agent, launch-profile session and stream identities must agree.
A duplicate start, resume or in-place fork returns `session_busy` (retry class
`Later`); a fork with a new session id can reserve independently.

A reservation records the agent, its optional parent/spawn depth and, after
spawn, the process PID plus platform start identity. The runtime binds it before
publishing an input handle. The socket captures the peer birth before queueing;
request handling and parked-request replay preserve it and reject stale or missing
birth evidence even after a lease is released. With an empty lease table, a
missing birth retains existing operator behavior; a captured birth mismatch
still refuses a queued or parked request. Socket callers whose verified process ancestry reaches
that process are the agent, including when the process runs under the opt-in
operator UID. Spawn ceilings and delegation lineage use that identity without
inventing a pane. Message send/reply uses the attested agent rather than a claimed
sender. Owner-policy, goal and other operator checks cannot treat that agent as
the human. Existing PTY callers retain their pane identity.

ADR-0036 §1a attributes callers by their own ancestry. Preparation and version
probes are scoped to the launcher's ancestry, with independently proven pane
callers retaining their PTY identity. A launch keeps its uncertain launcher marker
while fork/exec runs without registry locks. Binding captures process identity
outside the locks, then publishes PID/birth before exposing a runtime handle.
A failed birth lookup after PID publication retains that process root, so its
descendants fail closed. If binding times out before publication, the root stays
the server's launcher PID. If release also times out, every unproven server
descendant stays uncertain until the retained reservation is cleared or the
server restarts. Independently proven pane callers and unrelated ancestry remain
available. If spawn panics before
publishing a PID, its launcher ancestry remains uncertain. A proven PTY boundary
retains today's pane identity. Registry reads wait at most 100 milliseconds for
short contention on the primary table; after that deadline, the pre-spawn
published snapshot (also bounded to a 100 millisecond lock wait) still
allows unrelated operator and pane callers to resolve while the launching
process ancestry remains uncertain. If neither registry view is available,
proven pane callers retain their identity and other callers fail closed. macOS uses cross-UID
`KERN_PROC_PID` process facts and the walk stops at PID 1 without querying it.
Missing/reused process facts and ancestry cycles fail closed for affected callers.
Failed preparation or confirmed
spawn failure releases a reservation; confirmed process-tree cleanup releases it
on stop or crash. Panics after a spawn attempt and unconfirmed cleanup retain it,
with a diagnostic. Reservation, spawn-marker publication, binding, release and
Drop each wait at most 100 milliseconds per registry lock. Mutations acquire both
the primary table and published view before changing either; contention leaves
them unchanged. Admission reports `agent_start_failed` on contention. A release
that cannot acquire the locks retains the reservation and logs a warning; a
retained reservation can require a server restart after process cleanup. No
time-based lease expiry grants authority or permits a second process.

This is server-local in-memory bookkeeping. Existing S3a supervision owns process
tree cleanup and stale-session recovery. It does not provide a persistent lease
service across server restarts. The runtime's process identity is distinct from
turn state; an exit code alone is not a successful turn.

Pane authority binds the child PID to the birth captured at spawn. Live handoff
carries that birth as optional metadata; old manifests with no birth cannot grant
pane-based API authority. Missing or mismatched birth fails closed without
promoting the process to operator. Granting pane authority requires birth
validation. PID ancestry candidates still refuse pane callers as inbound relays and preserve
self-kill, self-submit, bulk self-exclusion and restart-warning protections. An
unproven candidate never proves that a caller is outside every pane.
Named hook reconciliation retains its existing
claim-healing behavior; an empty claim used for authority requires trusted birth.
The Mac Nix package explicitly declares its C compiler, and `macos-process-facts`
compiles the SDK helper using that package's compiler and SDK inputs.

The Unix API captures the kernel peer UID (`getpeereid` on macOS,
`SO_PEERCRED` on Linux) with PID and birth before queueing. That context is
preserved across mesh recovery and lifecycle dispatch. The server resolves
`flk-agent` once at construction. Dedicated-user callers never become the
operator, even from a shell pane or after detaching. A dedicated caller with no
verified pane or lease may read, but sends (including raw terminal writes) return
`msg_not_allowed`; other mutations return `operator_only`. Unknown UIDs, missing
credentials and account-directory errors also fail closed for mutations. An
absent dedicated account still permits the operator UID. Restart the server to
pick up account-directory changes or recover from a lookup error.

Pane candidates separate rate-limit and blocking-budget buckets without stamping
sender identity or granting authority. Candidates with no birth proof still
cannot send through the public API.

Public structured start, registry integration and structured `self` targets in
agent restart/list remain #1439; blob storage is #1630. Operator mode remains
cooperative under #1177: same-UID reparented children without lease ancestry
cannot be distinguished from the operator. This slice does not enable structured
launches or change the default dedicated-user launch policy.

Tests use the S1-derived fake Claude with owned Unix sockets, PID/birth evidence,
scripted tool requests and bounded waits. No live-model or operator-profile access
is needed. The runtime tests cover startup failure, crash, stop, duplicate ids,
PID reuse, unknown attribution and retained uncertain cleanup.
