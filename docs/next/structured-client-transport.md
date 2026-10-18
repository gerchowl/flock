# Structured client transport

ADR-0036 sections 5/5a define a separate JSON WebSocket protocol. The protocol core
from #1893 is mounted by #1894 in the real headless-server lifecycle, alongside
the GUI bundle and browser-code exchange. Build with the `web` feature and set
`[client] bind = "127.0.0.1:7682"` to opt in. Port 0 asks the OS for a free port;
`flk client open --print` reports the live address, including after a config
reload. The listener is off by default. `flk web` remains the separate terminal
bridge.

The first data message is a generated `hello` frame containing a client token.
The core opens the store once off the async executor, then verifies current
private token storage using S2a's bounded blocking-I/O budget. Unproven hellos
never take the exclusive management/cleanup lock. An absolute five-second deadline covers receipt, verification and the
successful `welcome` write. Missing, malformed, removed or incorrect
credentials and explicit authentication-scope refusals fail closed with 4401.
If the first `hello` never arrives before the five-second deadline, the server
closes 1001 and the client retains its token for reconnect. Initial hello
verification also closes with 1001
for a missing store worker, a deadline miss, store I/O failure or unsafe storage.
The client retains its token and reconnects, without gaining admission.
Close delivery is best effort within the failure ceiling; an unusable transport can end without a delivered close frame.
Credentials, raw frames and their parse errors do not enter diagnostics.

`welcome` carries the wire version, server version, verified opaque principal,
random server epoch and 24-hour idempotency-window metadata. The epoch is fresh
per core and never resets durable cursors. A newer client receives the server's
wire version and cannot execute commands (`server_version_gap`). Agent-history
subscriptions and the two supported structured commands are described below.
Binary frames and unknown protocol operations close with 1008.

## Structured commands

S7-min routes `cmd agent.send` and `cmd agent.decide` on the app owner, using
ADR-0036 sections 4/5/5a and the existing structured queue and S6 permission APIs.
`agent.send` uses the socket API's `target`, `text`, `submit` and optional
`item_id` fields. With `submit: true`, its success ack returns `{ item_id }`
after durable queue admission. A single input carries that lifecycle identity
into the harness turn; lifecycle events report subsequent progress. Admission
does not mean the harness has started or completed the turn. `submit: false`
returns `structured_submit_required` before queue or stdin work. Terminal-only
raw/write options are unsupported for structured turns.

`agent.decide` uses the generated `AgentDecide` contract. Echo both the
`request_id` and flk-issued `request_seq` from the open permission record;
never synthesize a sequence. Success returns `{ request_id, decision }` through
the normal ack envelope after S6 reserves the named answer. The agent stream's
closed record remains authoritative for dismissing a permission card. Unknown,
closed, stale and unauthorized decisions keep S6's registered refusal codes.
Neither verb allocates a runtime. Non-local or terminal targets and unsupported
verbs return `unsupported_for_agent` before allocating a lifecycle item.

Send requires `send` scope and decision requires `answer` scope; both shared
services additionally require an `operate` token for operator authority. Every
command rechecks the current token file and scopes while serializing admission
with revocation. A removed/replaced token gets `auth_revoked`; its ack is flushed
before all sockets for that token close 4403. Scope shrink instead returns
`scope` and keeps the socket and allowed reads alive. Already admitted work is
not cancelled by revocation. Deleting a token alone does not proactively close
an idle read-only socket; the next command or reconnect detects it.

Each socket permits four pending commands independently of heartbeat and
subscription tasks. Command IDs are at most 128 bytes; retain at most 1024
identities per connection. A repeated ID is refused as `session_unconfirmed`
after rechecking authorization, even if it is sent again as a first send.
The command ID is claimed only after authorization and provider acceptance;
refusals leave it unclaimed. Exhausting the identity budget returns `session_busy`
before dispatch. The owner mailbox and pending durable receipts each hold
at most 32 entries. Exhausted connection/mailbox capacity closes 1001. Existing
64 KiB message and 45-second blocked-writer bounds apply to command acks too.

This slice implements first sends, not the durable command journal of ADR-0036
section 5a. `resume_only: true` always returns `session_unconfirmed` without
dispatch, even within the advertised recovery window. There is no cached ack
lookup or automatic resend. After uncertain delivery, inspect the queue and
permission state; never manufacture another first-send command to force a
retry. Disconnecting a waiter does not cancel input already durably admitted.
Structured process launch remains separate from this adapter.

Application JSON pings run every 15 seconds independently of other traffic. Each
nonce is unique to its epoch/socket and sequence. Keep at most three outstanding
nonces, and forget each accepted nonce. Only a matching, non-expired JSON pong
renews liveness; replayed, foreign-socket and WebSocket control pongs do not.
No valid pong for 45 seconds ends the socket and releases its resources. The
initial baseline is the successful welcome. A supplied monotonic clock governs
handshake, write and heartbeat deadlines; wall-clock changes grant no liveness.

There are at most 32 HTTP connections before upgrade, plus at most 16 upgraded
sockets. HTTP overflow closes the accepted I/O before reading a request. The entire HTTP connection has a five-second ceiling, including headers,
body receipt, response writes and keepalive. Expiry closes its I/O. Successful
upgrades leave that HTTP lifetime and use the core's independent deadlines. Only
four unverified
hello/recheck attempts may be in flight at once; a verified token principal may
register at most four sockets. Pending overload refuses upgrade with 503; principal overload closes
1001. Existing authenticated connections retain capacity when slow hellos fill
the probation slots. This bounds ownership; repeated local connection spam can
still delay a new connection. It does not grant per-process fairness before
credentials are known. Each connection has an eight-message
data queue and a four-message priority queue.
The writer checks priority between data writes. Frames and reassembled messages
are capped at 64 KiB, and outgoing serialization stops at that cap rather than
first allocating an arbitrarily large JSON string. Compression is never
negotiated. A blocked write fails within 45 seconds; missing pongs retain their
independent original ceiling. Saturated internal queues end the connection
rather than growing a backlog. Oversized server output also ends the socket.

The core owns verified token-to-socket identities until teardown. Command-time
revocation flushes the triggering ack before the 4403 fan-out, within the
existing write/liveness bound, leaving other principals alone.
A verified socket registers before welcome and rechecks current token storage
after registration; revocation in that gap closes 4403 without welcome. A
store failure or handshake deadline, during either verification, instead closes 1001,
keeping the credential eligible for reconnect. A failed welcome encode or send drops the
transport without another send or an auth close. Neither failure proves token
revocation (ADR-0036 §5a). Command-time revalidation uses the token store,
without a filesystem watcher.
Closing the core cancels pending hellos and authenticated sockets. Explicit
shutdown waits up to five seconds for connection leases and reports failure to
release them; it never treats that ceiling as evidence of success.

Transport tests use an isolated TCP adapter around the real router. Fixture
vectors are selected by their machine-readable `covers` fields, remapping the
fictional credential to an isolated deterministic token record. Command tests
drive the `agent-send` and `token-revoked` scripts through real sockets and a
fake structured harness. Full client recovery conformance still requires the
durable command journal and S8's complete adapter.

The mounted listener accepts exactly `127.0.0.1`. Other addresses, including
other members of IPv4's loopback block, receive a typed `NonLoopback` refusal
before the forwarding probe or router construction. The lower-level descriptor
adopter independently rejects sockets outside `127.0.0.0/8` (#1960).
An inherited socket without a configured bind receives `Unconfigured`; an
address or nonzero-port mismatch receives `BindMismatch`. Descriptor adoption
closes a refused socket through its sole owner (#1972).

The listener requires numeric Host and same-origin Origin matching its actual
bound IPv4 authority (normalizing HTTP's default port only). API routes require
Origin; top-level GUI navigation may omit it. Forwarded headers grant nothing.
The GUI index carries CSP with self-only default, script and style sources,
self plus the exact WebSocket URL for connections, and no objects, base URL,
framing or form actions. There is no generated configuration script.

Startup runs `tailscale serve status --json` off the async executor, bounded at
five seconds and 64 KiB. Any serve or funnel configuration targeting the bound
port, unsuccessful or malformed status, and an unavailable probe refuse the
configured listener and server startup; there is no silent fallback. Exit status
is checked before parsing JSON. Forwarding changes after startup are not
monitored. The operator must keep forwarding to this port disabled.

Live handoff transfers the bound listening descriptor rather than rebinding its
port after commit. The importer validates a listening socket and its configured
address, constructs the guarded routers before ready, then accepts only after
commit. Descriptor-extension version 2 makes an older importer refuse before
commit, including handoffs with zero PTYs. Existing sockets reconnect when the
exporter exits. Connections attempted while the exporter is exiting may also be
reset; ownership acknowledgement precedes that exit and clients use §5a's normal
reconnect path. Failed imports preserve the exporter's listener. Received sockets
are close-on-exec so later children cannot retain the port. Process-local browser
codes are not transferred; obtain a fresh URL after handoff or restart. A changed
nonzero configured address requires a restart rather than adopting a mismatched
listener. A fatal accept failure stops only the listener and publishes typed
failed status;
the server loop and its agents continue. Server shutdown cancels owned
HTTP connection tasks, closes accepted I/O and checks core lease release before
returning. If the event loop and listener cleanup both fail, the event-loop error
is returned and the secondary cleanup error is logged by kind and errno.
HTTP/1 half-close handling and admission deadlines are configured on the
owned connection driver.

Completed exchange failures remain uniform 401 with no-store, including oversized bodies
and internal store/worker errors. Only internal typed classes and I/O kinds enter
server diagnostics; neither credentials, one-time codes nor raw errors do.

## Listener recovery foundation (#1894)

The separately reviewed foundation supplies the reusable accept policy. It treats peer resets,
interrupted accepts and other per-connection errors as recoverable and logs them
at debug level. Unsupported accept operations (`EOPNOTSUPP`, `ENOPROTOOPT`),
resource exhaustion and unclassified errors (including errors
without an errno) emit a structured warning and
wait `ACCEPT_RESOURCE_BACKOFF` (100 ms) on the supplied monotonic clock before
accepting again. Only an invalid listener descriptor (`EBADF`, `EINVAL`,
`ENOTSOCK`) ends that listener task and publishes the typed `failed` state once.
It does not end the headless server loop or its agents. Ping carries the optional
`client_listener` state (`listening` or `failed`), absent when disabled.
This state belongs to the logical server and is injected into its API socket
and listener owner. Handoff rollback retains it when replacing the API socket;
independent server instances never share a process static.

On Linux, `MSG_CMSG_CLOEXEC` makes every SCM_RIGHTS descriptor close-on-exec
atomically at receipt. Darwin has no equivalent receive flag: the immediate
fcntl leaves a residual receive-to-fcntl window against concurrent fork/exec.
The importer is already multithreaded: its deadline watchdog starts before
receipt (logging may also own a worker), but those threads do not spawn children.
The importer does not spawn its own children until descriptor protection is done.
Every returned descriptor is protected before import can spawn a child. The listener descriptor helper additionally verifies
that a transferred socket is IPv4, stream-typed and listening and retains exporter rollback ownership
through duplication. This mount integrates the helpers into the listener lifecycle
and the handoff manifest described above.

### Internal history delivery foundation

ADR-0036 §5a's owner bridge is crate-private and has a tested consumer that
recovers real session JSONL and delivers it through the verified socket writer.
WebSocket subscription registration uses that bridge as described below.

Each bridge has a 32-request mailbox and services at most 32 requests per owner
turn. Full admission fails with `WouldBlock`; losing the owner fails with
`BrokenPipe`. The owner captures the registry generation and stored namespace. Unsupported
stored path components receive `invalid_agent` (retry `never`), distinct from an
unknown subject, and never reach filesystem recovery.
Cold recovery defers with `WouldBlock` while a worker is preparing or cleanup
authority is explicitly retained. Once finished and joined, a worker without a
report permits cold recovery. An installed canonical source remains readable.
Retained recovery runs off the async executor, with at most four blocking jobs
per bridge. A cancelled waiter keeps its I/O slot until that job returns,
including when the job is still queued. A blocked filesystem can occupy those
slots; it cannot create an unbounded recovery queue.

Installation rechecks generation and namespace on the owner. A stale generation
or mismatched namespace fails with `Interrupted`. If another recovery or the live producer
already installed the current source, the consumer receives that canonical
source. A cancelled installation request is discarded before changing ownership.
Storage failures propagate without installing a source.

Queued writer deliveries carry an independent subscription liveness fence.
Closing it discards that subscription's queued frames and acknowledges them as
discarded. Sibling deliveries remain eligible, and heartbeat messages retain
priority between writes. A successful acknowledgement means the frame flushed;
a closed acknowledgement channel means the writer failed. A write already
started before the fence closes cannot be recalled, so clients still discard
late frames for closed IDs as required by §5a.

### Agent history subscriptions (ADR-0036 §5a)

`sub agent/<id>` checks the token's current read scope before consulting the
registry owner. Each connection remembers every used ID, including refusals,
unsubscribe and lag. Reuse closes the old registration and receives
`sub_id_reused`. Missing subjects and ahead cursors receive `unknown_stream`
and `cursor_ahead`. Unsafe or reserved namespaces receive `invalid_agent`, not
`unknown_stream`. Scope failure receives `scope`; genuine revocation closes
4403. Storage, worker or resource unavailability closes 1001, preserving the
client's token for reconnection.

Each ID owns one cut registered atomically with its captured head. Durable
records and inline history gaps precede exactly one `synced` barrier; subsequent
records are live. Legacy provenance gaps continue through later records. A known
stored payload that no longer parses emits an inline history `gap` over its
original sequence and continues to later records and the barrier. This records
missing history without reconnect loops or closing sibling IDs. Unknown
wrappers retain the original raw line and observed harness shape, and known
payloads retain their projected blob references. Partial deltas are absent from
durable history. A normal producer end drains already committed records.

There are at most eight active cuts per connection, 1024 lifetime IDs, and one
queued or in-flight delivery per cut. IDs are at most 128 bytes and stream names
256 bytes. Exceeding these budgets closes 1001. The shared writer's data queue
holds eight deliveries, separately from its four priority messages. Page work
runs outside the executor with at most 32 running/queued jobs across connections;
at most 128 bounded active cuts can wait for a page slot, cancellably. Once a
job acquires a slot, cancellation holds capacity until its blocking operation
returns. Exhausted page capacity waits rather than closing sibling cuts. Owner admission
retains its existing 32-message mailbox and four recovery jobs. History retains
at most 16 records/256 KiB per cut, plus one record and a 32 KiB read page.

An event whose encoded frame exceeds 65,536 bytes emits `lagged` and closes only
that ID; it never truncates the payload or kills a sibling. A single stored
record above 256 KiB lags all existing cuts, including caught-up cuts. Lag can
win before the barrier. `skipped_to` is diagnostic, never an applied cursor.
A fresh ID resumes from the client's applied cursor: if the oversized record
is still ahead of that cursor, it lags again rather than silently skipping it.
The server caches up to eight last-flushed cursor/offset hints per connection.
It pins the source notification identity alongside each opaque hint. It uses a
hint only for that exact requested cursor and source; hints that
no longer match the canonical source are discarded in favour of cursor paging.
Flush acknowledgements never advance the client's applied cursor. A resume without
an exact cached hint scans incrementally from the file beginning; no durable
offset index is maintained.

Unsubscribe cancels the cut and fences queued output. A write already started
cannot be recalled, so clients drop late frames for closed IDs. A frame for an
ID never opened on that connection remains a protocol error. A stalled cut has
no second Runtime receiver and cannot monopolize its siblings' queue slots or
heartbeat priority between writes.

Read scope is rechecked at subscription registration, not continuously for live
cuts. Token storage exposes no external scope-change notification. Shrinking
read scope alone leaves existing cuts active until unsubscribe or disconnect;
new subscriptions refuse with `scope`. Explicit revocation fan-out closes 4403
and tears down all cuts for that principal. A storage edit alone is detected at
next admission or reconnect. Continuous read-scope enforcement is tracked in
#2042; this limit matches ADR-0036 §5a.
