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
A bind failure or unconfirmed cleanup retains that root, so its descendants fail
closed without denying unrelated operator and pane callers. If spawn panics before
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

Public structured start, registry integration and structured `self` targets in
agent restart/list remain #1439. #1629 must land before #1439. Kernel UID
attestation and detached anonymous-agent policy are #1629; blob storage is #1630.
The UID slice can enforce the dedicated-user boundary. Operator mode remains
cooperative under #1177: same-UID reparented children without lease ancestry
resolve `Unrelated` until #1629; they cannot be distinguished from the operator. This slice does not enable structured
launches or change the default dedicated-user launch policy.

Tests use the S1-derived fake Claude with owned Unix sockets, PID/birth evidence,
scripted tool requests and bounded waits. No live-model or operator-profile access
is needed. The runtime tests cover startup failure, crash, stop, duplicate ids,
PID reuse, unknown attribution and retained uncertain cleanup.
