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
never take the exclusive management/cleanup lock. An absolute five-second
deadline covers receipt, verification and the successful `welcome` write. Missing, malformed, unsafe, removed or incorrect
credentials fail closed with 4401. Close delivery is best effort within the
failure ceiling; an unusable transport can end without a delivered close frame.
Credentials, raw frames and their parse errors do not enter diagnostics.

`welcome` carries the wire version, server version, verified opaque principal,
random server epoch and 24-hour idempotency-window metadata. The epoch is fresh
per core and never resets durable cursors. A newer client receives the server's
wire version and no command capability. This transport-only slice advertises no
capabilities to any client; subscriptions and command dispatch arrive in their
already assigned later slices. Binary frames and other non-protocol post-hello
data operations are policy violations and close with 1008.
There is no command admission, structured start or claim of durable journaling.

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

The core owns verified token-to-socket identities until teardown. The future
command slice calls its 4403 fan-out only after the triggering auth_revoked ack
has been written, within the existing write/liveness bound. The primitive closes
all registered sockets for that principal, leaving other principals alone.
A verified socket registers before welcome and rechecks current token storage
after registration; revocation in that gap closes 4403 without welcome. No
filesystem watcher or automatic command-time revocation route is added
here; the command slice must recheck storage before dispatch as ADR-0036 requires.
Closing the core cancels pending hellos and authenticated sockets. Explicit
shutdown waits up to five seconds for connection leases and reports failure to
release them; it never treats that ceiling as evidence of success. Concurrent
and repeated shutdown calls share a successful completion without sending
another cancellation. A timed-out or cancelled caller may retry the drain.

Transport tests use an isolated TCP adapter around the real router. Fixture
vectors are selected by their machine-readable `covers` fields. This slice
exercises their hello, token refusal and heartbeat portions only, remapping the
fictional credential to an isolated deterministic token record. It does not
claim full client/subscription-script conformance before the later slices and
S8's complete adapter have landed.

The mounted listener accepts exactly `127.0.0.1`. Other addresses, including
other members of IPv4's loopback block, receive a typed `NonLoopback` refusal
before the forwarding probe or router construction. The lower-level descriptor
adopter independently rejects sockets outside `127.0.0.0/8` (#1960).
If the configured bind was removed, the importer accepts the server handoff,
closes the inherited listener through its sole owner, and reports `disabled`.
An address or nonzero-port mismatch still receives `BindMismatch` (#1972).

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
is checked before parsing JSON. The running listener rechecks on config reload
and every 60 seconds, using a monotonic supplied clock. Reload requests during
an in-flight check retain a subsequent check; concurrent bursts coalesce.
An exposure stops only the listener and its connections, reports
`exposed_by_serve`, and emits a warning. A failed safety probe also stops only
the listener, reporting `failed`; raw probe output is never logged. Server
work and agent PTYs continue. Clearing forwarding does not automatically
restart a stopped listener; a fresh server or handoff can activate it safely.

Live handoff transfers the bound listening descriptor rather than rebinding its
port after commit. Export snapshots the descriptor and its manifest flag under
the exposure-stop ownership lock: if stopping wins, export records no listener;
if export wins, its owned descriptor survives closure of the serving copy.
An activation address-query failure clears the endpoint and socket and reports
`failed`, just like other listener failures. The importer validates a listening socket and its configured
address, constructs the guarded routers before ready, then accepts only after
commit. Descriptor-extension version 2 makes an older importer refuse before
commit, including handoffs with zero PTYs. Existing sockets reconnect when the
exporter exits. Connections attempted while the exporter is exiting may also be
reset; ownership acknowledgement precedes that exit and clients use §5a's normal
reconnect path. Failed imports preserve the exporter's listener. Received sockets
are close-on-exec so later children cannot retain the port. Process-local browser
codes are not transferred; obtain a fresh URL after handoff or restart. A changed
nonzero configured address refuses import rather than adopting a mismatched
listener. Removing the bind instead disables the inherited listener without
refusing the server handoff. A fatal accept failure stops only the listener and publishes typed
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
`client_listener` state (`listening`, `failed`, `disabled`, or
`exposed_by_serve`); older or non-web servers may omit it.
This state belongs to the logical server and is injected into its API socket
and listener owner. Handoff rollback retains it when replacing the API socket;
independent server instances never share a process static.

Listener diagnostics use `client.listener.accept` for classified accept errors,
`client.listener.probe_failed` for forwarding-guard failures, and
`client.listener.shutdown_failed` for secondary cleanup errors. Address lookup
failure during activation emits `client.listener.address_failed` with its I/O
kind and errno, and publishes `failed` directly without a transient `listening`
state. It is distinct from a forwarding probe failure: no recheck probe ran.
These events do not include raw error text, probe output or credentials.

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
