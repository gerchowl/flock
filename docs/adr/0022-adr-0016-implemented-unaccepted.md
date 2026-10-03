# ADR 0022 — ADR-0016 shipped without its decision; the log exists and §5 is now the operator's answer to give

- Status: Accepted
- Date: 2026-10-03
- Issues: #395 (the decisions waiting on this), #516 and #517 (both filed against a
  `Proposed` ADR with code already in production). **Amends ADR-0016** — records that
  its implementation shipped while it was `Proposed`, and settles §5 by declining to
  settle it.
- Decision owner: operator.

## Context

ADR-0016 (`docs/adr/0016-operator-notification-log.md`) is `Status: Proposed`. Its
implementation shipped in #381. So the repo contains an undecided ADR with production
code behind it, and `docs/adr/README.md:28` tells every reader — human and agent — that
no notification log exists.

This is a records problem before it is a design problem, and it is now blocking three
real issues. #516 asks for the TUI reading surface ADR-0016 §6 promised and #381's PR
body deferred without filing; #517 is a misfiled escalation; #395's three
load-bearing arguments all ask to amend §1, §3, §5 or §6. All three would be built
against a substrate whose status is a fiction.

## Why ADR-0016 was not flipped to `Accepted`

This ADR exists instead, and the reason is specific rather than procedural.

`docs/adr/README.md:5-7` and the `adr-matrix` gate
(`.pre-commit-config.yaml:79-84`) key on each ADR's own `- Status:` header:
**every Accepted ADR must be cited as `ADR-NNNN` in `FEATURE-MATRIX.md`.** Accepting
ADR-0016 therefore does not merely record a decision — it **keys a gate that
immediately wants a FEATURE-MATRIX row for a design whose §1, §3, §5 and §6 are all
still open**, which is precisely what #395 demonstrates. That manufactures pressure
toward a closure that is being withheld deliberately.

## Decision

### 1. ADR-0016 is recorded as implemented-unaccepted. Its status stays `Proposed`.

The index keeps saying `Proposed`, and this ADR is the record of why that is now
accurate rather than stale: the log exists, and the questions the ADR was written to
put in front of the operator are still in front of her.

A decided ADR is immutable (`docs/adr/README.md:3`), so this cannot be fixed by
editing ADR-0016's header. It is recorded here.

### 2. §5 is explicitly still open, and stays that way until answered

ADR-0016 §5 recommends "no agent-facing filing verb at all" for v1, states the
alternative (`notification.file`, narrowed, **not** on the MCP surface), and closes:

> **This ADR does not exercise that option.** It is named so the operator can accept
> or refuse it deliberately rather than discovering it in a diff.

The option was then effectively taken by #381 — `notification.show` exists on the
socket API. That is not the same as §5's recommended shape, and it is not the
alternative §5 named. So §5 has **not** been answered, and no reader should infer
otherwise from `notification.show` existing.

**This ADR does not answer §5.** The agent's investigation reported a recommendation
against flipping the status; the operator accepted the recommendation and adopted it.
Answering §5 remains the operator's, and #517 is explicitly independent of it: the
*classification* of an escalation that flock already files is a bug fix, while *who may
file one* is §5.

### 3. Work that does not depend on §5 may proceed, and the boundary is stated

- **Unblocked:** #517 (reclassify what flock already files; it changes no authority
  surface) and #516's classification half.
- **Still gated:** anything giving an agent a **new** way to file, and the retention
  question #395 raises — that `trim()` evicts read-first and then the oldest
  **unread** (`src/app/notifications.rs:158-178`), so an unanswered decision is
  exactly the record given up first. That is ADR-0016 §3's premise and it is not
  settled.

### 4. A `Proposed` ADR with shipped code must be visible, not discoverable by reading

11 of 20 ADRs are `Proposed`, and ADR-0016 is the only one implemented. So the index's
`Proposed` column means at least three different things: undecided, undecided and
unimplemented, and implemented anyway. That ambiguity is what let this go unnoticed.

The convention this sets: **an ADR gains an `Implemented:` line in its header when
code lands behind it while `Proposed`.** That is an additive header field, not a status
change, so it keys no gate. It makes the condition visible at the ADR rather than
requiring a reader to diff the tree.

## Consequences

**Gained.** #395, #516 and #517 stop building against a fictional substrate, and the
reason ADR-0016 is undecided is recorded where a reader will find it.

**Given up.** The index still says `Proposed` for a feature that shipped, so the
`adr-matrix` gate still does not require a FEATURE-MATRIX row for the notification
log. That is the deliberate trade: an accurate gate target is worth less than not
keying a gate toward an open decision.

**Not settled.** §5, and ADR-0016 §3's retention premise. Both are the operator's and
both are named above rather than quietly left.

Refs #395, #516, #517.