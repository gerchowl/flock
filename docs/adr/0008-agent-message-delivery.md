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

## Clarification (2026-10-08): cross-host replies and confirmations

The [Proposed mesh amendment to ADR-0009](0009-fleet-transport.md#amendment-2026-10-08-mesh-mode-and-durable-message-custody)
addresses [#623](https://github.com/gerchowl/flock/issues/623) and
[#661](https://github.com/gerchowl/flock/issues/661). It leaves this ADR's tool
inbox and sender-authority boundary intact. The following is the proposed
transport contract, not a claim that collection already ships.

Agent identity, current location and reachability are distinct. An envelope's
sender AgentId permits a reply to name its recipient, but cannot prove that a
live return channel exists. `from_host` is an origin label, never authority to
dial. Preserve a transport-attested, conversation-bound return capability in
queued and delivered metadata, so replying to accepted mail does not require
finding the sender anew in a stale directory.

Requests and answers are durable records. If immediate reply delivery fails,
the receiver or authenticated custodian retains the answer. The originating
server collects it over an edge it can open, including on reconnect, imports
it once into its local inbox/event model and acknowledges that import. A
sleeping or NATed laptop therefore needs no inbound SSH reachability. A sender
without an inbox still gets its answer through local wait/status. A sender
agent that has been removed is not replaced by another occupant of its pane.

Delivery confirmations and genuine read receipts travel through that same
store-and-forward/collection mechanism and become local facts at the origin.
"Delivered" means the target server durably imported the message into the
recipient's inbox, not that SSH succeeded, a hub took custody, a wake was
accepted or the agent read it. [#640](https://github.com/gerchowl/flock/issues/640)
tracks wake/submit evidence separately from mailbox read state. `wait_reply`
and `msg.status` consume imported local outcomes instead of polling remote
servers inside the waiter.

`msg.read`'s `replyable` must distinguish a valid durable return binding from
immediate connectivity. A disconnected bound sender is replyable with a
queued/collection-pending explanation. A legacy envelope without that contract
must report its weaker reachability honestly. Unknown identities, authoritative
agent removal and expiry are distinct from temporary offline routing. Neither
transport attestation nor a collection capability makes an agent message an
operator instruction.
