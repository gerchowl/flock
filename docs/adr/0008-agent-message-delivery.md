# ADR 0008 — Agent-to-agent messages ride the tool surface, not the keyboard

- Status: Accepted
- Date: 2026-08-03
- Implemented: all of it. `flock_msg_send` / `flock_msg_reply` /
  `flock_msg_list` are on the MCP tool surface (`src/mcp/tools.rs:113,160,172`),
  and the deliver-by-keystroke path this ADR replaced is gone
  (`src/app/api/messages.rs:1756-1762`). Its **transport** half was later
  superseded by ADR-0009; this ADR's delivery model stands unchanged.
- Issues: #213 (the delivered-message shape spike); supersedes ADR-0006's
  addressing where it assumes a server-local pane id is an address; builds on
  ADR-0005 (durable event log as the audit substrate) and #175 M1/M2.
- Decision owner: operator; design from the cross-machine failure observed
  sending hopper → kiln-dev on 2026-08-03.

## Context

`msg.send` delivers by **typing the message into the recipient's pane** and
submitting it. That single choice is the root of five separate problems.

**It puts agent traffic in the operator's channel.** A delivered message
arrives as the recipient's *user turn* — byte-identical in kind to the human
typing. #175's P3 says sender identity is routing and audit, **never
authorization**, but an agent cannot honour that distinction if the only signal
is a `[flk msg …]` prefix it may or may not respect. Under P7 (untrusted by
default) a text prefix is not a trust boundary.

**It forces a delivery race that does not otherwise exist.** Typing into a live
TTY is only safe at a settled turn boundary, so delivery is gated on `Idle` plus
an `ATTENTION_SETTLE` dwell, with a refusal when the agent is unknown so a
message is never typed into a bare shell prompt. That machinery is pure
consequence of the transport — and it has already produced one stranded-mailbox
bug (#188).

**It cannot carry structure.** Sender, correlation id, repo and reply
expectation have to be flattened into a prefix string and re-parsed by the
recipient by eye.

**It has no answer off-machine.** `MessageTarget` resolves within one server,
the sender is inferred from *local process ancestry*, and when that inference
finds nothing it degrades to `None`. A message relayed from another host
therefore arrives `from unknown` **and still advertises a reply command**, which
then fails with `no_reply_address`. The envelope lies.

**It is asymmetric with what already exists.** `flock_msg_send`,
`flock_msg_reply` and `flock_msg_list` are already MCP tools. Sending is
structured; only delivery is keystrokes.

## Decision

**Agent-to-agent messages are delivered through the agent's own tool surface.
The keyboard stays the operator's channel.**

Two channels, split by *sender class* rather than by mechanism:

| sender | channel | authority |
| --- | --- | --- |
| operator → agent | `agent.send` — raw text into the pane, the user's turn | the operator's |
| agent → agent | MCP inbox, read via `flock_msg_list` | none: routing and audit only |

`agent.send` semantics are untouched (#175 P5).

Delivery becomes two independent halves:

- **Content** is pulled: the recipient reads structured messages with
  `flock_msg_list` and answers with `flock_msg_reply`. Fields, not a prefix.
- **Wake** is pushed at a turn boundary by the agent integration's stop hook,
  which already returns `{"decision":"block","reason":…}` to keep a turn alive
  (it carries the recap sentinel today). The reason names the count and the
  tool — never the message body, so nothing an agent said can reach another
  agent through the wake channel.

MCP is *pull*; without a wake an idle agent never learns mail arrived. The hook
is what makes the inbox live, and it is the harness's own protocol rather than
synthetic keystrokes.

## Consequences

**Gained.** The authority boundary becomes structural: a tool result is not a
user turn, so P3 no longer rests on prefix compliance. `ATTENTION_SETTLE`, the
Idle dwell and the unknown-agent refusal all disappear with the transport that
required them. Payloads carry fields. Replies route inside flock, so no
`flk --remote …` command ever has to be rendered into text for a human-shaped
agent to copy.

**Given up — deliberately.** A recipient without an MCP inbox can no longer
receive agent messages. Today injection reaches anything with a TTY. Keeping
injection as a fallback would mean two delivery paths with independently
drifting semantics, which is the exact defect class #124/#197/#199–#210 were
about; one honest refusal (`recipient has no inbox`) beats two paths that
disagree. Operators keep full reach through `agent.send`.

**Per-integration cost.** The wake is the stop-hook contract of one agent
runtime. Others need their own, the same way state *detection* is already
per-integration. Runtimes without a wake degrade to pull-only: messages queue
and are read whenever the agent next lists them, which is correct but not
prompt.

**Not solved here, and still required.** This ADR settles *how a message reaches
an agent*. It does not settle identity, and deliberately so — a stable
`AgentId` with a gossiped directory (address ≠ location), the durable event log
as the message store with mailboxes as projections, and replication over the
existing peer channel are each their own decision. Cross-machine messaging needs
all three; this ADR is a prerequisite, not a substitute.

## Alternatives rejected

**Keep injection, improve the envelope.** The spike (#213) proposed a richer
prefix carrying a routable sender, a verified reply line and a `needs-reply`
flag. It is a strict improvement and still the right shape *if* injection
survives — but it mitigates the authority problem with a string instead of
removing it, and keeps every turn-boundary hazard.

**Deliver over MCP server-initiated notifications.** Cleaner in principle: no
hook, true push. Rejected as the primary mechanism because whether a harness
surfaces a server notification to the model mid-session is outside flock's
control, whereas the stop-hook contract is already relied on and tested here. To
be revisited if the guarantee firms up.

**A fourth `flk msg` transport of its own.** Rejected on DRY grounds: peer
federation already exists for cross-host traffic. If it cannot carry a message
record, that is the thing to fix.

## Mesh amendment (v1.0.0)

See [ADR-0026](0026-mesh-fleet-transport.md), implemented by #623 and #661.

The tool inbox and sender-authority boundary remain. ADR-0026 separates
AgentId identity from reachability: replies use a durable return binding and
custody routing, even when the sender is offline. An authenticated origin pin
supplies policy identity; a host label is never dialing authority.

## Proposed amendment: moves transfer identity, not a second name (#1500)

- Status: **Proposed; Owner decision required** for the choices below.
- Date: 2026-10-15. Part of [#1500](https://github.com/gerchowl/flock/issues/1500).
- Scope: design only. This amendment implements no API, resolver, detector,
  geometry, persistence or mesh change. Follow-up implementation and regression
  proofs remain tracked in #1500. The accepted delivery/authority rules above stand.
- Evidence baseline: `19397f1760262e69cd0cda6553525df0c2d3f2e7`; source references below identify the inspected
  functions. The reported incident is not a fresh isolated reproduction.

### Observed failure and code evidence

The incident reports an original name resolving to its hibernated old pane,
while a new `moved-` name resolves to the resumed pane. The migration driver's
exact commands and a before/after identity capture are not available in #1500.
Do not infer that the two panes have the same AgentId merely because they resume
one native conversation. Source inspection explains how this state can persist:

| Code | What it establishes |
| --- | --- |
| [target resolver](../../src/app/terminal_targets.rs#L51) | `resolve_terminal_target` searches terminal id, pane id, AgentId, then names/labels among local placements. It does not exclude hibernated or superseded records. |
| [agent records and name conflicts](../../src/app/agents.rs#L1298) | `agent_info_at` projects terminal identity and resumability; `agent_name_conflicts` counts existing names, including parked records. There is no independent transactional name registry in this path. |
| [hibernate and resume](../../src/app/hibernation.rs#L72) | `hibernate_pane` retains the terminal, name and resume plan. `resume_hibernated_pane` resumes into that same terminal/pane; it does not transfer identity to a different node. |
| [identity](../../src/terminal/agent_id.rs#L5) and [snapshot](../../src/persist/snapshot.rs#L134) | AgentId is restart-stable identity; its mint-time host component is not routing authority. Snapshots preserve AgentId and agent name separately. A session id is another identifier. |
| [directory](../../src/app/directory.rs#L65) and [owner hints](../../src/mesh/store/tombstones.rs#L129) | Local resolution wins over cached remote entries. `note_owner` stores node/name/seen, without a fenced move generation. Gossip alone cannot atomically replace authority. |

This extends ADR-0008's address/location distinction and ADR-0026's
[identity/lifecycle contract](0026-mesh-fleet-transport.md#identity-directory-and-vanished-agents).
ADR-0027 §1's proposed delegation edges use AgentIds, not cached placements;
ADR-0028's lifecycle/turn distinction and ADR-0029's explicit refusal contract
also apply. Their historical implementation-status notes are not proof of what
this checkout implements.

### Identity and resolution contract

A **move** preserves one logical AgentId, its operator-selected name, native
harness/session binding, delegation/goal references and mail identity. It changes
owning node, terminal/pane/workspace placement, and placement generation. It does
not copy a node key, operator credentials or authorization. Native session data
must be available on the destination through an explicitly verified export or
shared store with the correct harness/profile; a matching string alone is not
proof. Unavailable/incompatible session data refuses the move. Starting fresh or
forking is a **clone** with a new AgentId and a distinct name, never a fallback.

Persist a logical identity binding `(AgentId, name, owner_node, generation,
placement, native_session, move_id, lifecycle)` separately from the terminal
projection. A binding owns at most one active placement. Names retain their
current server scope; do not silently turn a local short name into a fleet-wide
unique address. Destination name collisions with another AgentId refuse during
prepare. A fleet-scoped search reports all conflicting identities and locations.

A successful source release marks its old record `moved` with the destination
binding/generation. Preserve historical name/session/AgentId as provenance;
remove its active name alias and active identity claim. A display label such as
`moved-away` is presentation, not a routable replacement name. Retain a durable
move fence independently of snapshot and audit-log retention. Restoring the old
snapshot, focusing its pane or issuing resume must not reactivate that claim.
No `AgentRemoved` tombstone is emitted for a move: ADR-0026 uses that tombstone to
terminate mail for the exact identity/session.

Resolution is intent-aware, shared by API, CLI, supervisors and reap paths:

- A unique committed current binding resolves to its live placement. An ordinary
  current hibernated binding remains inspectable/resumable, but send refuses
  `agent_not_ready` before writing. Resume is explicit and checks the move fence.
- A historical moved target refuses mutation with structured current AgentId,
  owner, generation and placement hints. It never wakes the old pane. At the
  destination, the transferred original name resolves to the current binding.
  On the source, the original name can report the move without claiming it is
  locally live. Remote action requires explicit configured-peer routing; labels
  never become SSH dialing authority.
- Ambiguity uses `agent_target_ambiguous` with candidates, not a live-first guess.
  This includes duplicate current claims and name/label collisions. Explicit
  pane/terminal inspection can still read history; it does not bypass mutation
  fences. A name-based get of only an inactive target refuses and names any
  known current binding; list/history retain inactive records for inspection.
- Destructive operations pin AgentId **and generation**, then revalidate the
  current placement and resource claim at execution. A cached workspace id is
  not identity and never redirects a destructive request to another workspace.
  Existing stable error strings come from `src/api/error_code.rs`; any new
  move-specific codes require registration and ADR-0029 exit classification.

### Atomicity and failure recovery

Proposed authority is a durable per-identity move ledger, with a monotonic
source-authorized generation and idempotent move id. Snapshots and directory
summaries are projections, not commit records. A same-node move can switch name,
identity and placement in one local transaction before publishing projections.
Cross-node movement is a recoverable fenced handoff, not an atomic distributed
filesystem rename or a promise of uninterrupted availability:

| Phase | Required durable fact and permitted activity |
| --- | --- |
| Prepare | Destination reserves the name, validates session/profile/export and records the proposed next generation. It has no public live alias and cannot start/receive agent input yet. Failure leaves source ownership intact. |
| Quiesce | Source durably records quiescing, refuses new sends/wakes/reaps and inbox imports for the identity, drains accepted operations, hibernates and confirms its owned process tree stopped. Capture final session/mail state after that barrier. Failure to confirm termination refuses release. |
| Release | Source commits its fence and signs a transfer certificate over move id, AgentId, session, source/destination node identities and consecutive generations. This is the authority cutover. Source never resumes automatically after this commit. |
| Activate | Destination verifies the committed release, atomically imports the binding and staged durable state, then starts the native resume with real layout geometry. It advertises the new name/placement only as current ownership; failed resume is offline/resume_failed, not a fresh identity. |
| Complete | Persist destination acknowledgement at the source and publish directory updates. Repeat messages are idempotent; a stale source projection refuses and supplies the current hint. |

Before release, the destination must durably acknowledge the final export hash,
name/generation reservation and storage admission. New mail during quiescence
remains in upstream/source custody rather than entering the frozen inbox export.
The later proof routes that custody without rewriting immutable envelopes.

There can be a period with no running process, but never two authorized runners.
A crash before release can abort preparation and unquiesce the source only after
proving no release committed. A crash after release keeps the source fenced even
if activation fails or the acknowledgement is lost. Reconciliation queries the
move id and committed generation, not wall clocks or name availability. An
unreachable owner means blocked/offline, not permission to take over. Returning
to the source after release is a new reverse handoff with a new generation.
Recovery must reject out-of-order/replayed certificates and competing moves.
The guarantee assumes durable ledger writes survive: restoring an entire ledger
and node-key backup is not the same as restoring an old session snapshot. It
requires owner-authorized recovery before that node can resume identity claims.
Audit events may describe phases, but cannot replace the durable ledger;
ADR-0028 §3a governs any future unknown-kind reader test.

Worktree ownership is separate from conversation ownership. Transfer or retire
resource claims explicitly, recording both physical checkout identity and owning
placement. Equal path strings across nodes do not prove equal storage; different
paths can refer to one shared checkout. Reap revalidates all current claims and
refuses an old workspace target that has moved. Never authorize removal solely
because the source pane is hibernated or its branch merged. Preserve the existing
[merge/resource gates](../../src/cli/delegate.rs#L4524) and
[worktree teardown](../../src/worktree.rs#L2363). Manual old-pane cleanup must not
remove resources still claimed by the destination.

### Mesh, custody and compatibility

**Protocol impact flagged: the proposed native cross-node handoff requires a
new mesh protocol version (recommended next version 6), subject to owner approval.
This design PR leaves production mesh VERSION at 5.**

ADR-0026 already requires authenticated owner updates. Current
[hello VERSION 5](../../src/mesh/hello.rs#L9) rejects mismatches, and owner hints
have no generation/certificate. Local-first stale claims can override remote
knowledge. New transfer proofs, generations, withdrawals and reconciliation
messages must not be smuggled through v5 name gossip or ordinary agent mail.
A coordinated capability/version rollout must precede native cross-node moves;
unsupported peers refuse that operation rather than degrading to session cloning.
Same-node identity/alias fencing can ship separately without this mesh change.

Custody transfer must preserve immutable message keys/content, original return
bindings, read/dedupe state and remaining TTL. Forwarding an existing envelope to
a new owner needs the authenticated move proof; do not rewrite signed immutable
content or mint a second message. Destination commits staged mailbox state and
dedupe before accepting ownership/delivery; source stays fenced from consumption
and retains custody until the destination acknowledges durable import. Lost
acknowledgements reconcile the same move, never reset expiry. If the original
custodian is unavailable, accepted mail remains queued there under ADR-0026's
existing availability/retention limits; a move cannot promise recovery of data
that was not exported. Node enrollment keys remain local to their node.

Existing name-based scripts keep the original name on the destination. Source
scripts receive an explicit refusal and current-location hint rather than sending
to a dead pane. Scripts should store AgentId plus generation and discover current
placement for each operation; stale pane/workspace references remain stale.
Existing `moved-` copies are not silently adopted: inventory both AgentIds,
sessions, names, outstanding mail and resource claims, then require an explicit
operator reconciliation. Do not automatically merge histories/inboxes from two
independently created agents. ADR-0029's exit constants remain authoritative;
a move/refusal is not a successful send or reap.

### Separate symptom hypotheses and verification

No live-pane capture or model call was made for this spike. These are hypotheses,
not confirmed causes of the reported narrow width or `unknown_composer`.

| Symptom and hypothesis | Evidence and isolated verification |
| --- | --- |
| Narrow pane: destination layout really allocates a small split, or spawn used stale geometry before layout/client resize. | [estimate_pane_size](../../src/app/state.rs#L3407) estimates the terminal area, while [pending resume](../../src/app/agent_resume.rs#L81) derives per-pane inner rectangles. [Headless resizing](../../src/server/headless.rs#L978) uses foreground-client geometry and skips this path without one. Its detached minimum is 80 columns for the server, not a guarantee of 80 columns for each split. Capture attached-client size, effective size, computed inner rectangle and actual PTY winsize separately on an owned fake-resume instance. Compare detached/attached, split/unsplit, resize-before/after-resume and hidden-tab cases. A correct narrow rectangle implicates layout policy; a PTY/rectangle mismatch implicates spawn/resize ordering. Do not merely raise a minimum or delay resume. |
| `unknown_composer`: missing effective harness identity or an unrecognized composer shape after resume; narrow reflow is a possible contributor, not a proven cause. | [guarded submit](../../src/app/guarded_submit.rs#L543) refuses both an unknown effective agent and `Composer::Unknown`. [runtime_composer](../../src/app/guarded_submit.rs#L171) uses the detector snapshot; [Codex region](../../src/detect/mod.rs#L392) requires visible prompt/footer evidence. [agent.send](../../src/app/api/agents.rs#L676) already routes submit through this guard; `--submit` is a compatibility alias, not another path. Capture known-agent/session state and `flk pane read --source recent --format text` plus ANSI where styling matters from an isolated resumed pane. Replay the exact bottom buffer at captured width and a normal width, including startup dialogs. Classify identity loss, missing visible controls and real draft separately; assert refusal writes zero bytes. Do not weaken detection or bypass guarded submit. |

### Acceptance evidence required for implementation (#1500)

The design adds no product tests claiming these behaviors are implemented.
Each implementation slice must demonstrate the corresponding regression once
against the baseline, then pass with the accepted change, using owned fakes:

| Criterion | Required failing-before proof |
| --- | --- |
| Name and AgentId address the current episode | Begin with a named hibernated source and a differently placed destination; move, restart both, resolve/send the original identity, and assert only the destination receives bytes. |
| No ambiguous/inactive mutation | Duplicate names/current claims and explicit historical targets refuse with candidate/current hints and zero input, including focus/resume and reap. |
| Atomic handoff and recovery | Fault each durable phase and lose each acknowledgement; replay and race two moves. Assert at most one authorized process/alias, stable generation and no source wake after release. A silent producer still fails at a bounded liveness-aware ceiling. |
| Mail and resources survive | Queue unread/read/in-flight mail, move, replay an old owner update, and retry a lost receipt. Preserve keys/dedupe/read state/TTL; an old workspace reap cannot remove a destination-claimed checkout. |
| Version/script compatibility | An unsupported peer refuses without partial release; v5 normal mail still follows its existing contract. Legacy references give a useful refusal, never success at a retired pane. |
| Geometry and composer symptoms | Drive real client resize/layout/native fake resume, capture recent buffers, and compare the two hypotheses above. Do not create detector fixtures from invented incidental text. |

### Owner decisions required before implementation

1. **Owner decision required:** approve preserved AgentId/name/native session
   as move semantics, with explicit reconciliation of already duplicated
   `moved-` records; clones remain separate identities.
2. **Owner decision required:** approve a coordinated mesh version/capability
   extension (recommended v6) for native cross-node handoff. If VERSION must stay
   5, limit this work to local fencing/refusal and same-node moves; do not claim
   an atomic native cross-node move or change v5 custody authority silently.
3. **Owner decision required:** approve fail-closed recovery after source loss.
   Forced takeover without the source's durable release needs a separate approved
   authority/recovery mechanism; it is outside this proposal's guarantees and
   remains part of #1500's decision record.
4. **Owner decision required:** choose whether migrated resource claims can
   authorize eventual source-checkout cleanup, and how shared storage is attested.
   Until accepted, moving alone grants no automatic worktree deletion authority.
