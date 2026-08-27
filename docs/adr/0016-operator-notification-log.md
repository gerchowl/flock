# ADR 0016 — Outcomes are filed as durable events; unread is a projection, not a second store

- Status: Proposed
- Date: 2026-08-27
- Issues: #372 (the gap and this design); builds on #36 (the delivery gate that
  decides *when* an outcome is worth saying, landed as the drainer this files
  behind), ADR-0005 (durable event log — the substrate, and the P5 that forbids
  a second one), ADR-0008 (the agent inbox, whose operator counterpart is
  missing), ADR-0009 (fleet transport — why the content is pulled, not
  replicated), ADR-0014 (the narrowed-verb precedent for agent-initiated
  writes), #367 (the ambient title badge these counts feed), #316 (the
  measurement this gap defeated).
- Decision owner: operator. Decision 5 in particular — whether an agent may
  file into the operator's list at all — is deliberately left as a
  recommendation with its alternative stated, not resolved here.

## Context

flock can say *that* something wants you. It cannot say *what happened*, and it
keeps nothing.

Verified rather than assumed:

- `ToastNotification` (`src/app/state.rs`) carries `kind`, `title`, `context`,
  `position`, `target` — and **nothing in `src/persist/` references it**. A
  toast is a field on `AppState` that a timer clears. A toast you did not see
  never existed.
- `EventKind` has 36 variants and every one is a lifecycle fact —
  `PaneCreated`, `AgentForked`, `MessageQueued`, `CheckFired`, `CronFired`,
  `RunReverted`. There is no kind meaning *"here is an outcome a human should
  read"*.
- ADR-0008's inbox is **agent → agent**. There is no operator inbox.
- **Even the explicit verb drops content on the floor.**
  `notification.show` already carries a `title`, a `body`, a placement and a
  sound (`NotificationShowParams`, `src/api/schema.rs:303`) — but its handler
  (`src/app/api.rs:1298`) answers `Busy` when a toast is already on screen,
  `RateLimited` under load, `NoForegroundClient` when nobody is attached, and
  `Disabled` when `[ui.toast] delivery = "off"` — **which is the default**. In
  every one of those four cases the caller's text is discarded. The content
  layer is not missing; the *keeping* is.

The gap bites in three shapes, and the third is the one that makes it
architectural rather than cosmetic:

1. **A finished background task.** An agent files an issue at 03:00 and exits;
   the issue URL — the only thing you wanted — is in a pane that may already be
   closed.
2. **A refusal.** `agent_profile_unresolved` (#366), `peer_not_configured`, a
   check that errored. Exactly the events worth reading later, and the ones
   that vanish soonest.
3. **Anything cross-host.** An outcome produced on `sage` while the operator is
   attached to `anvil` has no channel home at all.

And the cost is already measurable. #316 concluded agent-to-agent messaging had
no demand from `message_queued: 2` in 35,216 events. The log could not
distinguish *nobody tried* from *everyone who tried was blocked*, because there
is no event kind for a refusal. That is the same missing kind.

## Decision

### 1. Outcomes are events in the existing log, not a store beside it

Two new `EventKind` variants, both `is_persisted`:

- `NotificationFiled { id, title, body, severity, target, origin_host, source }`
- `NotificationSeen { id }`

ADR-0005's design round considered exactly this fork and closed it: *"§9
explicitly rules out adopting a message broker; **P5 rules out a second event
system** — the hub itself must become durable."* A notification store beside
the log is that second event system, with its own file format, its own
rotation, its own corruption posture and its own restart semantics — four
solved problems, re-solved.

The objection — *don't pollute a lifecycle log with human-facing prose* — is
real but describes a line that was already crossed, and not by this ADR.
`MessageQueued` carries an agent-authored free-text `body` (ADR-0008);
`CheckFired` carries operator-authored notify text. The log has held prose
since #175 M1.

What was actually missing is not a store but a **kind**. The distinction the
issue draws — a lifecycle fact versus an outcome a human should read — is a
property of the record, and `NotificationFiled` is precisely the record that
did not exist. Putting it in the log also buys the audit story for free: "what
happened while I was away" and "when did I acknowledge it" become the same
query as `flk lineage` and `flk digest`, over one substrate.

### 2. Unread is a derived read model, rebuilt from the log

The log is append-only, so "seen" cannot be a mutable field on a filed record.
It is a second event, and the unread set is the fold of one over the other —
`NotificationFiled` with no matching `NotificationSeen`.

This is not a new pattern here. `MailboxRegistry::seed_from_events`
(`src/app/mailboxes.rs`) already reconstructs undelivered agent-message queues
and their dedupe set at boot from `MessageQueued` minus `MessageDelivered`, and
`App::new` already runs `persisted_events_after(0)` twice. The notification
projection joins that existing pass; it costs a fold, not an I/O path.

The alternative — a `last_seen_seq` watermark in `session.json` — is smaller
but answers a different question. A watermark can only say "everything before
here is read", which is wrong the moment the operator reads the interesting one
and leaves the rest. Per-id acknowledgement is what an unread *list* means.

It also makes acknowledgement auditable, which is the class of question #316
could not answer: not just "was it filed" but "was it ever read".

### 3. Retention is bounded by count, and reading is what makes a record evictable

Unbounded is a leak. Time-based drops the thing you were about to read. So
neither: the projection holds at most `MAX_NOTIFICATIONS` records, and when it
is full it **evicts read records before unread ones**, oldest first within each
class.

That inverts the failure the issue names. Under a time bound, the record most
likely to be dropped is an old one you have not looked at — which is exactly
the one you kept the log for. Under this bound, the record most likely to be
dropped is one you have already read, and an unread record is only evicted when
there is nothing read left to give up. Reading is the operator saying "done
with this", so reading is what makes a record cheap to lose.

Two bounds that are honest rather than pleasant:

- **Past the cap with everything unread**, the oldest unread record is evicted.
  There is no third option that is not a leak. The eviction is logged.
- **The durable floor is the event log's own rotation** (32 MB / 100 000 events
  per file, 4 rotated files — ADR-0005). A notification older than the retained
  window cannot be rebuilt after a restart, whatever the projection's cap says.
  With notifications at a handful per day against lifecycle events in the tens
  of thousands, the log window is the binding constraint, not the cap.

### 4. Fleet-awareness: the count gossips, the content is pulled

`peers.summary` already flows over one held SSH connection per peer (ADR-0009)
and already carries per-workspace summaries, system health, version, protocol
and icon. It gains one integer: the peer's unread count.

The list *content* is fetched from that peer on demand when the operator opens
it. ADR-0009 measured and rejected replication for fleet state; pushing
notification bodies to every node is replication, for data the operator reads
at most once. A count is one field on a payload that already arrives.

The failure mode is the one every other field on that row already has: an
unreachable peer shows a stale count and is marked stale by the existing
freshness machinery. That is better than the current answer, which is that the
outcome does not exist anywhere the operator can reach.

### 5. Who may file one — recommendation, not resolution

**Recommended for v1: no agent-facing filing verb at all.**

The three shapes the issue names do not need one:

| shape | who already knows |
| --- | --- |
| a finished background task | flock — #36's `AgentNotificationDelivery` is the decided outcome, already carrying title, target and kind |
| a refusal | flock — `agent_profile_unresolved`, `peer_not_configured`, check errors are all its own returns |
| cross-host | flock — the relay is its own code path |

flock files from what it already decides. That covers the gap with **no new
authority surface**, which is the strongest version of this feature to ship
first: the thing that fails is a projection, not a write path an agent can
reach.

If the operator wants agents to file directly, the shape that fits the existing
precedent is a **narrowed verb over the socket API and not on the MCP surface**
— `notification.file`, sanitized and rate-limited exactly as
`notification.show` already is, reachable by an agent only through `flk` on the
operator's own machine. That is the authority boundary ADR-0014 drew for
agent-initiated spawn and the one `src/mcp/tools.rs` states explicitly: the MCP
surface excludes mutating verbs by design.

**This ADR does not exercise that option.** It is named so the operator can
accept or refuse it deliberately rather than discovering it in a diff.

### 6. Surfaces

- **API**: `notification.list` (with an unread filter) and `notification.ack`.
- **CLI**: `flk notification list [--unread]` and `flk notification read <id>`,
  beside the existing `flk notification show`.
- **Counts feed what exists.** `StateTally` (`src/ui/state_signal.rs`) gains an
  unread term, which the sidebar and the #367 title badge render through the
  paths they already use. No second counter is introduced beside them — and the
  badge's load-bearing rule survives: it still vanishes when nothing wants
  attention, because zero unread contributes nothing.
- **A place to read them in the TUI** follows the existing panel language
  (AGENTS.md: reuse the modal/panel structure rather than inventing a screen).

## Explicitly out of scope

- **Alacritty has no OSC 9 backend**, so the interrupt layer does not fire on
  the operator's own terminal (`src/terminal_notify.rs` covers Ghostty, iTerm2,
  Kitty, WezTerm). That is a backend question about the *interrupt* layer, not
  the *durable* one, and it is orthogonal: a notification that is filed
  survives the interrupt never arriving, which is the whole point. It stays a
  separate issue.
- **Replaying history to new API subscribers** remains the non-goal ADR-0005
  set (that is broker territory). `notification.list` is a query, not a
  subscription backfill.

## Consequences

- A notification survives not looking, a restart, and the pane closing — the
  three losses the issue names — bounded by the log's retained window.
- "Was it filed" and "was it read" become the same kind of query as `flk
  lineage`, over one substrate, so the #316 class of question (*nobody tried*
  versus *everyone was blocked*) is answerable for refusals for the first time.
- The event log grows by one record per outcome plus one per acknowledgement.
  At a handful of outcomes a day against tens of thousands of lifecycle events,
  this does not move rotation.
- `notification.show`'s `Busy` / `RateLimited` / `Disabled` /
  `NoForegroundClient` outcomes stop being silent content loss: the display is
  still refused, but the record is kept and readable.
- Nothing here changes who may write. Under the recommendation, the only
  producer is flock itself.
