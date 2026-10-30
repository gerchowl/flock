# ADR 0043 — Multi-node GUI access through a TLS home server and the mesh

- Status: Accepted
- Date: 2026-10-29
- Issues: #2293 (design and slice tracking), #2721 (fleet node/project rows),
  #1380 (mesh commands)
- Decision owner: operator; all six recommended options accepted 2026-10-29.
- Decision record: owner comment on PR #2727, 2026-10-29.
- Scope of acceptance: design and implementation gates, not shipped code.
- Implemented: none by this ADR. The S7 endpoint, client recovery machinery,
  fleet-lite provider and mesh already exist; their remote composition does not.

## NODE STATUS contract — FleetNodeRow (#2721, G4 #1596)

This section comes first so the server and GUI can implement the same boundary.
It is the accepted concrete form of ADR-0038's NodeSummary, not a new stream.
#2721 extends the existing `fleet` payload union with
`{type:"fleet.node.upsert", row: FleetNodeRow}` and `{type:"fleet.node.remove",
node_id: NodeId}`. Node, project and agent rows share its durable cursor and
inline snapshot cut. Rust is authoritative and TypeScript is generated; the
shape below is the design, not hand-maintained TS. New fleet payload kinds obey
ADR-0036's client-wire version/unknown-event gate, independently of mesh
VERSION. They do not authorize a mesh bump.

```typescript
type FleetNodeRow = {
  node_id: string;                  // pinned NodeId; key, never hostname
  name: string;                     // bounded policy-projected display alias
  health: PeerSystemSummary | null;
  latency_ms: number | null;
  last_ok: number | null;
  version: string | null;
  error: { code: string; message: string } | null;
  link_diagnostics: {
    source_node_id: string;
    error: { code: string; message: string };
  }[];
  online: boolean;
  route: Route | null;              // existing mesh Route, actual chosen path
  counts: {
    total: number;
    by_phase: Record<string, number>;
    waiting_on_human: number | null;
  } | null;
};
```

**Owner decision M1 — identity before the first successful poll.** Decided
2026-10-29: no provisional node row without an authenticated NodeId. The owner
accepted the recommended option and its cost below. `PeerSummaryState.node_id`
is optional and starts as None; PeerConfig has no NodeId pin. A configured name
or SSH destination cannot establish that identity.

**No provisional node row (chosen 2026-10-29)**

Key and behavior: Emit FleetNodeRow only when an authenticated mesh
identity/route or a previously verified binding supplies NodeId. Key it by that
NodeId for its entire life. A configured peer with no such identity emits no
node row.

Cost: A never-reached, unbound configured peer is absent from the GUI node tree.
Its configured-name failure remains in the home's existing peer/config
diagnostics, not a fake fleet node.

**Provisional row**

Key and behavior: Add a tagged configured-peer key scoped by home NodeId and
configured peer name, separate from NodeId; later add an atomic, typed
bind/merge delta to replace it with the authenticated NodeId, merging any
already present row.

Cost: Another wire identity variant, alias/rename collisions, persistence
migration and atomic reducer cases. A plain string name cannot safely stand in
for NodeId. This alternative is not selected here.


Thus **online=false before first summary success** is represented only if the
identity is already authenticated independently, for example from RouteTable or
a retained verified binding: emit that NodeId with online false, last_ok/health/
latency/version/counts null until known, and a classified pending/failure
reason. If no NodeId is known, there is no FleetNodeRow and no online field to
serialize. First authenticated discovery adds a normal upsert, not a rename from
a synthetic key. Never correlate unbound summaries to routes by matching names;
retain only observations actually bound to the same NodeId. A changed peer
identity cannot inherit the previous identity's last success, health, counts or
command authority.

Every top-level field is present; nullable fields serialize as `null` when
unknown. Legacy/future readers use ADR-0028 §3a's leniency, not strict
whole-list loading. Nonnegative integers must fit JavaScript's safe-integer
range. Unknown phase strings fold into the `unknown` count while retaining raw
persisted data. The GUI never treats null as zero, an empty inventory, healthy
or online.

**`node_id`, `name`**

Units, provenance and render contract: Merge PeerSummaryState with RouteTable
only by authenticated NodeId, under the identity choice above. A name/alias or
two diamond routes cannot create a second row. Project names through the
exposure policy in §2; unbound labels never become routing authority.

Relay provenance is captured at ingress and remains valid only while the
current local binding exists and equals the captured sender NodeId. If an
enrollment/config removal or a failed pin-store read leaves no current binding,
the captured proof is revoked. A missing binding never means permission to
retain authority. Neither an alias nor a later rebind can create provenance for
an old reading.

Poll and hub-push deliveries with no validated sender identity may retain
observations as unproven display data, never as authenticated node readings.
Such a delivery evicts previously proven rows from that supplying alias before
merging the unproven observations. This also applies to a rejected poll identity:
its earlier proven rows cannot survive that delivery with their old authority.
The current hub-push API requires an enrolled NodeId before admission; the
shared cache merge still applies the same fail-closed rule to absent provenance.

**`health`**

Units, provenance and render contract: Reuse PeerSystemSummary:
cpu_percent/gpu_percent are integer percent 0–100; mem_used, mem_total and
disk_free are bytes; thermal uses ADR-0013's declared severity 0–3 and
component. Missing individual measurements remain absent/unknown under that
existing type. Load never implies thermal health. The node tile renders only
supplied measurements.

**`latency_ms`**

Units, provenance and render contract: Unsigned integer milliseconds of the last
successful summary request, including its round trip and service time. This is
PeerSummaryState.latency_ms, not ICMP or pure network RTT. Retained on failure
and visibly stale when offline. Only a request measured by the serving home to
this owner can populate this scalar. A relayed leg with a different or unknown
observer yields null; it cannot be represented as home-to-owner latency.
Per-hop/provenance-rich latency is later #2293 work. The home has null, not
fabricated 0.

**`last_ok`**

Units, provenance and render contract: Unix epoch milliseconds in the home
server's clock domain for the last successful owner observation. Project the
existing monotonic last_ok/relayed age against home wall time at capture; do not
serialize Instant. Null means never observed or age unprovable. Presentation
only: GUI can display an age using the envelope ts_ms and its own monotonic
dwell, but must never derive online from wall clocks.

**`version`**

Units, provenance and render contract: Last owner-reported software version
string, or null. Show stale retained version as such. It is not capability
evidence. Mesh/client protocol incompatibility is a typed error, not a guessed
version comparison.

**`error`**

Units, provenance and render contract: The selected path's failure, or a retained
last-known cause while offline. For SSH failures use
SshFailureReason::classify and its stable as_str token (src/peers.rs), with a
bounded safe message, not raw PeerSummaryState.error or SSH stderr. Use
separately typed pending/stale/route/peer_incompatible reasons for non-SSH
states; null for a healthy selected path. No tokens, payloads or private paths.
Preserve specific authorization/version causes. Unknown code renders a generic failure
with its safe message.

**`online`**

Units, provenance and render contract: Server classification of the selected
path. A current local server is online. An unbound peer has no node row; an
already authenticated identity is offline before first success and when no
healthy path remains. It describes reachability, never command permission or
agent liveness.

**Owner amendment: healthy-path precedence (2026-10-29, #2779 B).**

A node is online when its selected owner-proven path has a compatible, fresh
successful observation and a live route. Select a healthy observation before
comparing observation ages. A direct-link failure does not make a node offline
when a healthy proven relay is available. If no healthy path exists, the selected
path retains its classified failure in `error` and the node is offline.

`link_diagnostics` is an array of `{source_node_id, error}`. The containing row's
`node_id` is the destination; `source_node_id` is the authenticated serving home
that observed the direct-link failure. Each `error` uses the same bounded,
content-free code/message contract as `FleetNodeError`. These diagnostics are
independent of the selected path and `online`; a direct `auth_refused` remains
visible alongside a healthy relay. They update on owner capture, clear after
direct recovery or removal of the current binding/observation, and are empty
when no authenticated current direct diagnostic is available. They contain no
alias, SSH destination, raw stderr, token or payload. Older persisted rows default the array to empty.

**`route`**

Units, provenance and render contract: Existing Route {node, name, next_hop,
hops}; node equals row.node_id, next_hop is the actual enrolled next hop and
hops is an integer edge count; route.name follows the same exposure policy as
row.name. Home/local or unavailable path uses null. Null plus online
distinguishes local from unavailable; the serving home NodeId is known from
enrollment. Never fill a route from labels or point a remote row through itself.

**`counts`**

Units, provenance and render contract: Last complete owner inventory, deduped by
AgentId, or null if unavailable. total counts those agents; by_phase is a
disjoint partition using shared AgentState phase (or the explicit legacy mapping
below), including unknown, and sums to total. waiting_on_human counts distinct
agents with one or more open non-automatic permission requests, not request IDs;
null if that source is unavailable. Hibernated/offline agents remain in total. A
confirmed empty inventory is total 0 with empty phase counts; local waiting is 0
when its book confirms it, while remote waiting remains null today. Counts stay
stale with their row; route loss never means zero agents.


**Remote status/count projection available today.**
PeerStructuredAgentSummary.status is AgentStatus, not the full S4 AgentState.
For each deduped remote agent map its reported value as follows; do not invent
S4 axes or a permission-book observation:

**Idle**

by_phase bucket: unknown (idle alone does not distinguish finished, settling or
waiting)

**Working**

by_phase bucket: working

**Blocked**

by_phase bucket: blocked

**Done**

by_phase bucket: finished

**Hibernated**

by_phase bucket: hibernated

**Offline**

by_phase bucket: offline

**Unknown or a future value**

by_phase bucket: unknown


Local structured agents use their actual S4 phase instead. **Remote
waiting_on_human is null today**, even for an otherwise complete inventory:
PeerStructuredAgentSummary carries no open non-automatic request IDs. Blocked
status is not a permission-book observation. #2721 must preserve that null;
#2293's later authorized owner-stream integration can supply the real book.

**Cadence and staleness.** Observe on completed peer summaries, local
registry/S4/ permission-book changes, route-table generation changes and
connection failures. Existing peer polling is 15 seconds by default (effective
configuration wins); local machine samples retain their existing sampler
cadence, not a GUI timer. Publish a material upsert in the next owner turn, with
at most one second of ordinary-summary coalescing. Failures/withdrawals bypass
that debounce. No per-tab or per-node GUI ping/poll is added. A server-owned
expiry timer emits an offline/stale upsert when last success becomes **older
than** the configured threshold (default 60 seconds), even without a request or
new summary. Heartbeats never refresh that observation. Unknown age fails closed
as offline/unknown. Use an injected monotonic clock for this decision, including
relayed origin age plus residence; wall-clock jumps affect only last_ok's
display timestamp.

**Completion and removal.** Offline nodes keep their last-known row, health,
version and counts. Removal means explicit unenrollment or absence from a
complete authoritative inventory, not a partial/failed poll. During checkpoint
replacement, mark node/project/agent entities unconfirmed and remove absent rows
only at advertised snapshot completion, including the empty snapshot. A fresh
snapshot belongs only to its subscriber; the shared ring carries live deltas. G4
renders the authoritative online/error state and a stale marker for retained
facts. A healthy client↔home ping cannot freshen an owner or its counts.

**Immediate fail-first acceptance for #2721.** Drive the public fleet
subscription: start with an unbound never-reached peer (no node row),
authenticate its NodeId without a successful summary (offline row), then
succeed; merge two routes to one NodeId; update only latency/version/health;
fail a poll; advance the supplied clock across the exact stale boundary with no
API request; withdraw/restore a route; change agent phases/permissions; finish
an empty snapshot. Assert row shape/units, actual route, retained offline
counts, explicit nulls and completion-only removal. Assert every AgentStatus
mapping and null remote waiting count. A live home plus silent remote stays
visibly stale. Existing VERSION 5 summaries with missing fields remain readable.

## 1. Context and settled owner decisions

The GUI must reach every enrolled node, show node health, latency, versions and
agents, and remain usable across sleep, route changes and partial failures. The
GUI is the priority surface. It must use the server's existing communication and
state machinery rather than require a frontend mesh or another broker.

The [#2293 issue and owner
comments](https://github.com/gerchowl/flock/issues/2293) settle the following;
the alternatives below do not reopen them:

- **Owner decision, 2026-10-22:** remote GUI access goes straight to TLS.
  There is no SSH-tunnel-only interim release.
- **Owner decision, 2026-10-24:** a browser or Tauri client attaches to one
  home server. That server reaches other nodes over the existing mesh.
Agent-to-agent communication continues independently of any GUI.
- **Owner exception, 2026-10-24:** #1380 may change mesh VERSION from 5 to 6
  in the PR that changes the mesh wire, with coordinated upgrade notes,
`peer_incompatible` refusal and version-mismatch tests. This docs PR and
unrelated implementation PRs leave VERSION at 5. No unknown command kind   is
smuggled into a VERSION 5 mail envelope.

[ADR-0036 §5/§5a](0036-structured-agent-sessions.md#5-the-client-wire-contract)
provides one client wire, scoped tokens, durable command identities, replay,
private checkpoints and bounded delivery. [ADR-0038
§3/§6/§9](0038-gui-workbench.md) provides the workbench, presentation-only
client and shared browser/Tauri bundle.
[ADR-0008](0008-agent-message-delivery.md#mesh-amendment-v100),
[ADR-0009](0009-fleet-transport.md#mesh-amendment-v100) and
[ADR-0026](0026-mesh-fleet-transport.md) keep authority at the owning node, use
held authenticated edges, and preserve origin and custody across routing. This
decision extends their remote-client boundary, not the agent mail contract. The
costs of the settled choices are certificate operations, a home failure domain,
and a coordinated fleet upgrade for the new mesh wire; none is hidden by a
frontend fallback. It supersedes ADR-0001's loopback-only remote exposure
decision only for the new S7 TLS listener; the legacy terminal bridge is
unchanged.

## 2. Topology and accepted owner decisions

```text
browser / Tauri -- HTTPS + WSS --> chosen home flk server
                                      |
                               held mesh routes
                                /           \
                       owning node A    owning node B
                       local authority  local authority
```

A TLS listener is an ingress capability of a server. A home gateway is that
server's routing role. They are complementary, not competing transports. Every
node runs its own server; any explicitly enrolled node may be a home. Enabling
remote ingress on every node is unnecessary for fleet reachability. No route
advertisement supplies an arbitrary URL, SSH target or new trust pin.

A GUI opening TLS sockets to every node would remove the gateway hop for a
reachable destination. It would also multiply credentials, sockets, reconnect
state and firewall requirements, and fail for undialable nodes that are
reachable through the mesh. That alternative is rejected by the settled
home-server choice; enabling another server's TLS listener does not select that
frontend architecture.

The owner accepted the following recommended options on **2026-10-29**. Together
with identity choice M1 above, these are decisions M1–M6. Alternative costs
remain recorded for rationale, not as pending questions. These decision IDs are
separate from the implementation slice IDs in §9.

**M2: names at read scope**

Chosen 2026-10-29 and cost: Alias raw row.name and route.name, following the
pre-wire exposure note on #1380. Default aliases are opaque NodeId-derived
labels, extending the displayed digest on collision; an operator can choose a
deliberately public alias. Apply projection before encoding snapshots/live
frames or persistence of client-visible cuts, never only in a component. Cost:
less recognizable machines and an alias policy to maintain.

Rejected alternative and cost: Allow configured/mesh host-like names to
read-scoped tokens: easier recognition, but discloses private hostnames and
topology labels. The owner did not choose this option; no raw-name default.

**M3: remote exposure**

Chosen 2026-10-29 and cost: Enable TLS only on explicitly selected home nodes.
Any other node can be enabled later. Cost: maintain certificate and firewall
configuration for those homes.

Rejected alternative and cost: TLS enabled on every node by default: more
certificate work and attack surface, without improving an existing mesh route.

**M4: client admission**

Chosen 2026-10-29 and cost: Existing per-home scoped bearer plus an explicit
per-destination gateway grant (§4). Cost: bearer theft is still meaningful and
each destination needs enrollment.

Rejected alternative and cost: Mandatory mTLS/device keys as well: stronger
device binding, but browser provisioning, recovery and rotation become a second
credential lifecycle. Track that option in #2293; it is not silently promised by
TLS.

**M5: certificates**

Chosen 2026-10-29 and cost: Operator-supplied certificate chain and private-key
files, renewed by the deployment's certificate manager, with validated atomic
reload. Cost: operator owns issuance and renewal.

Rejected alternative and cost: Embedded ACME automation: easier setup for public
DNS but adds account keys, challenges and private-network DNS policy; subsequent
work remains #2293. No self-signed bypass button.

**M6: home failure**

Chosen 2026-10-29 and cost: Explicitly switch to a separately enrolled home,
with separate credentials and cursor namespace (§7). Cost: no automatic
cross-home command replay.

Rejected alternative and cost: Replicate gateway journals and credentials for
transparent active-active failover: a new distributed authority/store problem;
not required or authorized here, tracked in #2293.


The name policy covers row.name and every route.name regardless of which mesh
hop supplied it. Read scope still exposes NodeId, actual next_hop and hop count;
aliasing hides labels, not topology. An alias is display-only, never dialing or
authorization input. The same safe alias must be used for a node wherever it
appears on one home, including agent routes. The [#1380 pre-wire
note](https://github.com/gerchowl/flock/issues/1380) records why this choice
must be settled before remote projection is enabled.

## 3. TLS ingress, enrollment and limits

The accepted listener design is disabled by default. Enabling it requires an
explicit bind address, advertised HTTPS origin, trusted certificate chain and
matching private key. Wildcard binds require an explicit exposure setting and
valid TLS; never start a plaintext non-loopback listener, even on a private
network. The local Unix listener and loopback S7 path retain their current
admission rules. No listener configuration changes the operating system firewall
automatically. Document the selected interface/port and require operator
firewall allowlisting.

Use TLS 1.3, standard certificate-name/chain validation, and no TLS early
application data: 0-RTT replay protection is an application concern ([RFC 8446
§8](https://www.rfc-editor.org/rfc/rfc8446#section-8)). An operator must enroll
the HTTPS origin with the intended stable NodeId; accepting another valid
certificate does not authorize changing that binding. Certificate renewal for
the same origin/identity needs no new client token. A key/chain mismatch or
expired certificate refuses startup; status/config diagnostics report expiry and
reload failure without key material; an invalid reload leaves a still-valid
loaded pair serving with a visible diagnostic. Once that pair expires, new TLS
handshakes refuse. Atomically swap only a complete validated pair; never fall
back to plaintext. Active authenticated connections keep normal revocation and
liveness checks. TLS does not authorize a client or create a mesh pin.

Serve the browser bundle from its home HTTPS origin. Admit its WebSocket upgrade
only with the configured exact Host/Origin pair and secure scheme; reject
missing, `null` and arbitrary reflected origins on remote browser ingress.
WebSocket's browser-origin protection is described in [RFC 6455
§10.2](https://www.rfc-editor.org/rfc/rfc6455#section-10.2); Origin is not
authentication for native clients. Tauri retains ADR-0038's captured, exact
shell-origin allowlist and per-home token provider. No wildcard CORS,
cookie-based ambient authority, token query parameters or redirect carrying a
credential. Native/non-browser admission still needs the same valid bearer.

Remote enrollment starts with the local operator on the chosen home issuing a
scoped token through the existing management boundary. Transfer it out of band
to that home's browser memory or the shell's origin-bound keychain entry. Do not
expose the current loopback-only one-time-code exchange remotely as an
unreviewed shortcut. #2293 tracks any later remote code-exchange extension. A
local app-session token, bound to a process over the Unix socket, is never
usable on TLS. No bearer, exchange code, raw hello or transcript enters logs.

Before TLS/HTTP admission, bound accepted connections, handshake work, bytes and
unauthenticated lifetime. Reuse S7's existing body/frame/subscription caps and
5-second hello deadline. Accepted initial caps: 32 pending TLS handshakes per
listener, four per source address, each with a 10-second ceiling; 64
authenticated remote sessions per listener. Exceeding capacity refuses with a
retryable typed reason, without evicting authenticated sessions. Address caps
are resource controls, not identity; shared NAT can hit them. Per-principal
command and subscription budgets prevent reconnect from escaping limits.
Counters and rejections are observable; the accepted caps still require load
evidence before implementation ships.

## 4. Node identity, scopes and command authority

The GUI holds one home's credential and socket, not destination tokens. Node
identity is the pinned NodeId, never a display hostname, SSH alias or route
label. Its client principal is namespaced by issuing home: `(home_node_id,
principal)`. Coincident principal strings on two homes confer no shared
authority.

Each destination's local operator explicitly grants an enrolled home a maximum
set of client principals, verbs, scopes and agent/workspace targets. Default is
no new gateway client access, including owner streams and commands. Existing
authorized peer summaries remain governed by their current mesh policy; #2721
projects those already admitted observations without requiring a new gateway
grant. Node enrollment, SSH uid and agent mail `allow_from` are not operator
grants. A home with `operate` does not implicitly get `operate` on another node.
Effective authority is the intersection of the current home token scopes, the
destination grant and the verb's local checks. The remote verb→scope mapping
follows ADR-0036 and the [#2585
ruling](https://github.com/gerchowl/flock/issues/2585#issuecomment-6471599727):
read authorizes permitted reads; send authorizes structured sends attributed to
the token's own principal, not the operator; answer covers its documented answer
verbs, not agent.decide. Permission decisions (agent.decide) and
lifecycle/operator verbs require operate. A destination grant may narrow this
map, never promote send into operate. #2585's attributed-principal
implementation is a dependency for send-only remote structured sends; until it
lands the stricter current backend refusal remains visible, without silently
granting operator identity.

**Config gateway gate.** Every remote config.* verb, including config writes, is
refused with typed `unsupported` before forwarding, claiming or dispatch, even
for an operate token. There is no local-write fallback, arbitrary target path or
raw-secret export. This gate remains until the accepted target
routing/authorization design
(#2366 and #2367 → ADR-0042, #2724) is implemented with the explicit destination
scope and revision-checked writer. Local config authorization is
unchanged. A gateway's operator scope alone cannot select another node's write
target; ordinary agent-command support in #1380 does not open this gate.

The destination remains the only server that changes its agents, queues,
permissions and transcripts. Stable permission request IDs/seqs are preserved.

#1380's new command contract carries the authenticated home/principal binding,
explicit destination NodeId and AgentId, immutable cmd_id, verb/argument digest,
requested scope and bounded routing budget. It contains no client bearer, SSH
key, reusable destination token or operator session credential. It is a request
attested by the enrolled home, not a credential a relay may redelegate. Use
ADR-0026's authenticated origin binding through existing trusted mesh hops;
never infer the origin from the last hop. The destination independently enforces
its grant. A compromised trusted mesh hop remains within ADR-0026's stated trust
boundary; this ADR does not claim Byzantine-safe authorization.

The destination journals `(home_node_id, principal, cmd_id)` plus exact payload
and target before dispatch; the home records the return binding durably. Return
the destination's admission/result ack, not an intermediary's custody receipt.
An admission ack still does not mean the requested work completed. Use the same
key for an uncertain retry with `resume_only`; an absent/lost claim refuses as
unknown outcome, never executes as new. Grant/token revocation fences new
admission and cached-result disclosure, and stops affected remote subscriptions.
It does not undo a previously admitted effect. A command already attested and in
transit when the home token is revoked remains possibly admitted; expose
uncertainty rather than claim cancellation. Destination grant revocation is the
immediate destination-side fence. Bound outstanding commands; no new indefinite
offline command queue disguised as reliable agent mail. Use a 30-second
destination-admission budget starting at the home, matching S7's acknowledgement
ceiling. Decrement it by monotonic residence at every hop, including pause;
rerouting never renews it. A recovered unadmitted request whose age cannot be
established refuses, never gets a new budget. Destination checks expiry
atomically before claiming. Expiry cannot cancel an already admitted operation
or justify a new ID. #1380 must encode and test this budget separately from
agent mail's seven-day custody TTL; a queued command is not ordinary mail.

Remote read subscriptions are grant-checked at the owner, multiplexed over held
mesh edges, and independently bounded at each hop. The owner captures replay and
live continuation atomically. The home does not rewrite owner agent seqs or
invent transcript events; agent cursor namespaces include owner NodeId/session.
Mesh route/owner changes invalidate the old continuation, then resume the same
owner stream from applied cursors. Remote `cmd` remains `unsupported_for_agent`
until #1380 and its rollout gates land. Remote blob and artifact carriage
remains ADR-0038 G11; #2293 tracks its integration, never a direct browser
bypass.

## 5. Federated fleet materialization

The NODE STATUS contract above is the immediate #2721/G4 boundary, including
VERSION 5 observations already available in PeerSummaryState and RouteTable. The
later remote-authority/read slices below must not block those local projections.
#2721 also adds FleetProjectRow, agent node_id/project and fleet.project.*
deltas; project identity follows its issue's Record.cwd rule. The fleet stream
contains all entity kinds in one captured materialization. Unknown state is
retained, never filled by a guessed remote measurement.

Machine facts are owner observations; route/reachability/RTT are home or
named-hop observations. Preserve both provenances. A fresh relay receipt cannot
freshen an old owner sample. Carry elapsed age conservatively across hops,
adding local monotonic residence; if age cannot be established, mark unknown. Do
not subtract remote wall clocks to measure age or latency. Source epochs fence
delayed old updates after restart. Persisted rows are last-known facts, never
live authority.

The home assigns durable `fleet` positions under S7's single writer, sharing the
existing allocator across agent, node and project deltas. Coalesce unchanged
observations before allocating positions; bounded materialized state is not a
replicated fleet audit log. Capture pending mutations, one private checkpoint
and live continuation under one serialization boundary. Only live deltas enter
shared bounded delivery. A slow subscriber gets `lagged` and a fresh
ID/resnapshot without lagging siblings. Complete a snapshot and send `synced`
exactly once; absent rows disappear only at snapshot completion, including empty
snapshots. A disconnected node remains a stale/offline row until explicit
unenrollment; a partial route snapshot cannot remove it. New homes have distinct
cursor namespaces. Ahead cursors refuse, never reset silently. Preserve unknown
fields/entries under ADR-0028 §3a and show future enum values as unknown.

The GUI renders the server's health and reachability classifications, freshness,
route, versions, distinct latency labels and agents in the Fleet tree/node tile.
Relative age formatting and grouping are presentation; the browser does not
synthesize online status, answerability or routes. Stale nodes stay inspectable.
Node-local failures disable only their actions, not unrelated node tiles.

## 6. Low ping cost, latency budget and measurement

Reuse held mesh edges and push/coalesce changed summaries. Never dial SSH per
GUI command or poll per tile. Keep S7's one 15-second application heartbeat per
client connection and its 45-second liveness window; do not multiply it by node
count. Reuse mesh transport liveness and ordinary request timings. An explicit
operator refresh may request one bounded owner observation, coalesced across
readers; it is not a periodic ping loop. Health sampler cadence stays with the
owner. Thermal or connection failure changes bypass ordinary summary coalescing.

Accepted performance targets, **not measured promises**, on an already-connected
path with client↔home RTT ≤50 ms and home↔owner RTT ≤100 ms:

**Home/owner admission and projection overhead**

p95 target and measurement: ≤50 ms total, excluding network, storage flush and
the underlying operation; span each separately so fsync cannot disappear from
the result.

**Small remote command admission ack**

p95 target and measurement: ≤250 ms from client send to owner ack observed by
client, including durable admission; model/tool runtime is a separate completion
measure.

**Changed node/agent fact to GUI application**

p95 target and measurement: ≤1 s from owner publication to client application;
underlying sampler age is reported separately.

**Warm small snapshot to Current**

p95 target and measurement: ≤2 s for 100 nodes/1000 agents; large history uses
S7's advancing-progress rule rather than this small-snapshot target.


Measure monotonic durations at each process boundary; do not calculate one-way
latency from different clocks. Correlate metadata-only command/stream ids across
home and owner. Record p50/p95/p99, error counts, queue depth, dropped/coalesced
observations and idle traffic with one versus many tabs; never exclude failed
attempts from the report. End-to-end RTT is measured by its requester; label
sample age and path changes. A missed target is a failed performance gate, not
permission to shorten heartbeats, hide staleness or widen timeouts. Use injected
clocks, controlled duplexes and bounded receives for correctness; real isolated
TLS/mesh probes measure performance without touching live servers.

## 7. Partial failure, reconnect and home failover

S7 connect/hello deadlines, equal-jitter capped backoff, foreground
invalidation, priority heartbeat writes and independent stream progress
deadlines remain. A local pong never clears remote staleness. An owner outage
retains its rows and transcript with a stale marker; other nodes keep serving. A
route withdrawal cancels only affected remote work and seeks an authenticated
alternative under ADR-0026's loop/hop budget. No guessed endpoint, new token or
second client socket is created. Pause and `peer_incompatible` are explicit
reasons, not fast reconnect loops. Incompatibility requires coordinated upgrade,
not a fallback wire. A destination that cannot be reached produces a typed
refusal before new admission; a previously sent command can instead be
uncertain.

On reconnect to the same home, authenticate again and restore only applied
cursors and unfinished checkpoints. Same-principal outbox entries obey S7's
24-hour journal window, immutable target and resume-only rules. The home never
turns a cached remote display row into permission to dispatch offline.

A failed home leaves the GUI visibly disconnected. The person may select an
already enrolled alternative home. It authenticates independently and bootstraps
its own fleet materialization; it does not inherit the first home's token,
principal, server epoch or home-stream cursor. Preserve the old outbox under its
original home with unknown outcomes and an inspection action; never resend it
through the new home with a fresh command identity. Destination-owned effects
remain inspectable through the new home's separately granted reads. Replicated
gateway journals/transparent failover are tracked in #2293, not part of this
accepted scope.

**Mobile forward reference (#2733).** The Tauri iOS/Android client is a consumer
of this remote-access design for escalation notifications. #2733 owns that
notification and background-delivery design. This is a dependency reference
only: it adds no push-relay, enrollment, payload or delivery decisions here.

## 8. Threat model and pre-mortem

Protect client credentials, operator authority, session content, integrity of
node identity and availability of the server/GUI. Adversaries include a hostile
website, network peer, unenrolled node, malicious agent content and stolen
bearer. Same-uid operator compromise and a compromised trusted mesh hub are
inside the existing trust boundary; TLS cannot repair either. A compromised
authorized home can exercise its granted destination authority, so enroll
narrowly and revoke at destinations. Neither hostnames nor source IPs are
authorization.

**An attacker steals a bearer through a redirect, log or browser artifact.**

Prevention and required evidence: No credential redirects/logs, memory-only
browser token, keychain scoped to home origin; CSP and opaque artifact sandbox
from ADR-0038 remain. Seed a secret and inspect every denial/log path.

**A hub launders operator authority or a principal collision crosses nodes.**

Prevention and required evidence: Origin pin plus explicit destination grant,
namespaced principal, scope intersection. Wrong origin/target/grant never claims
a command or opens a read stream.

**A recovered gateway repeats an answer after losing its ack.**

Prevention and required evidence: Destination durable claim and unchanged return
binding; crash before/after effect and recover the same key. Missing claim is
uncertain, never new execution.

**A certificate expires overnight and the client accepts insecure fallback.**

Prevention and required evidence: Validate rotation, alert on expiry, refuse
expired/mismatched TLS. Test renewal while connected and no plaintext/socket
fallback.

**One slow node or tab stalls the fleet or triggers resnapshot storms.**

Prevention and required evidence: Independent bounded work, per-subscriber
checkpoints, coalescing and control-frame priority. Stall one owner and one
reader while two healthy readers converge.

**A green home heartbeat hides a frozen remote sampler.**

Prevention and required evidence: Separate source age, poller health and path
RTT; hold the home live while the owner stops producing, and assert the remote
row becomes stale/unknown.

**Sleep or a partition destroys the operator's view or duplicates rows.**

Prevention and required evidence: Preserve stale rows/cursors, source fences,
NodeId dedupe and completion-only removal. Inject diamond route changes and
suspend mid-checkpoint.

**Many clients exhaust TLS or reconnect capacity.**

Prevention and required evidence: Bounded handshake/principal work and jitter; a
deterministic admission flood cannot starve an established client or evade
budgets by reconnecting.


## 9. Implementation slices and release gates

N1 (#2721) is the **first implementation slice**, ahead of TLS and gateway work;
it uses existing VERSION 5 observations and is not gated by #1380. All work
below is tracked by #2293 unless #1380 is named. These are accepted slice
boundaries; each implementation PR still requires its own review. Each PR must
stay at or below **400 own production lines, Rust plus TypeScript combined**;
tests and docs are separate. Estimates are budgets, not excuses to exceed the
cap: split further before opening an oversized PR. Preserve a behavioral failing
baseline for every listed oracle, using explicit interleavings and virtual
clocks; no sleeps, retries or timing slack to turn red into green.

**N1 node schema/projection (#2721)**

Own production budget: 300

Dependencies: NODE STATUS contract, existing directory/samplers

Fail-first acceptance: Never-reached unbound peers emit no row; authenticated
identities before first success emit offline rows. Pin AgentStatus buckets, null
remote waiting, SshFailureReason codes, name projection, NodeId dedupe and
generated types.

**T1 certificate/config boundary**

Own production budget: 250

Dependencies: Accepted §2/§3; ADR-0041

Fail-first acceptance: Missing key, wrong chain, expired cert and non-TLS
wildcard exposure refuse; valid explicit config works; logs contain no key.

**T2 TLS listener and origins**

Own production budget: 350

Dependencies: T1, existing S7 endpoint

Fail-first acceptance: Real isolated TLS/browser connects; wrong
name/Origin/Host, insecure redirect and early application data refuse; current
Unix admission remains covered.

**T3 reload and admission limits**

Own production budget: 300

Dependencies: T2

Fail-first acceptance: Rotate a valid pair, then an invalid pair and expiry; no
plaintext fallback. Flood pending handshakes while one admitted reader
progresses; cap reservations release on cancellation.

**A1 destination gateway grants**

Own production budget: 300

Dependencies: Accepted §4, existing token management

Fail-first acceptance: Valid home token without a destination grant refuses;
wrong principal/home, read scope and send→operate escalation refuse before
claim; remote config.* refuses unsupported; revocation fences a racing
admission.

**M1 command envelope/version**

Own production budget: 300

Dependencies: A1; #1380; coordinated upgrade decision

Fail-first acceptance: VERSION 5/6 mismatch yields peer_incompatible with no
dispatch; origin/target/payload substitution fails; no credential occurs in
serialized frames. This is the only slice authorized to bump VERSION.

**M2 destination journal dispatch**

Own production budget: 350

Dependencies: M1, S7 command journal

Fail-first acceptance: Lose ack, restart at claim/effect/result, then resume:
exactly one effect or explicit uncertainty; same id/different target refuses.

**M3 home return binding**

Own production budget: 300

Dependencies: M2

Fail-first acceptance: Crash home across forwarding/ack; replay preserves
destination key and result, never returns custody as command success or creates
another command.

**R1 owner read cut**

Own production budget: 350

Dependencies: A1, M1, S7 replay core

Fail-first acceptance: Mutate during capture/replay and withdraw route: no
missing/duplicate owner event or sequence rewrite; wrong read grant and late old
generation refuse.

**R2 home subscription relay**

Own production budget: 350

Dependencies: R1

Fail-first acceptance: One lagged reader or stopped owner does not lag siblings;
fresh IDs recover on a diamond route; blob access does not bypass grants.

**N2 fleet node/project persistence (#2721)**

Own production budget: 350

Dependencies: N1, fleet-lite cut machinery; R2 only for new remote reads

Fail-first acceptance: Concurrent mutation and replay, empty completion and
absent-row removal, restart/ahead cursor, two readers plus a third snapshot: no
lost delta, false removal or resnapshot storm.

**G1 node tree/tile**

Own production budget: 350

Dependencies: N2, ADR-0038 client store

Fail-first acceptance: Real endpoint fixture drives health, versions, latency
and agents; stale remote plus live home renders distinct states, unknown remains
visible, no per-node frontend socket appears.

**G2 enrollment/failover UX**

Own production budget: 300

Dependencies: T3, M3, G1, existing shell provider

Fail-first acceptance: Browser and shell connect to one enrolled TLS home;
switch home after lost ack and show old outbox as uncertain without replay;
stolen origin cannot access keychain credential.

**V1 latency/robustness gate**

Own production budget: 150

Dependencies: All above

Fail-first acceptance: Controlled loss, delay, route change and slow-reader runs
meet §6 targets or fail with stage evidence; traffic counters prove no GUI-tabs
× nodes probe growth or per-command SSH spawn, while allowing one existing
heartbeat per client connection.


Before public remote writes, land T1–T3, A1, M1–M3 and their compatibility,
revocation and crash tests. Remote owner reads require R1/R2. N1/N2 can expose
existing peer observations without #1380; the complete multi-node GUI milestone
requires G1/G2 and V1 as well. Tests use isolated servers and short socket
paths. CI is the full platform gate, including browser/Tauri origin captures.
This ADR PR runs documentation/governance checks only; it does not claim any of
these implementation acceptance tests already pass.

Known scope limits all have trackers: additional remote enrollment/certificate
providers and transparent home failover (#2293), continuous enforcement of
existing local read-scope changes (#2042; remote grants must be continuously
fenced here), activity production (#2243), artifact/blob mesh integration
(#2293, ADR-0038 G11), and mobile background/push behavior (#1402). None may be
filled with invented status or a second frontend communication layer.
