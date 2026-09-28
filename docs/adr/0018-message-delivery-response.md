# ADR 0018 — A message says how much it needs, a recipient can say "not now" and must say so, and an idle agent is reachable

- Status: Accepted
- Date: 2026-09-28 (accepted 2026-09-28)
- Issues: #316 (the model this records), #320/#323 (cross-host addressing, landed),
  #280 (sender intent, landed as two tiers), #408 (tracking issue for the rest).
- Amends: ADR-0008. Its two-channel split and its wake rule stand unchanged; this
  ADR settles the half 0008 left open — *when* a message reaches an agent and
  *what the sender learns*.
- Decision owner: operator.

## Context

ADR-0008 made agent messages ride the tool surface: content is pulled through
`flock_msg_read`, and a wake is pushed at a turn boundary by the agent's stop
hook, naming a count and a tool and never a body. That is structurally right and
behaviourally incomplete, in three ways measured on the fleet:

1. **An idle agent is unreachable.** The wake rides the `Stop` hook, so an agent
   that has already stopped never learns it has mail until a human prompts it.
   Delivery latency for an idle pane is unbounded. On sage (2026-09-15) a
   `tvk376 → tvk377` message sat 131 minutes with an idle recipient and zero
   delivery attempts.
2. **Urgency is only half expressible.** #280 landed `fyi` / `needs_reply`, but
   nothing in the wake path reads it — an `fyi` still burns the recipient's turn —
   and there is no way to say "I am blocked on you".
3. **"Not now" is indistinguishable from silence.** `msg.mute` (#316's receiver
   half) suppresses the wake, bounded at 30 minutes, but the sender learns
   nothing. A muted recipient and a dead one look identical from the other side.

#316 proposed the model below and was closed on the reading that the event log
showed almost no message traffic. #320 later showed that reading was wrong: the
log records successful sends only, so it could not distinguish "nobody tried"
from "every attempt was blocked". This ADR records the model and supersedes
that closure.

## Decision

### 1. Three sender-declared intents, and the wake reads them

| intent | inbox | turn-boundary nudge | idle wake | attention surface | operator escalation |
| --- | --- | --- | --- | --- | --- |
| `fyi` | yes | no | no | no | no |
| `needs_reply` | yes | yes | yes | no | no |
| `blocking` | yes | yes | yes | yes | when the recipient is muted |

`fyi` never costs the recipient a turn; it is read the next time the agent reads
its inbox for any reason, and a nudge fired for another message reports it in
the count. The intent is what the SENDER wants; flock decides how hard to knock
from it, and no sender text ever reaches the wake.

**An answer wakes whoever asked.** A message whose `in_reply_to` names a
`needs_reply` or `blocking` message wakes its recipient as if it were
`needs_reply`, whatever its own stamp. A reply's own intent says whether the
reply *asks something back*, and it defaults to `fyi` because answering is what
a reply normally does; without this rule the answer to an agent's own question
would never nudge it, and the question would be asked and then not heard. The
one exception is the §3 deferral: it is `in_reply_to` a waking message by
construction, but it carries no answer, only "later", so it stays non-waking —
otherwise a mute would reach back into the sender's turn, which is exactly the
interruption §3's deferral exists to replace. The rule reaches as far as the
server remembers the question — its delivery history and its record of
questions relayed away, both bounded — so an answer to a question that has aged
out of both is read at its own stamp.

`blocking` is sender-declared and therefore sender-abusable, so it carries a
cost: its own per-sender rate limit, tighter than the general one, declared in
config (`[msg] blocking_per_hour`) rather than compiled in. A spent budget
**downgrades** the message to `needs_reply` — it is still delivered and still
wakes, and the send result says it was downgraded — rather than refusing it:
`blocking` is a courtesy tier, and what it adds (the operator's attention) is
exactly what a sender over budget has had its share of. The budget is keyed only
on what the receiving server itself attested: an agent its process ancestry
proved, keyed by that agent. Every unattested sender — each relayed message and
each socket client outside a pane — shares ONE bucket, because on that path both
the sender id and its host are claims, and keying on a claim lets a caller mint
a fresh budget per invented name or spend a real agent's by naming it. Sharing
is safe precisely because exhausting the budget only downgrades: nobody can
silence anyone by spending it.

Sender identity shown on an operator surface — the attention label, the
escalation notification — is a validated agent id or a server-minted pane id,
never caller text: an identity that fails the agent-id format is refused at
ingress, and a surface with nothing validated to show says "unknown sender".

An intent a receiving server does not recognise — version skew across the relay
— is treated as `needs_reply`: skew fails toward the recipient hearing about
it, never toward silence.

### 2. An idle agent is woken by a flock-authored constant

When a wake-worthy message (intent ≥ `needs_reply`) is queued for a pane whose
agent is `Idle`, flock types a fixed sentence into the pane and submits it:

> You have N unread message(s) from other agents. Read them with the
> `flock_msg_read` tool.

This is not the keystroke injection ADR-0008 rejected. The injected text is a
constant flock authors, carrying a count and nothing else; the body is still
pulled through MCP, and the authority boundary of 0008 is untouched.

The wake fires only when ALL hold, and every one is re-checked immediately
before the keystroke:

- the agent state is `Idle` and has been for at least a settle window, and is
  not stale (a hook-authority report older than its TTL does not count as idle);
- no operator keystroke reached that pane within a quiet window — flock does not
  type over a human;
- the fleet is not paused and the recipient is not muted;
- the agent is one whose integration exposes the inbox tool; others stay
  pull-only, as ADR-0008 already accepts;
- no idle wake has already been sent for the currently queued set. The in-flight
  marker clears when the agent reads its inbox or leaves `Idle`, so a wake is
  never re-typed on every tick.

The settle and quiet windows are config (`[msg]`, alongside the existing `enabled` and `allow_from`), and the feature has a
kill switch (`idle_wake`, default on).

### 3. A mute must answer

`msg.mute` gains an optional `reason`. Muting still suppresses the wake and
never the delivery. In addition, every `needs_reply` or `blocking` message that
is queued while the recipient is muted — including those already waiting when
the mute is set — produces exactly one automatic reply to its sender:

- `in_reply_to` the message's `correlation_id`;
- intent `fyi` **by construction**, so two mutually deferring agents cannot
  ping-pong;
- correlation id `<the message's correlation_id>:deferred`. The suffix is the
  contract §1's reply rule keys on: it is how a server recognises a deferral —
  which replies to a waking message by construction — and keeps it from waking
  the sender, on whichever host it lands;
- a body stating that the recipient deferred, the reason if one was given, and
  the time the mute lifts — a deadline the sender can act on.

The reply routes through the ordinary reply path, so a sender on another host
hears it the same way. `fyi` messages produce no deferral: nothing was owed.

### 4. When sender and recipient disagree, the operator decides

A `blocking` message to a muted recipient means the two have disagreed about
urgency. Neither wins silently: the operator is told through the notification
log (ADR-0016), naming sender, recipient and count — never the body — and the
recipient pane appears in the attention surface under a label of its own.

The attention entry for waiting mail is its own label, never the `blocked` agent
state. `blocked` means "waiting on input", and #311 spent a PR making that state
honest; overloading it would erode what it means.

### 5. One surface, every transport

All of the above is expressed on the socket API and the MCP tools in the same
terms (`flock_msg_send` gains `blocking`; `flock_msg_mute` gains `reason`), and
holds identically for a message relayed from another host: the intent rides the
envelope, and the idle wake, deferral and escalation happen on the recipient's
own server, where its state is known.

## Consequences

**Gained.** Every message now has a bounded answer to "will it be seen": an idle
agent is reached, a busy one says when, and an unresolvable disagreement reaches
a human. A sender can express urgency without the recipient paying for every
notice.

**Given up — deliberately.** Flock now types into agent panes on its own
initiative, which ADR-0008 had removed entirely. The constraint that keeps this
safe is narrow and must stay narrow: a constant, a count, a freshness re-check
and an operator-quiet window. Any proposal to widen what is typed — a sender
name, a subject line — reopens the hole 0008 closed and needs its own ADR.

**Per-integration cost.** The idle wake assumes an agent that treats a typed
sentence as a turn and has the inbox tool. That is Claude today; other runtimes
are explicitly pull-only until their integration says otherwise.

**Operational dependency.** The turn-boundary half requires the agent's `Stop`
hook to carry flock's entry. On nix-managed hosts that is declared in the host's
Home-Manager configuration, not installed by `flk integration install`, which
cannot write a read-only `settings.json` (g-fleet#156).

## Alternatives rejected

- **Waking idle agents through the operator** (a notification asking a human to
  prompt the agent). Keeps flock out of the keyboard entirely, but makes every
  message's latency a human's latency — the failure this ADR exists to fix.
- **Putting the sender's words in the idle-wake text** ("reviewer asks: …").
  Faster to act on, and exactly the injection ADR-0008 rejected: the pane would
  receive another agent's words as a user turn.
- **Silent mute.** Cheaper, and makes deferral indistinguishable from death —
  #316's "black hole wearing a politeness hat".
- **Overloading `blocked` for waiting mail.** One less label, at the cost of the
  meaning #311 restored.
