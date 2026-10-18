# Structured client transport core

ADR-0036 sections 5/5a define a separate JSON WebSocket protocol. The core in
#1893 is a self-contained router. It starts no listener and changes no running
server: #1894 owns the opt-in loopback listener, HTTP origin/Host and public-funnel
guards, GUI router and browser-code exchange. Mounting it without those hosting
guards is unsupported. `flk web` remains the existing terminal bridge.

The first data message is a generated `hello` frame containing a client token.
The core opens the store once off the async executor, then verifies current
private token storage using S2a's bounded blocking-I/O budget. Unproven hellos
never take the exclusive management/cleanup lock. An absolute five-second deadline covers receipt, verification and the
successful `welcome` write. Missing, malformed, removed or incorrect
credentials fail closed with 4401. Initial hello verification closes with 1001
for a missing store worker, a deadline miss, store I/O failure or unsafe storage.
The client retains its token and reconnects, without gaining admission.
Close delivery is best effort within the failure ceiling; an unusable transport can end without a delivered close frame.
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

There are at most 16 upgraded sockets, including unauthenticated hellos.
A single token, or 16 slow unauthenticated hellos, can occupy all 16 slots.
This slot-hogging limit needs the hosting guards in #1894; the core applies no
per-token fairness or pending-hello quota. Each connection has an eight-message
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
after registration; revocation in that gap closes 4403 without welcome. A
store failure or handshake deadline, during either verification, instead closes 1001,
keeping the credential eligible for reconnect. A failed welcome encode or send drops the
transport without another send or an auth close. Neither failure proves token
revocation (ADR-0036 §5a). No
filesystem watcher or automatic command-time revocation route is added
here; the command slice must recheck storage before dispatch as ADR-0036 requires.
Closing the core cancels pending hellos and authenticated sockets. Explicit
shutdown waits up to five seconds for connection leases and reports failure to
release them; it never treats that ceiling as evidence of success.

Transport tests use an isolated TCP adapter around the real router. Fixture
vectors are selected by their machine-readable `covers` fields. This slice
exercises their hello, token refusal and heartbeat portions only, remapping the
fictional credential to an isolated deterministic token record. It does not
claim full client/subscription-script conformance before the later slices and
S8's complete adapter have landed.
