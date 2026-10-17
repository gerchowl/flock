# Structured-wire scripts (fixture version 1)

These JSON Lines scripts define G3's (#1595) fake-server inputs and expected
client behavior under ADR-0036 §4/§5/§5a and ADR-0038 §3. S7a supplies types,
parsing and generated TS; G3 executes them with a virtual clock and transport.
There is no endpoint, real token store, model, network or wall-clock sleep.

Each line has `v:1` and exactly one operation. Every complete script must start
with one `given` and end with an explicit `quiet`. Invalid script fields,
versions, selections and typed frames fail visibly. Transport frames permit
additive fields but reject unknown `t` tags. `expect` never supplies input or
manufactures success. Missing expectations fail at their bounded virtual
ceiling; mismatch, unexpected output and premature exit fail immediately.

| Step | Fields and meaning |
| --- | --- |
| `given` | `cursors{stream:number}`, `outbox:[{cmd_id,verb,args,principal,created_ms,first_sent_ms,status}]`, optional `resync`, `now_ms` and cached `idempotency_window_ms`. Restore last applied cursors and commands with their explicit status before any attempt. Empty maps/lists are explicit. |
| `client` | `action:{kind:"connect"}` starts the initial attempt. `subscribe{sub_id,stream}`, `unsubscribe{sub_id}` and `command{cmd_id,verb,args}` are caller inputs. The caller supplies deterministic subscription IDs. The client derives `after` and `resume_only` from its state, never from an expectation. |
| `expect_connect` | `not_before_ms` (default 0) and positive `within_ms` bound the next attempt inclusively. It does not start an attempt. |
| `open` | Complete upgrade, cancel the connect ceiling, begin the handshake ceiling. Every upgrade declines permessage-deflate. |
| `send` | `frame:ServerFrame`, optional `socket:"current"` (default) or `"previous"`. Inject into the active or most recently abandoned socket generation. Old callbacks must not send pong, renew liveness, apply events/barriers or change command outcomes. |
| `expect` | `frame:ClientFrame`, `fields:string[]`, optional `absent:string[]` (default empty). Match `t` plus deep JSON equality of the selected top-level fields. Require each absent optional field to be omitted. Other fields are ignored. The example remains typed; invalid/missing selections, overlap, duplicate absence or nonexistent optional fields fail. |
| `observe` | Optional `connection:string`, `streams{sub_id:{state:string,cursor:number\|null}}`, `outbox{cmd_id:OutboxStatus}`, optional `absent_streams`, `absent_outbox`, `connection_transitions`. Assert only specified entries; null means no applied cursor, while zero is meaningful. G3 owns connection/stream states; outbox statuses are the closed six-state enum below. |
| `deadline` | `stage:{kind:"connect"\|"handshake"\|"subscription"\|"command_ack",sub_id?,cmd_id?}`. Advance to the original stage ceiling, rejecting every early effect, with no duration override. Subscription and command stages require their identity. |
| `heartbeat_missed` | Withhold valid current-socket traffic and advance to 45 s from the last valid current-socket frame. The fake transport determines that instant; an old callback cannot shift it. |
| `close` | `code`, `reason`. Deliver a WS close; 4401/4403 are rejected here. |
| `drop` | Lose the active socket without a close, retaining applied state and outbox. |
| `token_refused` | `code:4401\|4403`. Enter NeedsToken, hold pending outbox and do not reconnect until credentials change. |
| `quiet` | Positive `ceiling_ms`. Advance the fake clock by that duration, requiring no client frame, close **or connection attempt** throughout. Required last, also usable between other operations. |

Initial connection and handshake ceilings are 10 s and 5 s. Subscription
progress and command ack ceilings are independently 30 s; unrelated traffic
cannot reset either. Heartbeat cadence is 15 s and liveness is 45 s. Reconnect
uses equal jitter in `[ceiling/2,ceiling]`, starting at 500 ms, multiplying by
two, capped at 30 s, and resetting only after 60 s healthy. All time is virtual.

`WireEvent = UnknownEvent | KnownSessionEvent | JsonValue`. Known events come
from #1723’s production known-event catalogue projection.
Unknown agent events use **exactly** `{type:"unknown",raw,shape}`: raw is the
original line (including non-JSON diagnostics), shape is the sender-supplied
harness/version key. A malformed known event must be wrapped, never accepted
as raw agent JSON. `JsonValue` is reserved for non-agent streams pending #1677;
command args/results are also deliberately opaque until that follow-up.
Error codes are open strings from ADR-0029's full registry, with future codes
accepted; data stays under `error.data`.

The scripts cover clean replay; connect/handshake expiry; nonempty and empty
snapshots; empty and nonempty bootstrap; lagged recovery from the applied cursor
without an invented unsubscribe requirement; heartbeat/protocol reconnect;
token refusal/revocation; read-only version skew; uncertain command recovery;
ordinary command failure; all four subscription refusals; cursor_ahead followed
by a sub explicitly **without after**; subscription-progress expiry that sends
unsub then sub for only that stream; sibling stream independence; and abandoned
socket frames ignored for cursor, barrier, pong and liveness purposes.

Snapshots finish at snapshot_to, independently of synced's captured replay
head. An empty snapshot completes immediately, even before synced. A newly
subscribed seq stream gets one synced; a later gap on an already current stream
does not invent a second barrier. agent/<id>/live has no seq, after or synced.
Command ack expiry makes only that command uncertain and never resends on the
same socket. Reconnect lookups retain principal/id/args and use resume_only:true;
session_unconfirmed becomes unknown_outcome with exit UNCONFIRMED and retry later.
G3 enforces these stateful rules; the format itself is not a client engine.

Round 3 runner contract (ADR-0038 §3):

- `given` optionally restores `resync{stream:{snapshot_to,unconfirmed:string[]}}`,
  `now_ms` (default 0) and `idempotency_window_ms` (last known window, optional).
  The virtual clock starts at `now_ms`; cached expiry runs even before a welcome.
  Every restored command has a required `status`, never an inferred one.
  `first_sent_ms` measures age for sent/uncertain entries, independently of
  `created_ms` (queue time). Missing first-send age after restart means
  unknown_outcome. Pending entries have no first-send time yet. The expiry
  scripts use creation at 0 and first send at 100 with a 2,000 ms window.
  Connecting scripts restore at 2,098, so every allowed 0–1 ms attempt stays
  below expiry at the sent observation. Disconnected restores at 2,099 and
  advances 1 ms to assert unknown_outcome exactly at age == window; a client
  using > instead of >= fails. Using creation time expires the command early.
- The closed outbox vocabulary is **pending, sent, uncertain, unknown_outcome,
  acked, failed**. Pending first sends use `resume_only:false`; restored sent
  and uncertain sends use `resume_only:true`. A 30 s ack deadline with the
  socket up enters uncertain, sends nothing, and a late ack can still settle
  it. Socket loss resends it as a lookup. A `session_unconfirmed` ack enters
  unknown_outcome. An entry expires when `now_ms - first_sent_ms >= idempotency_window_ms`.
  Expired entries become unknown_outcome in every connection
  state and are never resent. Changing principal drops the old outbox.
- `disconnect` closes the socket (1000), cancels reconnect and holds the outbox.
  `set_token{principal}` changes credentials and is the only exit from NeedsToken.
  For deterministic scripts the initial bearer is `fixture-token`; set_token
  supplies `fixture-token:<principal>` through the runner's credential provider.
  `network_change` and `foreground` bypass backoff immediately in Reconnecting;
  they cannot escape NeedsToken.
- `expect_connect` matches an attempt in the inclusive interval
  `[not_before_ms,within_ms]` measured from entry to that assertion, default
  lower bound zero. The clock rests at the actual attempt instant, never at the
  upper bound. An attempt already queued at the same instant has delay zero.
  Explicit connect/foreground/network/token-change attempts use 0–1 ms.
  A server retry uses the full equal-jitter interval, including the 30 s cap.
- `deadline` advances to the original named stage's ceiling, **checking the
  entire intermediate trace**. No state change, client frame, close or connection
  attempt may occur strictly before it; the stage effect must occur exactly at
  the ceiling. An early effect fails immediately, even if the final observation
  would match. Do not advance first and check only at the end. Unrelated traffic
  cannot move the stage origin. All waits remain virtual and bounded.
- `observe` optionally includes `absent_streams` and `absent_outbox`: assert
  missing keys, not null values or empty maps. Positive and absent selections
  cannot overlap or repeat. `connection_transitions` asserts the **exact ordered
  transition trace since the previous observe** (or since given for the first).
  Every observe drains that trace, including when it does not select it.
  This exposes Stale before the immediate Reconnecting without freezing a
  transient state or moving timers. Empty streams/outbox maps assert nothing.
- `expect_close{code}` matches the next client-initiated socket close, distinct
  from the server-stimulus `close`. Missing, wrong or extra closes fail. Ordinary
  `expect` still matches only client JSON frames. `quiet` prohibits frames,
  closes and connection attempts but permits internal state changes (expiry).

S7a's Rust self-tests feed deliberately wrong synthetic traces into a contract
oracle: early/late attempts, early deadlines, wrong close codes, missing Stale,
retained absent entries, wrong restored resume_only, and wrong action outcomes.
They validate the script assertions; they do not execute G3. G3 must consume the
same scripts with its actual client, fake transport and virtual clock.


Every script header has machine-readable `covers:["s7-min"]` or `covers:["s7"]`.
S7-min (#1746) selects clean, agent-gap, agent-send, ping-pong, token-refused and
token-revoked: hello/welcome/token, agent/<id> cursor replay with synced and gap
(no entity snapshot), agent.send with ack, ping/pong and 4401/4403 closes. These
scripts request no fleet/inbox/queue verb. All other scripts target full S7.
The field defaults to an empty list only when reading older version-one scripts;
the checked-in inventory requires exactly one of the two slice labels.

The generator also emits FixtureExamples.ts from every JSONL line as a literal
checked against FixtureLine. The strict ts-check recipe compiles that artifact;
the Rust drift test ensures it cannot silently lag behind the scripts.
