# Structured queue scheduling and lifecycle

ADR-0036 §3/§4's runtime owner (#1879, part of #1802 and #1428) wraps the
admission/batch core from #1878. Construct it once for a fresh runtime
generation, with exclusive ownership of user-input submission. The event pump
feeds persisted session events in order, while the registry supplies the S4
reducer's current AgentState. Final events drain before process exit, and queue
cuts publish only after successful operations. Disk work stays on the dedicated
worker.

Delivery queries S6's live `Runtime::waiting_on_human` book directly. An open
request or a lookup error cannot be treated as idle. S4 must report live,
attention-free idle. A fresh runtime can accept its first input while S4's
turn is still Unknown: init traffic alone cannot establish idle, and the
private stdin admission remains the final guard. After an accepted offer or
known top-level result, further inputs require a fresh S4 Idle snapshot. The
worker tags each snapshot with the last session sequence reduced into it. Submit
invalidates the cached gate; a result invalidates it again, and no snapshot
covering less than that result sequence can release the next item. Equal Idle
values with newer coverage still reach the queue. A refused offer writes no
input and restores or holds the candidate; it never re-runs its gate.

Before offering stdin, the scheduler durably replaces every member with the
same batch ID and outstanding-result flag. It retains an owned reservation
until it consumes that batch's known top-level result. Writer completion,
S4 becoming idle, and a lifecycle completed event do not release it early.
An accepted Runtime::try_submit result means only that the writer accepted
the envelope; items remain queued until the harness reports started.

Matching command_lifecycle started/completed/cancelled updates all members
atomically. Unknown IDs/states, unknown result variants and nested results
do not release the reservation. Top-level success/execution-error results
retire the outstanding flag, mark remaining members done and preserve
cancelled states. Producer attribution and batch IDs remain on the journal
items and their raw audit records, including verbatim native commands.

`deliver()` returns a typed `Offer`: `Submitted`, `NoEligibleItems`, or
`Pending { batch_id, since_ms, cause }`. `status()` exposes the same turn
blocker without writing. A reservation keeps its original batch ID/time across
observations; S4/S6 gate observations have no batch ID. Pending causes are:

- `awaiting_result`: stdin accepted the batch, but the queue worker has not
  consumed its known top-level result.
- `turn_end_unknown`: lossy output left the boundary unconfirmed. The S3a
  writer guard stays closed; no input crosses that boundary.
- `cancelled_awaiting_result`: matching harness cancellation keeps the items
  cancelled, but does not establish the turn boundary. ADR §3 operator
  take-back, which returns items to queued, is not implemented in this slice.
- `state_not_ready`: S4 lacks live, attention-free idle/fresh bootstrap.
- `waiting_on_human`: S6 still has an open permission/question request.

The three turn-pending causes end only on a real result or runtime exit/restart.
Restart holds outstanding items `session_unconfirmed`; they are never
automatically resent. Pending age never triggers a timeout or recovery. Gate
causes recover only when their S4/S6 evidence changes.

A known busy refusal restores the exact original members and returns
session_busy. A typed `structured_turn_end_unknown` offer refusal also
restores the original items/order, without holding them or claiming any stdin
write. It returns `Pending { cause: turn_end_unknown }`; repeated status/delivery
calls do not rewrite the journal or retry the offer before its real result or
exit. This Pending's batch ID and time describe the candidate at refusal time
(ADR-0036 §3), not the current queue head or a newly formed batch. Later admission,
edits, cancellation or removal do not refresh that historical ID. A real result
or runtime exit clears it; the next accepted batch has its own ID and time.
Other offer errors hold the candidate with input_write_failed and the source
error. Uncertain journal I/O stops the operation before success;
reopen under the lease instead of blind retry. Process exit without a result
holds every unresolved member with no_result, including a completed lifecycle
whose result never arrived. Cancellation remains terminal. A fresh scheduler
holds any previous outstanding or unrecognized result flag with
session_unconfirmed, preventing automatic replay into a new process.

Worker integration (#1955, Part of #1803) runs the sole journal/scheduler on the
existing session event pump, away from the server loop. Persisted events reach
it before registry S4 projection; generation-fenced registry snapshots supply
the S4 gate and the live S6 book supplies permission state. Queue-owned runtimes
refuse raw submission. Nonblocking requests acknowledge only buffer acceptance;
typed completions carry durable admission, offer/pending status and atomic cuts.

Request and S4 channels hold one item each; completions hold four plus one pending
owner completion. Saturation backpressures queue work and session delivery.
Consumers drain through terminal completion before relaunch; terminal cuts remain
available after process exit. Relaunch refuses with `session_busy` while any
completion remains unread. An unchanged storage refusal does not generate another
completion per harness event.

The application drains completions for lifecycle publication and durable send
acknowledgements before relaunch. Shutdown cancellation explicitly reports
incomplete notification delivery and requires durable replay. Final persisted
records drain before reservation retirement. Operator admission uses the shared
send service (#1957); authenticated message custody routing remains #1958.

The worker machinery in #1955 is not production enforcement by itself. The
production launch path is #1843; #1928 remains open until that path exists.

The internal [queue publication provider](structured-queue-publication.md) exposes
committed cuts without competing for the Runtime receiver. Cold recovery and
global facts remain follow-up parts of #1956.
