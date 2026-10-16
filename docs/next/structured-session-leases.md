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
birth evidence even after a lease is released. Socket callers whose verified process ancestry reaches
that process are the agent, including when the process runs under the opt-in
operator UID. Spawn ceilings and delegation lineage use that identity without
inventing a pane. Message send/reply uses the attested agent rather than a claimed
sender. Owner-policy, goal and other operator checks cannot treat that agent as
the human. Existing PTY callers retain their pane identity.

Pending launches, missing/reused process facts, ancestry cycles and an unavailable
lease table fail closed. While any launch is pending, lease attribution is unavailable for all callers:
its unknown process must not be mistaken for an ancestor agent or the operator. Registry reads
never block the server on a runtime worker's lock. Failed preparation or confirmed
spawn failure releases a reservation; confirmed process-tree cleanup releases it
on stop or crash. Panics after a spawn attempt and unconfirmed cleanup retain it,
with a diagnostic. No time-based lease expiry grants authority or permits a
second process.

This is server-local in-memory bookkeeping. Existing S3a supervision owns process
tree cleanup and stale-session recovery. It does not provide a persistent lease
service across server restarts. The runtime's process identity is distinct from
turn state; an exit code alone is not a successful turn.

Public structured start and registry integration remain #1439. Kernel UID
attestation and detached anonymous-agent policy are #1629; blob storage is #1630.
The UID slice can enforce the dedicated-user boundary. Operator mode remains
cooperative under #1177: a reparented same-UID child without lease ancestry cannot
be distinguished from the operator. This slice does not enable structured
launches or change the default dedicated-user launch policy.

Tests use the S1-derived fake Claude with owned Unix sockets, PID/birth evidence,
scripted tool requests and bounded waits. No live-model or operator-profile access
is needed. The runtime tests cover startup failure, crash, stop, duplicate ids,
PID reuse, unknown attribution and retained uncertain cleanup.
