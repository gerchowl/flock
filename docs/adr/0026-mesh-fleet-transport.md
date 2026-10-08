
# ADR 0026 — Mesh fleet transport and durable message custody

- Status: Proposed
- Date: 2026-10-08
- Issues: [#661](https://github.com/gerchowl/flock/issues/661),
  [#623](https://github.com/gerchowl/flock/issues/623),
  [#640](https://github.com/gerchowl/flock/issues/640).
- Decision owner: operator. Proposed extension of ADR-0009 and clarification
  of ADR-0008, superseding their affected transport contracts only if accepted.
- Implemented: none. No mesh implementation ships with this document.

## Evidence and scope

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
and reconnect backoff in ADR-0009. Replace the hub/spoke routing restriction and
one-hub forwarding ceiling for negotiated mesh traffic. Do not replicate logs
or tune polling intervals to repair message transport. Directory and route
knowledge remain bounded summaries pulled over existing SSH channels under
ADR-0001. Discrete messages, acknowledgements and collection responses can
travel both ways on those channels, as existing uplink traffic already does.

## Topology and routing

Every node is a peer. A **dial-capable** node has configured SSH authority
and can initiate an edge. An **undialable** node cannot accept inbound SSH,
but may still be dial-capable. A **spoke with no outbound edges** initiates
none and relies on an inbound edge opened by a hub. A hub forwards or holds
custody for others, rather than being a distinct identity class. A laptop
need not be a spoke: ADR-0009's Macs hold outbound edges.

Any dial-capable node may dial reachable configured peers. An authenticated,
mesh-capable held edge carries traffic in both directions regardless of its
initiator, without granting SSH credentials or operator authority. An
undialable laptop uploads mail and receives replies over an edge it opens.
A spoke with no outbound edges receives the hub's pushes down the edge the
hub opened. No node must be dialable, but every delivering component needs
some established edge. During disconnection custody persists, and eventual
delivery requires a usable path before expiry and a surviving custodian.

Nodes advertise authenticated node identity, supported capabilities, live
adjacencies and route generation, without message bodies. Every learned route
is tied to the held edge and generation that supplied it: withdraw it when
that edge closes, or when an explicit withdrawal arrives. Propagate topology
changes over surviving held edges and pull the current summary on reconnect.
Silence never expires a healthy route. There is no route refresh cadence or
time lease. Retain last-known owners as offline identity hints, never live
routes. SSH EOF/liveness and the wedged-relay timeout remain ADR-0009's rule 2.

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

## Durable outbox and acknowledgement contract

**Recommended: a dedicated SQLite custody store** at
`session::data_dir()/mesh-mail.sqlite`, independent of ADR-0005's rotating
audit log. Use the existing SQLite dependency, WAL transactions and
`synchronous=FULL`, with durable file/directory creation. The store owns
outbox envelopes, inbox imports and read state, dedupe tombstones, collection
capabilities, node identity bindings and final outcomes. Commit envelope,
dedupe and mailbox import atomically before acknowledging acceptance. Recover
mailbox/outbox projections from this store, not from audit events. Single
writer ownership must transfer during live handoff before the new server
accepts mail. Corrupt/unwritable storage refuses durable acceptance loudly,
never degrades this contract to memory-only mail.

Persist authenticated origin node and sender identity, target AgentId, a
sender-server-minted message ULID, caller correlation id for threading,
optional `in_reply_to` plus the referenced request message key, body, intent,
remaining TTL and conversation-bound collection capability. Persist logical
collection peers and receipts, never live connection handles. A reply gets a
new server-minted message id. The idempotency key is
`(origin_node_id, message_id)`, not caller-chosen correlation id. Retransmits
reuse that key and immutable content, conflicting reuse is refused. A caller
retry that creates a new ULID is a new message, so return the minted key on
acceptance and provide an authenticated retry operation for that same key.
At-least-once transport produces exactly-once mailbox import within the
retention contract, not exactly-once agent execution.

Bound the store independently: recommended 256 MiB total logical data and
10,000 active envelopes per node, including retained inbox bodies, with a
reserved 16 MiB within that bound for dedupe and terminal receipts. Check
quota inside the acceptance transaction. When full, return `mail_store_full`
without custody acknowledgement, leaving the upstream/origin responsible.
Refuse new originating sends explicitly. Never evict accepted unexpired mail
to admit new mail. Terminal receipts use reserved space, and if even that is
exhausted, pause imports until GC restores space. GC removes bodies only after
their custody/inbox retention condition is satisfied, and tombstones/outcomes
only after their own retention deadlines. Checkpoint/truncate WAL and reclaim
pages to bound physical overhead, with a hard disk-reserve guard that refuses
writes before exhausting the filesystem.

Events stay audit and notification only. Today hubs can append forwarded
bodies through message events. For mesh records, bodies live solely in the
custody store: emit metadata (message key, identities, state, reason and
receipt reference), never forwarded bodies or collection tokens. Local inbox,
wait and status read the store and use events only to wake observers. Audit's
rotation or memory-only degradation cannot delete accepted mail or dedupe.

Custody and final delivery are separate receipts. A forwarding node persists
before issuing a custody receipt and retains its copy until downstream
custody is durable or a terminal outcome is recorded. The origin retains its
record until final delivery or expiry, even after a hub accepts custody.
Custody transfers and final receipts are themselves replayable and collected
idempotently. A lost acknowledgement triggers a retry of the same key. A hub
restart reconstructs outstanding forwarding and collection from the store.

Retry transient disconnection with bounded concurrency and exponential
backoff with jitter, starting at the existing 60-second failed-edge backoff,
capped at five minutes and reset when an authenticated edge reconnects.
Use a seven-day TTL budget rather than comparing clocks across hosts.
Each custodian starts a local deadline at receipt using the transferred
remaining budget, subtracts elapsed residence time before forwarding, and
persists that deadline for restart. Duplicate receipt never resets it. Nodes
must maintain a nondecreasing local expiry clock across restarts (clamp wall
clock rollback to the last persisted value), so clock rollback cannot revive
expired mail. No inter-host skew bound is required. Each origin, hub and
receiver expires its copy independently, without assuming a synchronous
fleet-wide expiry instant. Reject exhausted budgets. Keep local dedupe
records at least through the admitted remaining TTL plus 24 hours, and
retain final outcomes for seven days after local termination. A receiver's
retry budget cannot be increased by duplicate transfers. Quota admission
reserves the metadata needed to honor these deadlines.
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

## Replies and sender-side collection

At request acceptance, retain a return capability bound to authenticated
origin, recipient and full request key in both queued and delivered metadata.
A correlation id is caller-chosen and is not a secret. Mint an unguessable
collection token, authenticate each collection caller and scope results to
that conversation. With same-user SSH, this token scopes requests but is not
a secrecy boundary against trusted peers or the same OS user. A forwarded capability is delegated only to authenticated
custodians on the recorded path. Never dial an inbound host label or treat a
caller-supplied hub assertion as attestation.

`route_msg_reply` uses this durable return binding before directory lookup.
Persist the answer even if the sender is absent from the current directory or
its laptop is offline. Try immediate delivery over any negotiated edge,
otherwise report held/queued for collection. `msg_target_not_found` no longer
means an accepted request's known sender is temporarily unreachable.

A bounded asynchronous collection worker at the origin resumes from its
outstanding request records without an active CLI waiter. It asks the original
receiver or forwarding custodian for scoped answers and receipts. Persist
those custodian node ids at send time, so collection can address them without
requiring the original SSH alias to be reachable. The original one-shot SSH
process is gone: later collection uses a new authorized connection or an
already-open edge, not that process.

**Mesh collection uses relayed collection, not proactive reply replication.**
If (a) a laptop wakes where the original holder is not directly reachable,
it addresses collection to that holder's node id through another connected
hub. The hub relays the authenticated, conversation-scoped query along a
loop-free live route and returns results along the request path, taking
durable custody before acknowledging any returned answer. The original
holder does not spray replies at hubs that might later be reachable. This
keeps body copies and authority bounded to participating paths. If there is
no path from the new hub to any recorded custodian or receiver, collection
remains queued until one appears or TTL expires. A mesh cannot retrieve a
physically partitioned holder, and no replication guarantee is implied.

If (b) a spoke has no outbound edges, its hub collects on its behalf and
pushes answers and receipts down the inbound-capable held edge the hub opened.
The spoke can also request collection in the reverse direction on that edge.
Push and relayed collection converge on the same durable import/dedupe
transaction. When the edge is absent, the hub retains custody until reconnect
or expiry. This applies equally to requests addressed to that spoke.

Import collected replies through `src/app/api/messages.rs`'s local event and
mailbox/store path exactly once, then acknowledge collection. Both
`msg.status` and
`src/api/reply_wait.rs` observe the same local result, and ordinary inbox wake
rules apply. If the sender has no inbox, retain the result for its waiter and
status. A deferral never replaces an actual answer. Collection must not drain
unrelated receiver inbox messages. Retain terminal results through the agreed
receipt lifetime and expose expiry when the sender returns too late.

## Identity, directory and vanished agents

ADR-0008's AgentId is identity, not an SSH address or a server-local pane id.
`src/app/directory.rs` remains the single resolution surface. Extend its peer
summaries with owning node identity, incarnation/generation and edge-bound
route provenance. Local authoritative identity wins over cached entries. A
closed advertising edge withdraws its routes, not the agent's existence: known identity plus stale location is
queued/offline. A never-known fresh target may still be refused as unknown,
while a reply uses its retained return binding even when directory discovery
fails. Movement requires authenticated owner updates, not reinterpretation
of the host substring in an AgentId.

The owning server's agent lifecycle handler emits and persists an explicit
removal tombstone for that AgentId/incarnation on explicit kill, permanent
pane close without a retained resumable session, or confirmed agent exit with
no resumable session. Process exit alone, transient disconnect, hibernation,
and a pane replacement during planned resume do not emit removal. The
lifecycle handler consults persisted session/resume state before declaring
permanent removal. Transport workers do not infer death from missing gossip.

[#582](https://github.com/gerchowl/flock/issues/582)'s restart and same-session
resume must persist the AgentId and resume binding before stopping the old
process, then attach the resumed process/pane to that same identity. If resume
fails, report offline/resume_failed and retain mail pending explicit kill or
TTL, rather than falsely announcing removal or assigning a new identity.
A deliberate fresh session uses a new AgentId, never inherits old mail.

A removal tombstone terminates pending mail for that exact incarnation as
`recipient_gone`, returned to sender status/wait with the lifecycle reason.
Without it the sender sees queued/offline until delivery or expiry. A reply
to a removed sender still enters its origin's conversation status/wait store,
but never a replacement agent's inbox. Removal does not rewrite existing read
or delivery evidence.

## Cross-host replies and confirmations (ADR-0008 clarification)

This proposal preserves ADR-0008's tool inbox and sender-authority boundary.
Agent identity, location and reachability are distinct. A sender AgentId
names a reply recipient but does not prove a live return channel. `from_host`
is never dialing authority. Queued and delivered metadata retain the
transport-attested conversation binding, independent of directory freshness.

The originating server receives replies either by collection over any
available authenticated edge (including one opened by another node), or by
a custodian's push down that edge. A sender with no inbox still gets results
through its local status/wait store. Delivery and genuine read receipts use
the same custody/collection mechanism. Delivered means durable target inbox
import, not SSH success, hub custody, accepted wake or recipient read. #640's
wake evidence remains separate. Wait/status read the dedicated local store,
not remote polling or an audit ring whose records may rotate away.

`msg.read`'s `replyable` reports a valid durable return binding even when the
sender is offline, with queued/collection-pending explanation. A legacy
message must disclose its weaker contract. Unknown identity, permanent removal
and expiry differ from temporary routing loss. Transport attestation and
collection tokens never make agent traffic an operator instruction.

## Components and staged rollout

These are implementation responsibilities, not claims of existing APIs:

- `src/peer_stream.rs` and `src/cli/peers.rs`: negotiate capabilities, frame
  symmetric message/receipt/collection requests and recover authenticated
  edges. Keep summary coalescing separate from discrete durable mail.
- `src/peers.rs`: preserve structured acknowledgements and provide bounded
  one-shot collection fallback where negotiated, instead of discarding output.
- `src/app/api/messages.rs`, `src/app/mailboxes.rs` and event/schema modules:
  dedicated custody store, atomic dedupe/import, return bindings and outcomes.
  Audit schemas carry metadata only, while wait/status query durable records.
- `src/app/api/uplink.rs`, `src/app/uplink.rs` and runtime worker dispatch:
  hub custody and origin collection off the app/render loop.
- `src/app/directory.rs`: edge-bound routes and lifecycle-backed owner tombstones.
- `src/remote.rs`: keep thin-client bootstrap and protocol compatibility gates
  separate from fleet routing. A client attachment is not a durable mailbox
  route or permission to turn every remote session into a router.
- `src/api/reply_wait.rs`: consume imported local facts, without networking.

**Step-1 prerequisite: node identity bootstrap and persistence.** Recommended:
generate an Ed25519 keypair once in the state directory with mode 0600 and
fsync, derive node id from the public key, and pin the public key/node id
exchanged on the first authenticated held SSH edge. Reconnect proves
possession against that pinned binding. Reject unexpected replacement and
require operator re-enrollment, rather than silently trusting a new host label.
Retain identity across server restarts and upgrades, and bind any one-shot
collection fallback to the enrolled node. Prevent cloned state directories
from concurrently claiming the same identity. Final provisioning and key
format remain an owner decision, but step 1 cannot defer stable identity to
step 2 because its dedupe and collection authorization depend on it.

**Step 1 (#623):** dedicated store, minted message ids, durable reply outbox
and authenticated sender-side collection on existing direct or one-hub logical
paths, with restart recovery, idempotent
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

## Alternatives rejected

**Require reciprocal peer entries or a dialable laptop.** Makes roaming/NAT
an operator configuration failure and repeats #623's asymmetry.

**Hold replies without collection, or only warn about unreachable senders.**
The former strands answers, the latter diagnoses rather than satisfies the
conversation. Reachability snapshots are advisory, not permanent refusal.

**Only a live reverse route.** Useful fast path, but loses the guarantee when
an edge closes between request and answer. Durable custody and pull survive it.

**Mandatory central hub.** Existing hubs remain useful custodians, but making
one indispensable turns its availability into a fleet-wide send prerequisite.

**Extend audit-event retention to support custody.** Rejected. ADR-0005
rotates at 32 MB or 100,000 events and retains four archives, so neither a
seven-day hold nor eight-day dedupe window survives arbitrary event volume.
Time retention would change the audit subsystem's bounded-storage and failure
posture. A dedicated store provides atomic import and independent quota/GC.

**Proactively replicate replies toward potentially reachable hubs.** Rejected:
adds body copies and retention obligations without guaranteeing the laptop's
next network. Relayed collection uses the available path to an actual holder.

**Replicated logs, a broker, faster polls or ControlMaster alone.** Retain
ADR-0009's rejections and measurements. Conversation-scoped custody transfers only
required bodies and receipts, not every node's audit log.

## Non-goals and validation

No new public listener, SSH credential distribution, global log replication,
keyboard injection, exactly-once agent execution, total ordering, or Byzantine
consensus. The separate #623 hook/MCP availability gap is outside this design.
An offline origin with no surviving disk cannot originate new durable mail.

Implementation acceptance must drive isolated real fleet servers from send
through read, reply and original inbox/wait/status. Cover a one-way-only edge,
a laptop offline then reconnecting, sender/receiver/hub restarts, duplicate
imports and lost acknowledgements, route cycles and exhausted hop budget,
forged tokens/routes/origins, stale directory versus authoritative removal,
expiry and outbox limits, audit rotation during a held conversation, no active
waiter, a real answer after deferral, and mixed-version paths. Also cover a
laptop collecting through a different hub, a spoke with no outbound edges,
quiet healthy edges retaining routes, closed edges withdrawing routes, and
#582 restart/resume retaining identity without a removal tombstone. Existing `tests/support/fleet.rs` and
`tests/mcp_fleet_messaging.rs` supply the harness. This docs-only PR adds no
runtime tests or behavior.

## Owner decisions

Each recommendation below requires owner acceptance before implementation
advertises mesh capability:

1. **TTL and resource defaults:** recommend seven days of remaining TTL,
   retry backoff 60 seconds to five minutes with jitter, hop limit eight,
   256 MiB logical store including a 16 MiB metadata reserve, and 10,000 active
   envelopes. Bound WAL/disk overhead with checkpoints and a disk-reserve guard.
2. **Storage:** recommend the dedicated SQLite store with FULL durability and
   transactional inbox/dedupe import. Audit events remain metadata only.
3. **Node identity:** recommend a once-generated persisted Ed25519 keypair,
   public-key-derived id, first-edge SSH-authenticated enrollment and pinned
   possession checks. Operator re-enrollment is required on key replacement.
4. **Agent removed:** recommend explicit kill or permanent close/exit only
   when no retained resumable session exists. Preserve AgentId through #582
   restart/resume, and report resume failure as offline until kill or expiry.
5. **Final-outcome retention:** recommend seven days after local termination,
   independent of audit rotation. After GC, return `outcome_retention_elapsed`
   for queries carrying an expired, authenticated message reference rather
   than inventing a delivery result.
6. **Old peers:** recommend explicit opt-in legacy sends with weaker-guarantee
   warnings. Default durable sends stay queued for a capable path, or are
   refused before acceptance when only a legacy route is available and the
   caller does not accept queuing. Never silently downgrade accepted custody.
7. **Hub body retention:** recommend bodies only in the bounded custody store,
   delete after durable downstream custody transfer (retain metadata/receipt)
   or local expiry, and never append them to mesh audit events. Origins and
   final inbox owners retain their own copies under their respective TTL/read
   rules. Hubs can read bodies as trusted same-user custodians, and do not
   replicate them to speculative future hubs.
