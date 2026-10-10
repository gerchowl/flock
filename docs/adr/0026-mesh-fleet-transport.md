# ADR 0026 — Mesh fleet transport and durable message custody

- Status: Accepted
- Date: 2026-10-08
- Issues: [#661](https://github.com/gerchowl/flock/issues/661),
  [#623](https://github.com/gerchowl/flock/issues/623),
  [#640](https://github.com/gerchowl/flock/issues/640).
- Decision owner: operator, accepted 2026-10-08 with the defaults and further
  rulings recorded in the Decisions section below.
  Extends ADR-0009 and clarifies ADR-0008, superseding their affected transport
  contracts. The amendments below record the final implementation decisions.
- Implemented: v1.0.0 (#623 step 1, #661 step 2)

## Evidence and scope

Laptop reachability determining send is asking for trouble. A sleeping,
roaming or NATed sender must be able to send and receive answers using the
connections it can initiate. Connectivity determines when mail moves, not
whether an accepted conversation has a return address.

The pre-mesh #623 analysis established the original failure:
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
and reconnect backoff in ADR-0009. Replace the hub/spoke routing restriction
and one-hub forwarding ceiling for mesh traffic. Supersede the one-shot
compatibility fallback: mesh requires a held edge with matching protocol. Do
not replicate logs or tune polling intervals to repair message transport.
Directory and route knowledge remain bounded summaries pulled over existing
SSH channels under ADR-0001. Discrete messages, acknowledgements and
collection responses travel both ways on those channels. The acceptor signals
pending work with a wake push; the dialer pulls it over the held edge.

## Topology and routing

Every node is a peer. A **dial-capable** node has configured SSH authority
and can initiate an edge. A **dialable** node accepts inbound SSH from an
authorized peer. An **undialable** node cannot accept inbound SSH, but may
still be dial-capable. These are independent properties. A **spoke with no
outbound edges** initiates none and must be dialable by its hub to receive
the edge that hub opens. An undialable node with no outbound edges has no
transport path until one of those conditions changes. A hub forwards or
holds custody for others, rather than being a distinct identity class. A
laptop need not be a spoke: ADR-0009's Macs hold outbound edges.

Any dial-capable node may dial reachable configured peers. An authenticated,
mesh-capable held edge carries traffic in both directions regardless of its
initiator, without granting SSH credentials or operator authority. An
undialable laptop uploads mail and receives replies over an edge it opens.
A spoke with no outbound edges receives the hub's pushes down the edge the
hub opened. No node must be dialable, but every delivering component needs
some established edge. During disconnection custody persists, and eventual
delivery requires a usable path before expiry and a surviving custodian.

Nodes advertise authenticated node identity, mesh protocol version, live
adjacencies and route generation, without message bodies. Every learned
route is tied to the held edge and generation that supplied it: withdraw it
when that edge closes, or when an explicit withdrawal arrives. Propagate
topology changes over surviving held edges and pull the current summary on
reconnect. Silence never expires a healthy route. There is no route refresh
cadence or time lease. Retain last-known owners as offline identity hints,
never live routes. SSH EOF/liveness and the wedged-relay timeout remain
ADR-0009's rule 2.

Advertisements name logical next hops, not arbitrary SSH destinations.
Choose a live shortest path, with a stable node-id tie break, using only
configured outbound peers or authenticated attached edges. Keep no dial task
for an address learned only from gossip or `from_host`.

Replace the one-hub guard in `relay_message_to_host` with a bounded route:
carry the immutable message key, visited node ids and remaining hop budget
(default eight). Refuse a repeated node or exhausted budget for that
attempt, retain custody and seek a fresh route. Duplicate edges and changed
routes do not create new messages. Reject forged origin claims and route
advertisements not bound to the authenticated neighbor. Same-user SSH is the
trust boundary, not Byzantine protection against a compromised trusted hub.

## Durable outbox and acknowledgement contract

**Use a dedicated SQLite custody store** at
`config::state_dir()/mesh-mail.sqlite`, independent of ADR-0005's rotating
audit log. Use the existing SQLite dependency, WAL transactions and
`synchronous=FULL`, with durable file/directory creation. The store owns
outbox envelopes, inbox imports and read state, dedupe tombstones,
collection capabilities, node identity bindings and final outcomes. Commit
envelope, dedupe and mailbox import atomically before acknowledging
acceptance. Recover mailbox/outbox projections from this store, not from
audit events. Single writer ownership must transfer during live handoff
before the new server accepts mail. Corrupt/unwritable storage refuses
durable acceptance loudly, never degrades this contract to memory-only mail.

Persist authenticated origin node and sender identity, target AgentId and
session id, a sender-server-minted message ULID, caller correlation id for
threading, optional `in_reply_to` plus the referenced request message key,
body, intent, remaining TTL and conversation-bound collection capability.
Persist logical collection peers and receipts, never live connection
handles. A reply gets a new server-minted message id. The idempotency key is
`(origin_node_id, message_id)`, not caller-chosen correlation id.
Retransmits reuse that key and immutable content, conflicting reuse is
refused. A caller retry that creates a new ULID is a new message, so return
the minted key on acceptance and provide an authenticated retry operation
for that same key. At-least-once transport produces exactly-once mailbox
import within the retention contract, not exactly-once agent execution.

Bound the store independently: 256 MiB total logical data and
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

A full receiver mailbox is a retryable delivery refusal, not a terminal
outcome. The current custodian retains the message and retries until the
seven-day custody TTL expires. The local unread inbox TTL is separately 24
hours from durable inbox import, and does not shorten pre-delivery custody
TTL. Inbox expiry is recorded as unread-inbox expiry without rewriting the
already-confirmed delivery receipt. Custody, dedupe and final outcomes keep
their independent retention rules.

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
capped at five minutes and reset when an authenticated edge reconnects. Use
a seven-day TTL budget of unpaused time rather than comparing clocks across
hosts. Each custodian starts a local deadline at receipt using the
transferred remaining budget, subtracts elapsed residence time before
forwarding, and persists that deadline for restart. Duplicate receipt never
resets it. Nodes must maintain a nondecreasing local expiry clock across
restarts (clamp wall clock rollback to the last persisted value), so clock
rollback cannot revive expired mail. The clamp prevents rollback, not
forward wall-clock jumps: a forward jump can expire mail early and later
correction cannot revive it. This conservative failure is acceptable to
preserve bounded retention, and produces an explicit expiry outcome rather
than a delivery claim. No inter-host skew bound is required. Each origin,
hub and receiver expires its copy independently, without assuming a
synchronous fleet-wide expiry instant. Reject exhausted budgets. Keep local
dedupe records at least through the admitted remaining custody TTL plus 24
hours, and retain final outcomes for seven days after local termination. A
receiver's retry budget cannot be increased by duplicate transfers. Quota
admission reserves the metadata needed to honor these deadlines. There is no
cross-message ordering guarantee, including within a conversation:
`in_reply_to` supplies causality, not FIFO. Hold an answer whose request has
not yet been imported until the reference can be attached, without losing
it.

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

## Origin policy and fleet pause

`[msg] allow_from` matches the ORIGIN node, authenticated by its persisted
node key and mapped to its locally stored pin name: a configured peer name
or the name pinned at inbound first contact. An advertised name alone is not
a policy binding. Display node ids through that pin name in status, routing
and policy diagnostics. Never match the
caller-controlled host label or a forwarding hub's identity. Hubs traversed
by a message are not checked against `allow_from`, so an allowed hop cannot
launder a disallowed origin. Forwarding preserves the authenticated origin
binding end to end, and an unknown/unbound origin cannot borrow a hub's
name.

`flk fleet pause` freezes custody completely on the paused node: no new send
acceptance, retry, forwarding, collection/import, inbox delivery or expiry.
Return an explicit paused refusal to incoming custody attempts and leave the
upstream custodian responsible. Existing accepted records remain intact. The
custody and inbox TTL clocks, as well as retention GC clocks, stop while
paused. Persist remaining budgets and pause state so restart during pause
cannot spend TTL or expire mail. Resume from those remaining budgets without
charging elapsed paused wall time, including a forward clock jump during
pause. The conservative forward-jump expiry rule applies only to unpaused
clock advancement. Edge maintenance and read-only status can continue, but
cannot mutate or delete custody records while paused.

## Replies and sender-side collection

At request acceptance, retain a return capability bound to authenticated
origin, recipient and full request key in both queued and delivered
metadata. A correlation id is caller-chosen and is not a secret. Mint an
unguessable collection token, authenticate each collection caller and scope
results to that conversation. With same-user SSH, this token scopes requests
but is not a secrecy boundary against trusted peers or the same OS user. A
forwarded capability is delegated only to authenticated custodians on the
recorded path. Never dial an inbound host label or treat a caller-supplied
hub assertion as attestation.

`route_msg_reply` uses this durable return binding before directory lookup.
Persist the answer even if the sender is absent from the current directory
or its laptop is offline. Try immediate delivery over any authenticated mesh
edge, otherwise report held/queued for collection. `msg_target_not_found` no
longer means an accepted request's known sender is temporarily unreachable.

A bounded asynchronous collection worker at the origin resumes from its
outstanding request records without an active CLI waiter. It asks the
directly connected receiver or custodian for scoped answers and receipts.
Custodian node ids survive loss of an SSH alias. Collection runs on a held
authenticated edge; beyond that neighbor, routed custody supplies the
return path described below. There is no one-shot message or collection
fallback.

**Amendment Q1 (owner accepted 2026-10-09): route-driven custody forwarding
replaces the relayed collection query.** If a laptop reconnects through a
different hub, the holder forwards its single custody copy toward the
origin node once a live route appears, or offers it for the next-hop
neighbor to pull. Requests, answers and delivery/read receipts use the same
bounded routing and durable import mechanism. No synchronous collection
query is relayed through a chain of hubs, and replies are not replicated to
speculative future hubs. Without a route to a surviving custodian, mail
remains held until a path appears or its TTL expires.

A spoke with no outbound edges signals pending work with `mesh.wake` on the
hub-opened edge. The hub pulls neighbor-addressed custody with
`mesh.collect` and pushes mail toward the spoke with `mesh.deliver`.
Follow-up answers remain eligible after an earlier final answer; a wake
can prompt another pull without a permanent conversation poll. Import
commits before acknowledgement. When the edge disappears, its custodian
retains mail until reconnect or expiry.

Import collected replies through `src/app/api/messages.rs`'s local event and
mailbox/store path exactly once, then acknowledge collection. Both
`msg.status` and `src/api/reply_wait.rs` observe the same local result, and
ordinary inbox wake rules apply. If the sender has no inbox, retain the
result for its waiter and status. Mute deferrals are reply producers on this
same durable path: mint a reply message id, retain custody, route/collect
and dedupe just like an agent's answer. Delete the separate SSH deferral
hop. A deferral never replaces an actual answer. A channel-push original is
settled on durable acceptance of its reply or deferral into custody, not on
a transient relay write or remote read, so retries cannot produce repeated
channel originals. Settling this producer-side original does not claim final
reply delivery or mailbox read. Collection must not drain unrelated receiver
inbox messages. Retain terminal results through the agreed receipt lifetime
and expose expiry when the sender returns too late.

## Identity, directory and vanished agents

ADR-0008's AgentId is identity, not an SSH address or a server-local pane
id. `src/app/directory.rs` remains the single resolution surface. Extend its
peer summaries with owning node identity, incarnation/generation and
edge-bound route provenance. Local authoritative identity wins over cached
entries. A closed advertising edge withdraws its routes, not the agent's
existence: known identity plus stale location is queued/offline.

**Amendment Q3 (owner accepted 2026-10-09):** a new send whose owner is
outside the direct/one-hop gossip horizon and has no stored owner hint is
refused as unknown. A known-but-offline owner is queued. Replies use the
stored return binding at any depth, even when directory discovery fails.
Movement requires authenticated owner updates, not reinterpretation of the host
substring in an AgentId.

The owning server's agent lifecycle handler emits and persists an explicit
removal tombstone keyed on `(AgentId, session_id)` on explicit kill,
permanent pane close without a retained resumable session, or confirmed
agent exit with no resumable session. Process exit alone, transient
disconnect, hibernation, and a pane replacement during planned resume do not
emit removal. The lifecycle handler consults persisted session/resume state
before declaring permanent removal. Transport workers do not infer death
from missing gossip.

[#582](https://github.com/gerchowl/flock/issues/582)'s restart and same-session
resume must persist the AgentId and resume binding before stopping the old
process, then attach the resumed process/pane to that same identity. If resume
fails, report offline/resume_failed and retain mail pending explicit kill or
TTL, rather than falsely announcing removal or assigning a new identity.
A deliberate fresh session uses a new AgentId, never inherits old mail.

A removal tombstone terminates pending mail for that exact AgentId/session
as `recipient_gone`, returned to sender status/wait with the lifecycle
reason. Without it the sender sees queued/offline until delivery or expiry.
A reply to a removed sender still enters its origin's conversation
status/wait store, but never a replacement agent's inbox. Removal does not
rewrite existing read or delivery evidence.

## Cross-host replies and confirmations (ADR-0008 clarification)

This decision preserves ADR-0008's tool inbox and sender-authority boundary.
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

`msg.read`'s `replyable` reports a valid durable return binding even when
the sender is offline, with queued/collection-pending explanation. Unknown
identity, permanent removal and expiry differ from temporary routing loss.
Transport attestation and collection tokens never make agent traffic an
operator instruction.

Terminal refusals by the owning node are recipient-signed receipts
([#872](https://github.com/gerchowl/flock/issues/872)). When the owner behind
a forwarding hub refuses a message with a policy reason (`refused` plus the
reason) or because the target was removed (`recipient_gone`), it signs a
receipt bound to the message's return binding. The hub takes custody of that
receipt before recording the refusal and routes it back like delivery and
read receipts. The origin accepts it only under the recipient node's
signature, so a hub cannot mint a refusal for another node. Refusals a hub
decides on its own are not signed by the owner. The receipt travels in an
optional `receipt` field of the outbound acknowledgement, with a new
`refused` receipt state, so it requires mesh protocol 5.

Known limitations, fixed in v1.1.0: failures decided at an intermediate node
(hop budget, loop, no route) are not routed back to the sender
([#876](https://github.com/gerchowl/flock/issues/876)), and routed receipts do
not carry expiry or elapsed outcome retention to multi-hop origins
([#858](https://github.com/gerchowl/flock/issues/858)). In both cases the
origin keeps the message in custody until its own deadline.

## Components and implementation steps

Implementation responsibilities:

- `src/peer_stream.rs` and `src/cli/peers.rs`: require matching mesh protocol, carry
  messages and receipts in both directions using delivery, wake pushes and
  dialer collection, and recover authenticated edges. Keep summary coalescing
  separate from discrete durable mail.
- `src/peers.rs`: replace one-shot message sends with held-edge custody and
  structured acknowledgements, exposing edge refusal reasons in peer status.
- `src/app/api/messages.rs`, `src/app/mailboxes.rs` and event/schema modules:
  dedicated custody store, atomic dedupe/import, return bindings and outcomes.
  Audit schemas carry metadata only, while wait/status query durable records.
- `src/app/api/edges.rs` and `src/app/edges.rs`: authenticate per-edge relay
  attachments, replacing the single message uplink. Runtime workers handle
  mesh custody, deferral production and collection off the app/render loop.
- `src/app/directory.rs`: edge-bound routes and lifecycle-backed owner tombstones.
- `src/remote.rs`: keep thin-client bootstrap and protocol compatibility gates
  separate from fleet routing. A client attachment is not a durable mailbox
  route or permission to turn every remote session into a router.
- `src/api/reply_wait.rs`: consume imported local facts, without networking.

**Step-1 prerequisite: node identity bootstrap and persistence.** Generate
an Ed25519 keypair once in the per-user `config::state_dir()`, with mode
0600 and fsync, and derive node id from the public key. Never put the
identity in `~/.config`, which may be dotfile-synced. Persist a local
machine binding alongside it, validate the binding at startup/enrollment and
refuse an identity copied to another machine. Report cloned identity
explicitly and require a new identity/enrollment there, not concurrent reuse
of the key.

Pin the public key/node id exchanged on the first authenticated held SSH
edge, and map it to the configured peer name for display and policy.
Reconnect proves possession against that pinned binding. Reject unexpected
replacement and require operator re-enrollment. Retain identity across
restarts and upgrades on the same machine. A direct peer must establish a
held edge for enrollment and keep a working held edge for messaging or
collection. Step 1 cannot defer identity to step 2 because dedupe and
collection authorization depend on it.

**Step 1 (#623):** dedicated store, minted message ids, durable reply outbox
and authenticated sender-side collection on existing direct or one-hub logical
paths, with restart recovery, idempotent
import, delivery receipts and honest pending-collection status. This milestone
preserved the hub/spoke guard until step 2 replaced it. Holding and collection
were implemented together: holding alone
strands the answer. Choose records and keys that step 2 can reuse.

**Step 2 (#661):** generalise custody to requests and replies, advertise
routes, use either direction of authenticated mesh edges, and add bounded
multi-hop routing and offline/queued reporting. Replace the
one-relay-per-server limit with one relay attachment per edge, so multiple
authenticated neighbors can coexist. Existing reachable hub/spoke topology
becomes a mesh with fewer edges, requiring no new inbound reachability or
implicit SSH grants, after the coordinated configuration upgrade below.

The two steps are implementation milestones within the same mesh series, not
mixed-version releases. Both use held mesh transport, and the old
uplink/one-shot paths are removed before the coordinated release.

## Rollout: one coordinated upgrade

Mesh ships as a clean breaking release, **v1.0.0**, under semantic
versioning. Upgrade the whole fleet together in one rollout. Implementation
commits that change the wire use `!` in the conventional subject or a
`BREAKING CHANGE` footer. This ADR does not itself bump package versions or
publish a release.

At each held-edge handshake require identical mesh protocol versions. Refuse
a mismatch with a clear error naming both local and remote versions and
`upgrade flk on <peer>` using the configured peer name. v1.0.0 speaks mesh
protocol 5. Protocol 4 existed only in development builds, whose nodes deny
the new acknowledgement fields and would reject whole batches, so they are
refused at hello. The previous release, v0.11.0, has no mesh at all: its
server rejects `mesh.hello` as an unknown method, and an older relay prints
CLI usage instead of answering. Both are refused as
`upgrade flk on <peer> (peer runs a pre-mesh flk)` with the long refusal
backoff, never the transient retry schedule. Nothing queues for
that incompatible peer and nothing is downgraded. Refuse new acceptance
addressed through a known incompatible edge, including collection jobs for
it. Existing custody remains stored rather than being sent to an
incompatible peer. Ordinary offline queuing for enrolled matching-protocol
destinations remains, subject to custody TTL, and is not a compatibility
mechanism.

A peer that cannot hold an edge, such as an old `flk` or a failing stream,
is refused until fixed (see Amendment #844 for `summary_command`).
`flk status` and `flk peers` name the configured peer and concrete reason
(pre-mesh flk, protocol/version unsupported or
stream failure). No one-shot or alternate-message transport is attempted.

Delete the old message uplink, one-shot message relay, separate deferral SSH
hop and their related configuration keys in the mesh series, without a
deprecation window. This includes `[msg] uplink_timeout_secs`,
`uplink_heartbeat_secs`, `deferral_relay_concurrency` and custom peer
`summary_command` compatibility behavior. There are
no legacy acknowledgements, legacy gates or opt-in legacy sends. Review
`src/protocol/wire.rs` under the repository's release-relative version rule
while implementing the breaking wire, rather than bumping it in this doc PR.

**Amendment #844 (owner accepted 2026-10-09):** removed keys warn and are
ignored rather than refused. Startup, live handoff and reload strip
`[msg] uplink_timeout_secs`, `uplink_heartbeat_secs`,
`deferral_relay_concurrency` and `[[peers]] summary_command`, apply every
other setting, and keep the peer, which then holds an ordinary mesh edge.
Warnings name the file, line and key in `flk status` and on CLI stderr;
`flk config check [--json]` lists removed and unknown keys before a rollout
and exits 1 when there are warnings. Startup, handoff and reload are never
refused for a removed key. This supersedes the earlier "reject removed keys"
rule and the refusal of peers with a custom `summary_command`.

**Amendment Q2 (owner accepted 2026-10-09):** dev-build stores at schema
v1–v11 upgrade in place, idempotently, to the v1.0.0 schema baseline (v12).
Do not discard live custody to upgrade. Before rollout, make a consistent
SQLite backup of `mesh-mail.sqlite` and preserve the identity and both
`mesh-quarantine.jsonl` sidecar files. Copying only the main database while
its WAL writer is active is not a consistent backup. Quarantined rows are
archived with their original typed columns before bodies are cleared; a
failed archive write leaves the row intact. Migration failures name the
store path and recovery hint; moving it aside starts empty and loses its
custody, so preserve a backup before any such recovery.

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
next network. Route-driven custody forwarding transfers one copy along an
actual route, retaining it until downstream durable acceptance.

**Replicated logs, a broker, faster polls or ControlMaster alone.** Retain
ADR-0009's rejections and measurements. Conversation-scoped custody transfers only
required bodies and receipts, not every node's audit log.

## Non-goals and validation

No new public listener, SSH credential distribution, global log replication,
keyboard injection, exactly-once agent execution, total ordering, or Byzantine
consensus. The separate #623 hook/MCP availability gap is outside this design.
An offline origin with no surviving disk cannot originate new durable mail.

Implementation acceptance must drive isolated real fleet servers from send
through read, reply and original inbox/wait/status. Cover a one-way-only
edge, a laptop offline then reconnecting, sender/receiver/hub restarts,
duplicate imports and lost acknowledgements, route cycles and exhausted hop
budget, forged tokens/routes/origins, stale directory versus authoritative
removal, expiry and outbox limits, audit rotation during a held
conversation, no active waiter, a real answer after deferral, and refusal of
mismatched protocols. Also cover a laptop collecting through a different
hub, a spoke with no outbound edges, quiet healthy edges retaining routes,
closed edges withdrawing routes, and #582 restart/resume retaining identity
without a removal tombstone. Existing `tests/support/fleet.rs` and
`tests/mcp_fleet_messaging.rs` supply the harness. Also require end-to-end
coverage of origin
allowlists across an allowed forwarding hub, pause/resume and restart while
paused with no TTL consumption, mailbox-full retry versus 24-hour inbox
expiry, durable channel-original settlement, cloned identity refusal,
per-edge relay attachments and explicit refusal diagnostics for unsupported
summary commands or failed held streams.

## Decisions (accepted by the owner 2026-10-08)

The owner accepted the defaults and made further rulings on 2026-10-08 after
the port-versus-supersede spike. These rulings replace the previous old-peer
compatibility decision. Implementation must satisfy all decisions below
before advertising mesh capability:

1. **TTL and resource defaults:** use seven days of remaining TTL,
   retry backoff 60 seconds to five minutes with jitter, hop limit eight,
   256 MiB logical store including a 16 MiB metadata reserve, and 10,000 active
   envelopes. Bound WAL/disk overhead with checkpoints and a disk-reserve guard.
2. **Storage:** use the dedicated SQLite store with FULL durability and
   transactional inbox/dedupe import. Audit events remain metadata only.
3. **Node identity:** use a once-generated persisted Ed25519 keypair,
   public-key-derived id, per-user state-dir storage and machine binding,
   first-edge SSH-authenticated enrollment and pinned possession checks.
   Operator re-enrollment is required on key replacement.
4. **Agent removed:** declare removal on explicit kill or permanent close/exit
   only when no retained resumable session exists. Preserve AgentId through #582
   restart/resume, key tombstones on AgentId plus session id, and report resume
   failure as offline until kill or expiry.
5. **Final-outcome retention:** retain outcomes for seven days after local
   termination, independent of audit rotation. After GC, return `outcome_retention_elapsed`
   for queries carrying an expired, authenticated message reference rather
   than inventing a delivery result.
6. **Clean break:** ship v1.0.0 and upgrade the fleet together. Require identical
   mesh protocol at the edge handshake, naming both versions and
   `upgrade flk on <peer>` on refusal. No legacy path, downgrade or queuing
   for incompatible peers. Remove message uplink, one-shot relay, deferral hop
   and related keys as part of the series, with no deprecation window.
7. **Hub body retention:** retain bodies only in the bounded custody store,
   delete after durable downstream custody transfer (retain metadata/receipt)
   or local expiry, and never append them to mesh audit events. Origins and
   final inbox owners retain their own copies under their respective TTL/read
   rules. Hubs can read bodies as trusted same-user custodians, and do not
   replicate them to speculative future hubs.
8. **Origin policy and display:** `[msg] allow_from` matches the node-key-
   authenticated origin under its local pin name (configured or inbound
   first-contact), never intermediate hubs. Advertised names alone confer no
   policy authority.
9. **Fleet pause:** freeze all custody activity, including sends, retries,
   delivery and expiry. Stop TTL/retention clocks while paused, including
   across restart, and resume without charging paused time.
10. **Held-edge requirement:** refuse custom summary transports, old peers and
    failing streams until fixed. `flk status` and `flk peers` expose the reason.
11. **Reply producers and settlement:** mute deferrals use the same durable
    reply path. Settle channel-push originals on durable reply/deferral
    acceptance, without claiming remote delivery or read.
12. **Inbox and custody lifetime:** local unread inbox TTL is 24 hours,
    distinct from seven-day custody TTL. A full mailbox is retryable until
    custody expiry, not terminal.
13. **Relay attachment scope:** replace the one-relay-per-server limit with
    per-edge attachments in step 2.
