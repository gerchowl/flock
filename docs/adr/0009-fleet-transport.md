# ADR 0009 — Fleet transport: one held SSH connection per peer, not a replicated log

- Status: Accepted
- Date: 2026-08-04
- Implemented: all of it. `src/peer_stream.rs` holds one long-lived SSH
  connection per peer, with `PeersRelayAttach` / `MessageRelayed`
  (`src/api/schema.rs:140,1748`) as the wire surface.
- Issues: #224 (the transport spike); PRs #225, #226, #227, #228, #229.
  Supersedes the *transport* half of ADR-0008 — its delivery model (messages
  ride the tool surface) stands unchanged. Constrained by ADR-0001 (fleet
  gossip is pull; no push between servers) and ADR-0005 (the durable event log
  is the audit substrate).
- Decision owner: operator; design from four independent expert reviews on
  #224 plus measurements taken on the live tailnet.

The [mesh-mode amendment below](#amendment-2026-10-08-mesh-mode-and-durable-message-custody)
is Proposed separately. The Accepted status and implementation statement above
cover the original held-SSH decision only.

## Context

ADR-0008 shipped agent-to-agent messaging but left its transport dependent on
a **direct SSH edge per sender/recipient pair**. The fleet does not have that
shape: it is hub-and-spoke, the Macs hold the outbound edges, and a spoke has
no edge back. A spoke-to-spoke message had nowhere to go.

The proposal on the table was to **replicate the durable event log** between
nodes and let messages ride it — appealing because the log already exists and
is already the audit substrate.

## What decided it

**Four independent reviews said don't ship it.** The decisive objection was in
flock's own source: `persisted_events_after` carries the docstring *"COLD PATH
— re-reads every log file per call — fine for one-shot queries, wrong for
polling"*, and it was the exact function the design leaned on, at every poll
interval, per peer (~160 MB re-parsed per call). Replication also required
origin-scoped cursors, per-origin dedupe, replica-log integrity, and would put
every message body on every spoke's disk permanently.

**Measurement decided the alternative.** Cold SSH handshake from `atlas`,
`ControlMaster=no ControlPath=none`, exit status verified:

| peer | handshake |
| --- | --- |
| kiln | 0.13s |
| kiln-dev | 0.16s |
| ethz-heimdall (Tailscale-remote) | 0.97s |

Five requests: **1.93s** spawning per call versus **0.38s** over one held
connection — linear versus constant.

There is no cheap peer. The floor is ~130ms per call, so per-call spawning is
wasteful fleet-wide rather than only for remote peers.

## Decision

**Hold one SSH connection per peer, carrying API requests to `flk peers
relay`.** Spoke-to-spoke is one pull hop plus one push hop, so gossip stays
pull and ADR-0001 holds.

1. **The relay is a multiplexer, not a byte pump.** The API server is
   one-request-per-connection, so each line gets its own short-lived local
   socket. Those are free; the SSH connection is what gets amortized.
2. **Liveness is not silence.** On an idle fleet nothing flows for long
   stretches, so a no-traffic timer cannot distinguish healthy from dead.
   Death is *observed*: ssh's own `ServerAliveInterval=5 ServerAliveCountMax=2`
   drops a dead or half-open link and exits, arriving as EOF. The 15s timeout
   covers only what ssh cannot see — a wedged relay behind a healthy
   connection.
3. **The one-shot spawn remains, as a working path.** Primarily a
   *compatibility* path: a peer whose `flk` predates the relay can never hold a
   stream, so during any rollout it is the steady state, not an error. This is
   what makes the change never-worse-than-today.
4. **State pushes as a coalesced summary, not raw events.** The hub already has
   exactly one path that applies a summary, so a push needs no second path to
   drift from it — and a snapshot coalesces by nature, so a 1s debounce drops
   nothing the next push does not already carry.
5. **Throttling is structural, not configurable.** State coalesces because it
   is a snapshot; messages pass through because each is discrete. A per-kind
   cadence knob was reviewed and rejected as a footgun.

## Alternatives considered

**Replicate the event log.** Rejected above. Its one lasting contribution: the
review found a real sequence-regression bug (#225) that the design would have
made load-bearing.

**Tune the poll interval.** Rejected on measurement. A 2s cadence is a 6.5%
duty cycle against the nearest peer and ~50% against the furthest; the spread
is 7.5×, so no single value is right and a per-peer knob relocates the problem
onto the operator.

**A message broker (NATS/JetStream, Iggy).** Correct instinct — do not
hand-roll a distributed log — but wrong conclusion for this fleet. It means a
daemon on every host, a second trust domain alongside the SSH host CA, and an
inbound-reachable port, which *is* the topology problem. The right conclusion
was not to build the distributed log at all.

**SSH `ControlMaster`.** Amortizes handshakes for free, and is already in the
operator's `~/.ssh/config`. Not a substitute: its sockets go stale across
exactly what a roaming hopper does — sleep, wifi↔LTE, Tailscale DERP↔direct —
and then hang rather than reconnect. An app-managed connection with an
explicit fallback handles that; borrowing ssh's multiplexing does not.

## Consequences

- Connection lifecycle is new live state: reconnect, backoff, sleep/wake. The
  one-shot fallback keeps every failure benign.
- Failures back off 60s so an asleep peer cannot become a reconnect storm.
- A customized `summary_command` keeps the one-shot path — it is a shell string
  where the connection carries an API request, and the two coincide only at the
  shipped default.
- Cross-host sends now emit `MessageRelayed` (#228), closing a hole where a
  message that left the machine left no audit record at all.
- **Still open:** a relayed message's outcome lives on the receiving node, so
  `msg status` reports `outcome_known: false` for it. Querying the far side is
  now cheap over the held connection and is the obvious follow-up.

## Measurement note

An earlier round of the numbers above included a "0.01s LAN" figure that was
`exit=255` — a DNS failure timed as though it were a handshake — and it reached
a merged commit message before being caught. `/usr/bin/time -p ssh … | grep
real` times a failure just as happily as a success. Check exit status when
timing network calls.

## Amendment (2026-10-08): mesh mode and durable message custody

- Status: Proposed
- Issues: [#661](https://github.com/gerchowl/flock/issues/661),
  [#623](https://github.com/gerchowl/flock/issues/623),
  [#640](https://github.com/gerchowl/flock/issues/640).
- Decision owner: operator. Acceptance of ADR-0009 above does not accept this
  amendment. No mesh implementation ships with this document.

### Evidence and scope

Laptop reachability determining send is asking for trouble. A sleeping,
roaming or NATed sender must be able to send and receive answers using the
connections it can initiate. Connectivity determines when mail moves, not
whether an accepted conversation has a return address.

The #623 analysis remains applicable to this checkout:
`src/peers.rs::send_peer_message` performs a one-shot SSH command and discards
its successful response body. `src/app/api/messages.rs::route_msg_reply`
resolves the sender afresh and falls back to `hand_up_or_refuse`, not to a
persisted return capability. `hold_reply` retains answers only in the branch
with neither sender agent nor pane. `src/app/mailboxes.rs::DeliveredMeta`
drops the originating host, and `src/api/reply_wait.rs::wait_for_reply` watches
only the local EventHub. An initial delivery therefore proves neither an
attached uplink nor a usable route when the answer is written. The historical
incident's exact topology is not established by these code facts.

Retain the measured held-SSH decision, summary coalescing, explicit liveness
and reconnect backoff above. Replace the hub/spoke routing restriction and
one-hub forwarding ceiling for negotiated mesh traffic. Do not replicate logs
or tune polling intervals to repair message transport. Directory and route
knowledge remain bounded summaries pulled over existing SSH channels under
ADR-0001. Discrete messages, acknowledgements and collection responses can
travel both ways on those channels, as existing uplink traffic already does.

### Topology and routing

Every node is a peer. Any node with configured SSH authority may dial any
reachable configured peer. An authenticated, mesh-capable held edge is usable
in both directions regardless of which node opened it. No node is required
to accept inbound SSH, and edge symmetry grants no new SSH credentials or
operator authority. A spoke-only laptop can keep its current peer config,
open an outbound edge to a reachable hub, upload its outbox and collect replies
on that edge. While it sleeps, a receiver or hub holds the reply. Reconnect
resumes collection without anyone dialing the laptop. A disconnected fleet
cannot deliver immediately: eventual delivery requires a usable path before
expiry, and at least one durable custodian surviving.

Nodes advertise authenticated node identity, supported capabilities, live
adjacencies, route generation and lease expiry, without message bodies.
Advertisements name logical next hops, not arbitrary SSH destinations.
Choose a live shortest path, with a stable node-id tie break, using only
configured outbound peers or authenticated attached edges. Keep no dial task
for an address learned only from gossip or `from_host`.

Replace the one-hub guard in `relay_message_to_host` with a bounded route:
carry the immutable message key, visited node ids and remaining hop budget
(proposed default eight). Refuse a repeated node or exhausted budget for that
attempt, retain custody and seek a fresh route. Duplicate edges and changed
routes do not create new messages. Reject forged origin claims and route
advertisements not bound to the authenticated neighbor. Same-user SSH is the
trust boundary, not Byzantine protection against a compromised trusted hub.

### Durable outbox and acknowledgement contract

Use ADR-0005's durable event store as the source of truth, with an outbox
projection alongside mailbox projections in `src/app/mailboxes.rs`. Persist
an envelope before reporting local acceptance: authenticated origin node and
sender identity, target AgentId, correlation id, optional `in_reply_to`, body,
intent, creation time, absolute expiry, and conversation-bound collection
capability. Persist logical collection peers and custody receipts, never a
live connection handle. A reply has its own message key and references the
request's key, not just its unscoped correlation string.

The idempotency key is `(origin_node_id, correlation_id)`. A retry preserves
that key and immutable envelope. Reuse with different content or target is a
conflict, not an overwrite. Each receiving node durably records the key and
mailbox import together before acknowledging it. At-least-once transmission
thus produces exactly-once mailbox import within the retention contract,
including after a crash between import and acknowledgement. It does not
promise exactly-once agent execution.

Custody and final delivery are separate receipts. A forwarding node persists
before issuing a custody receipt and retains its copy until downstream
custody is durable or a terminal outcome is recorded. The origin retains its
record until final delivery or expiry, even after a hub accepts custody.
Custody transfers and final receipts are themselves replayable and collected
idempotently. A lost acknowledgement triggers a retry of the same key. A hub
restart reconstructs outstanding forwarding and collection from events.

Retry transient disconnection with bounded concurrency and exponential
backoff with jitter, starting at the existing 60-second failed-edge backoff,
capped at five minutes and reset when an authenticated edge reconnects.
The proposed default absolute envelope lifetime is seven days, fixed at
origin and never extended by forwarding. Keep dedupe tombstones for at least
the envelope lifetime plus a 24-hour replay margin, reject expired envelopes,
and retain terminal receipts long enough for origin collection. Outbox byte
and count limits reject new acceptance explicitly, rather than evict accepted
mail silently. These defaults require owner confirmation before shipping.
There is no cross-message ordering guarantee, including within a conversation:
`in_reply_to` supplies causality, not FIFO. Hold an answer whose request has
not yet been imported until the reference can be attached, without losing it.

| Evidence | Sender-visible meaning |
| --- | --- |
| Origin persisted | Accepted locally, queued, possibly offline |
| Hub persisted | Custody accepted, still no final-delivery claim |
| Target server persisted mailbox import | Delivered to recipient inbox |
| Recipient calls genuine `msg.read` | Read |
| Reply persisted at receiver | Answer held, pending return or collection |
| Reply imported at origin | Answer available locally to inbox and wait/status |
| Expiry or authoritative recipient removal | Terminal outcome with reason |

A successful SSH exit or relay write is only transport evidence.
`MessageRelayed` must not stand in for final delivery. Wake/submit attempts
from #640 (`typed`, `submit_sent`, `accepted`, `observed_accepted`,
`unconfirmed`, `abandoned`) are a separate axis: a wake can be accepted while
mail remains unread. Return delivery/read receipts through the same durable
mechanism as answers, so the origin's `msg.status` distinguishes custody,
delivery, read and wake evidence. A wait timeout ends that caller's wait,
not the outbox job. `wait_reply` observes locally imported results and honest
queued/expired outcomes without doing synchronous remote polling.

### Replies and sender-side collection

At request acceptance, retain a return capability bound to authenticated
origin, recipient and full request key in both queued and delivered metadata.
A correlation id is caller-chosen and is not a secret. Mint an unguessable
collection token, authenticate each collection caller and scope results to
that conversation. A forwarded capability is delegated only to authenticated
custodians on the recorded path. Never dial an inbound host label or treat a
caller-supplied hub assertion as attestation.

`route_msg_reply` uses this durable return binding before directory lookup.
Persist the answer even if the sender is absent from the current directory or
its laptop is offline. Try immediate delivery over any negotiated edge,
otherwise report held/queued for collection. `msg_target_not_found` no longer
means an accepted request's known sender is temporarily unreachable.

A bounded asynchronous collection worker at the origin resumes from its
outstanding request records, including without an active CLI waiter. It asks
the original receiver or forwarding custodian for conversation-scoped answers
and delivery receipts over sender-authorized logical edges. A hub collects
from its receiver and holds results for its originating spoke. A one-shot
request's SSH process has already exited: collection opens another authorized
connection rather than pretending to reuse that process. In mesh mode any
live authorized path may carry the request, answer and receipts, while the
original custodian remains a discovery anchor for collection.

Import collected replies through `src/app/api/messages.rs`'s local event and
mailbox path exactly once, then acknowledge collection. Both `msg.status` and
`src/api/reply_wait.rs` observe the same local result, and ordinary inbox wake
rules apply. If the sender has no inbox, retain the result for its waiter and
status. A deferral never replaces an actual answer. Collection must not drain
unrelated receiver inbox messages. Retain terminal results through the agreed
receipt lifetime and expose expiry when the sender returns too late.

### Identity, directory and vanished agents

ADR-0008's AgentId is identity, not an SSH address or a server-local pane id.
`src/app/directory.rs` remains the single resolution surface. Extend its peer
summaries with owning node identity, incarnation/generation and leases. Local
authoritative identity wins over cached entries. A missed summary expires a
route, not the agent's existence: known identity plus stale location is
queued/offline. A never-known fresh target may still be refused as unknown,
while a reply uses its retained return binding even when directory discovery
fails. Movement requires authenticated owner updates, not reinterpretation
of the host substring in an AgentId.

An owning server's explicit removal tombstone terminates mail for that exact
agent incarnation as `recipient_gone`. Propagate that receipt to the origin.
Do not retarget to a new agent occupying the same pane or silently transfer
identity. Without authoritative removal, keep retrying until expiry. A reply
to a removed sender can still be imported into the origin's conversation
status/wait store, but is never delivered to a replacement agent's inbox.

### Components and staged rollout

These are implementation responsibilities, not claims of existing APIs:

- `src/peer_stream.rs` and `src/cli/peers.rs`: negotiate capabilities, frame
  symmetric message/receipt/collection requests and recover authenticated
  edges. Keep summary coalescing separate from discrete durable mail.
- `src/peers.rs`: preserve structured acknowledgements and provide bounded
  one-shot collection fallback where negotiated, instead of discarding output.
- `src/app/api/messages.rs`, `src/app/mailboxes.rs` and event/schema modules:
  outbox, atomic dedupe/import, return bindings, receipt replay and outcomes.
- `src/app/api/uplink.rs`, `src/app/uplink.rs` and runtime worker dispatch:
  hub custody and origin collection off the app/render loop.
- `src/app/directory.rs`: leased identities and routes with owner tombstones.
- `src/remote.rs`: keep thin-client bootstrap and protocol compatibility gates
  separate from fleet routing. A client attachment is not a durable mailbox
  route or permission to turn every remote session into a router.
- `src/api/reply_wait.rs`: consume imported local facts, without networking.

**Step 1 (#623):** durable reply outbox and authenticated sender-side collection
on existing direct or one-hub logical paths, with restart recovery, idempotent
import, delivery receipts and honest pending-collection status. Preserve the
current hub/spoke guard. Ship holding and collection together: holding alone
strands the answer. Choose records and keys that step 2 can reuse.

**Step 2 (#661):** generalise custody to requests and replies, advertise routes,
use either direction of negotiated edges, and add bounded multi-hop routing
and offline/queued reporting. Existing hub/spoke configs become meshes with
fewer edges, requiring no new inbound reachability or implicit SSH grants.

Negotiate a versioned mesh capability on each relay edge. Only capable peers
can accept mesh custody, tokens or routes. Older peers retain existing
one-shot/hub-spoke delivery with an explicit legacy/unknown-outcome warning,
never a mesh durability guarantee. Do not forward multi-hop mesh traffic
through an old peer as if it understood dedupe or receipts. An origin can
retain queued mail awaiting a capable path, or allow an explicitly requested
legacy send with its weaker result. Gate unsupported collection operations
without silently declaring a reply delivered. Audit socket JSON defaults,
durable event replay and bincode wire changes separately. For changes to
`src/protocol/wire.rs`, apply the release-relative protocol bump rule in
AGENTS.md rather than incrementing once per implementation PR.

### Alternatives rejected

**Require reciprocal peer entries or a dialable laptop.** Makes roaming/NAT
an operator configuration failure and repeats #623's asymmetry.

**Hold replies without collection, or only warn about unreachable senders.**
The former strands answers, the latter diagnoses rather than satisfies the
conversation. Reachability snapshots are advisory, not permanent refusal.

**Only a live reverse route.** Useful fast path, but loses the guarantee when
an edge closes between request and answer. Durable custody and pull survive it.

**Mandatory central hub.** Existing hubs remain useful custodians, but making
one indispensable turns its availability into a fleet-wide send prerequisite.

**Replicated logs, a broker, faster polls or ControlMaster alone.** Retain the
rejections and measurements above. Conversation-scoped custody transfers only
required bodies and receipts, not every node's audit log.

### Non-goals, validation and owner questions

No new public listener, SSH credential distribution, global log replication,
keyboard injection, exactly-once agent execution, total ordering, or Byzantine
consensus. The separate #623 hook/MCP availability gap is outside this design.
An offline origin with no surviving disk cannot originate new durable mail.

Implementation acceptance must drive isolated real fleet servers from send
through read, reply and original inbox/wait/status. Cover a one-way-only edge,
a laptop offline then reconnecting, sender/receiver/hub restarts, duplicate
imports and lost acknowledgements, route cycles and exhausted hop budget,
forged tokens/routes/origins, stale directory versus authoritative removal,
expiry and outbox limits, no active waiter, a real answer after deferral, and
mixed-version paths. Existing `tests/support/fleet.rs` and
`tests/mcp_fleet_messaging.rs` supply the harness. This docs-only PR adds no
runtime tests or behavior.

Owner decisions still needed: accept this amendment and its extension of
hub/spoke routing, confirm lifetime/backoff/hop defaults and storage quotas,
choose how long terminal receipts remain queryable after expiry, and approve
the node-identity provisioning and delegated collection-token format. The
identity binding must survive reconnect and restart without treating an SSH
alias or hostname as a cryptographic identity. Implementation must settle
those details before advertising mesh capability.
