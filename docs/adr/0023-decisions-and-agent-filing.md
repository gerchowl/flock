# ADR 0023 — A decision is a record with an answer, and an agent may raise one

- Status: Accepted
- Date: 2026-10-04
- Issues: #395 (a decision waiting on the operator is not an outcome), #516 (the
  reading surface, shipped in #534), #517 (the misfiled escalation, shipped in #520).
  **Answers ADR-0016 §5 and §3's retention premise**, both of which
  `docs/adr/0022-adr-0016-implemented-unaccepted.md` recorded as open. Does not
  change ADR-0016's status; see §7.
- Decision owner: operator.

## Context

ADR-0016 built the notification log: outcomes are filed as durable events, unread is a
projection over them, and there are exactly two verbs (`notification.list`,
`notification.ack`). It shipped in #381 and the TUI can now read it (#534).

Two questions it deliberately left open have proven to be one question.

**§3's retention premise.** `trim()` evicts **read**-first, and past the cap evicts the
oldest **unread** (`src/app/notifications.rs:158-178`, cap 512 at `:34`). ADR-0016 §3
rests on "the thing you already dealt with is the safe casualty". For an **outcome**
that holds — reading an outcome tells you it is handled. For a **decision** it is
exactly backwards: reading a question is the *beginning* of dealing with it, so a
decision is unread until answered, which places it permanently in the class `trim()`
gives up first. A decision the operator has seen and is thinking about is evicted
before a refusal they never opened.

The shipped panel already reports this honestly — `src/ui/notifications.rs` states that
records may already have been evicted — but honesty about a defect is not a fix.

**§5, who may file.** §5 recommended no agent-facing filing verb for v1, named a
narrowed `notification.file` **not** on the MCP surface as the alternative, and closed
by saying the option is named "so the operator can accept or refuse it deliberately
rather than discovering it in a diff". #381 then shipped `notification.show` on the
socket API — neither the recommended shape nor the named alternative — so §5 was never
answered.

The substrate anticipated the answer. `NotificationSource` (`src/api/schema.rs:372-378`)
already exists and its doc says flock "is the only producer today; the field exists so a
later agent-facing verb is distinguishable in the log rather than indistinguishable from
flock's own judgement."

## Decision

### 1. `Decision` is a fourth record kind, and answering is what retires it

```rust
pub enum NotificationRecordKind {
    Attention,   // Something is waiting on the operator
    Outcome,     // Something finished and produced a result
    Notice,      // Everything else worth keeping
    Decision,    // Something is asking the operator to choose
}
```

**A `Decision` is evicted when it is *answered*, not when it is *read*.** `seen` becomes
a rendering concern — whether it draws the unread dot — and a new `answered_at` becomes
the retention concern. So `trim()` keeps read-first for `Attention`/`Outcome`/`Notice`
(ADR-0016 §3 unchanged) and answered-first for `Decision`.

**Unanswered decisions are exempt from the cap's unread eviction.** If the cap is
reached with decisions outstanding, the oldest *notice-class* unread record is evicted
first; a decision goes only if the operator has answered it. A backlog of open
questions is the one thing in this store an operator cannot reconstruct, because #516's
host-local ids (`src/app/notifications.rs:281-289`) and ADR-0005's rotation (32 MB /
100k events, 4 files) mean an evicted decision is gone with no trace — the panel already
says "never answered and evicted" is indistinguishable from "never filed".

### 2. An answer is a message with `in_reply_to`, and the operator is its author

ADR-0018 §1 already decided the shape: **"An answer wakes whoever asked"**, and an answer
*is* a message carrying `in_reply_to`. So there is no new verb and no new concept:

```
flock_notification_answer { notification_id, body }
```

resolves to a message from a **server-minted** pane id to the asking agent, with
`in_reply_to` set. That satisfies ADR-0018's authority rule — operator identity is "a
validated agent id or a server-minted pane id, never caller text" (`docs/adr/0018:82-86`)
— because flock mints the pane id from the operator's own session rather than accepting
one from a caller. Answering marks the decision answered; it does not remove the record.

**Why a message and not a field.** A decision's answer usually needs prose ("use the
staging branch", "no, not until CI is green"), and it usually needs to *reach the agent*
— a decision answered into a log nobody reads is a decision not made. Reusing the
message path gets delivery, `in_reply_to` threading, escalation and mute for free, and
ADR-0018 has already decided all of it.

### 3. An agent may raise a decision, through a narrowed verb

This answers §5: **yes.**

```
flock_notification_ask { title, question, options?, urgency? }
```

rejected on the MCP surface, exactly as ADR-0015 §5 requires for a mutation an agent
initiates — reachable by an agent only through `flk` on the operator's own machine, the
authority boundary ADR-0014 drew for agent-initiated spawn and that
`src/mcp/tools.rs` already states. Sanitized and rate-limited as `notification.show`
already is.

**The three constraints that make this safe, each of which is a refusal:**

- **`NotificationSource::Agent`** stamps the record so the log distinguishes an agent's
  question from flock's own judgement — the field was built for this and has been
  carrying no second value.
- **A decision is text into the operator's attention, not into a machine.** ADR-0014 §4
  established that a dispatcher's prompts come from GitHub issue bodies — attacker-
  authored — and that is why the spawn prompt is fenced behind a flock-owned preamble
  with control bytes refused. The same applies here: the question is fenced, control
  bytes refused, and **it is never rendered as an instruction**, only as a question with
  an optional option list.
- **The agent cannot answer its own decision**, cannot mark one resolved, and cannot
  raise one `blocking` enough to change any ceiling. An unanswered agent decision
  occupies operator attention until the operator acts, which is the honest cost and the
  reason the urgency field is advisory and bounded.

### 4. The operator's shape is a pane with answers in it

The panel shipped in #534 is a reader. It becomes the place decisions get answered, and
that is what makes §1's retention rule worth having: a decision you can answer where
you see it does not stay unread.

Reused, not invented: the settings-overlay modal language (#534 already extends it), the
existing `↵ primary` / `[hint verb]` / `[esc close]` button row, and the `a ack unread`
armed-then-committed shape from #534 — which is exactly the right interaction for a bulk
"answer all with the same answer", and is **not** the right interaction for answering a
*specific* question. So answering is per-row, with the option list as buttons, and the
confirm path is only for the bulk case.

Agents keep running while the operator answers. Nothing here blocks a pane, and an
unanswered decision never escalates into a stall — it is a question in a list, and the
list is a surface the operator opens when they choose.

### 5. The badge term counts decisions separately

`{n}U` already exists for unread-beyond-live-states (#367). A decision gets its own
term, because "12 things happened" and "3 questions are waiting" are different urgencies
and merging them is how the second gets ignored. `B`/`D` are agent-state counts and stay
untouched.

### 6. What is deliberately not decided here

- **Whether an agent may raise a `blocking` decision that gates a fleet ceiling.** Not
  this ADR. §3's urgency is advisory; anything load-bearing is a separate question with
  its own refusal shape.
- **Whether a decision expires.** A question can go stale. Time-based expiry needs a
  rule for what a stale question *means*, and that is a decision, not a default.
- **Multi-hop.** `in_reply_to` answers the asking agent. A decision raised by an agent
  on a spoke, answered by an operator on the hub, is covered by the existing relay; a
  decision whose answerer is *another agent* is not, and should not be.

## Consequences

**Gained.** §5 is answered and §3's premise is repaired for the one record class it was
wrong for. An agent can ask the operator a question and get an answer that reaches it.
The operator answers in the pane they already have, and open questions stop being the
first thing evicted.

**Given up.** `NotificationRecordKind` gains a variant and `NotificationEntry` gains a
field, so the schema, the wire, the badge and the panel all move. `trim()` gains a
second policy rather than one rule, which is more code in the one place that decides what
the operator loses. And agents can now put something in the operator's attention without
being asked — which is the cost ADR-0016 §5 was deferring on, accepted here with the
source stamp, the fence and the three refusals as the price.

**Unchanged.** ADR-0010 decision 6 and ADR-0015 §5 — this says nothing about an agent
filing *issues*. `flock issue drop --file-it` remains the only write path there.

## On ADR-0016's status

This ADR answers §5 and repairs §3's premise, but **ADR-0016 stays `Proposed`**, for the
reason ADR-0022 gave: the `adr-matrix` gate keys on the status header, and accepting it
would assert a decision whose remaining parts are open. Accepting it now would also
contradict ADR-0022, and a decided ADR is immutable — so the status question is a
separate, explicit act, not a side effect of this one. `docs/adr/0022` remains accurate
except for §4's claim that ADR-0016 is the only Proposed ADR with an implementation,
which the concurrent ADR-acceptance pass made stale; that amendment is owed.

refs #395, #516, #517
