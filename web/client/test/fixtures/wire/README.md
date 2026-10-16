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
| `given` | `cursors{stream:number}`, `outbox:[{cmd_id,verb,args,principal,created_ms}]`. Restore last applied cursors and pending commands before any attempt. Empty maps/lists are explicit. |
| `client` | `action:{kind:"connect"}` starts the initial attempt. `subscribe{sub_id,stream}`, `unsubscribe{sub_id}` and `command{cmd_id,verb,args}` are caller inputs. The caller supplies deterministic subscription IDs. The client derives `after` and `resume_only` from its state, never from an expectation. |
| `expect_connect` | `within_ms` requires the next connection attempt to start within this positive virtual backoff ceiling. It does not start an attempt. |
| `open` | Complete upgrade, cancel the connect ceiling, begin the handshake ceiling. Every upgrade declines permessage-deflate. |
| `send` | `frame:ServerFrame`, optional `socket:"current"` (default) or `"previous"`. Inject into the active or most recently abandoned socket generation. Old callbacks must not send pong, renew liveness, apply events/barriers or change command outcomes. |
| `expect` | `frame:ClientFrame`, `fields:string[]`, optional `absent:string[]` (default empty). Match `t` plus deep JSON equality of the selected top-level fields. Require each absent optional field to be omitted. Other fields are ignored. The example remains typed; invalid/missing selections, overlap, duplicate absence or nonexistent optional fields fail. |
| `observe` | Optional `connection:string`, `streams{sub_id:{state:string,cursor:number\|null}}`, `outbox{cmd_id:string}`. Assert only specified entries; null means no applied cursor, while zero is meaningful. G3 owns the state/status values; S7a exports their shape without closing the vocabulary. |
| `deadline` | `stage:{kind:"connect"\|"handshake"\|"subscription"\|"command_ack",sub_id?,cmd_id?}`. Advance to the named stage's failure ceiling, with no duration override. Subscription and command stages require their identity. |
| `heartbeat_missed` | Withhold valid current-socket traffic and advance to 45 s from the last valid current-socket frame. The fake transport determines that instant; an old callback cannot shift it. |
| `close` | `code`, `reason`. Deliver a WS close; 4401/4403 are rejected here. |
| `drop` | Lose the active socket without a close, retaining applied state and outbox. |
| `token_refused` | `code:4401\|4403`. Enter NeedsToken, hold pending outbox and do not reconnect until credentials change. |
| `quiet` | Positive `ceiling_ms`. Advance the fake clock by that duration, requiring no client frame **or connection attempt** throughout. Required last, also usable between other operations. |

Initial connection and handshake ceilings are 10 s and 5 s. Subscription
progress and command ack ceilings are independently 30 s; unrelated traffic
cannot reset either. Heartbeat cadence is 15 s and liveness is 45 s. Reconnect
uses equal jitter in `[ceiling/2,ceiling]`, starting at 500 ms, multiplying by
two, capped at 30 s, and resetting only after 60 s healthy. All time is virtual.

`WireEvent = UnknownEvent | KnownSessionEvent | JsonValue`. Known events come
from the existing catalogue; #1656 owns its remaining nested literal tags.
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
session_unconfirmed remains uncertain with exit UNCONFIRMED and retry later.
G3 enforces these stateful rules; the format itself is not a client engine.
