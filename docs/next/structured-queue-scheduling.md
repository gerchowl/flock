# Structured queue scheduling and lifecycle

ADR-0036 §3/§4's runtime owner (#1879, part of #1802 and #1428) wraps the
admission/batch core from #1878. Construct it once for a fresh runtime
generation, with exclusive ownership of user-input submission. The registry
feeds durable session events in order, supplies the S4 reducer's current
AgentState, drains final events before process exit, and publishes queue cuts
only after successful operations. Disk work stays on the dedicated worker.

Delivery queries S6's live `Runtime::waiting_on_human` book directly. An open
request or a lookup error cannot be treated as idle. S4 must report live,
attention-free idle. A fresh runtime can accept its first input while S4's
turn is still Unknown: init traffic alone cannot establish idle, and the
private stdin admission remains the final guard. After an accepted offer or
known top-level result, further inputs require S4 Idle. A refused offer writes
no input and restores or holds the candidate; it never re-runs its gate.

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

A known busy refusal restores the exact original members and returns
session_busy. Other offer errors hold the candidate with input_write_failed
and the source error. Uncertain journal I/O stops the operation before success;
reopen under the lease instead of blind retry. Process exit without a result
holds every unresolved member with no_result, including a completed lifecycle
whose result never arrived. Cancellation remains terminal. A fresh scheduler
holds any previous outstanding or unrecognized result flag with
session_unconfirmed, preventing automatic replay into a new process.

Actual producer routing, server ownership, global lifecycle acknowledgements
and registered submit:false refusal remain #1803. Steer, interrupt, urgent
priority, queue UI and System One remain their approved follow-up scopes.
