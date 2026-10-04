# ADR 0024 — The eleven implemented `Proposed` ADRs are decided; what each acceptance does and does not assert

- Status: Accepted
- Date: 2026-10-04
- Issues: #544 (the audit behind this sweep), #543 (ADR-0023, merged first so
  ADR-0016's status could move without contradicting ADR-0022), #516/#517.
  **Corrects ADR-0022 §4's claim** that ADR-0016 "is the only one with a shipped
  implementation" — see §3. Does not amend ADR-0022 itself, which is immutable.
- Decision owner: operator.

## Context

Eleven ADRs sat at `Status: Proposed`. ADR-0022 recorded that one of them
(ADR-0016) had shipped its code anyway, so `Proposed` in the index meant at
least three different things: undecided, undecided and unimplemented, and
implemented anyway.

That was the state when the index held eleven `Proposed` rows. It is not now,
and the reason is worth recording plainly: **the ambiguity was never about
ADR-0016.** It was about the index as a whole, and ADR-0022 diagnosed a
condition it then fixed for one row.

A closed issue is not evidence of implementation — a spike that concluded "don't"
closes its issue too. So #544 audited each of the eleven against `src/` rather
than against its tracker. Six had code that implemented what they decided, three
had partial code with a question left open, and **two had no code at all**.

## Decision

### 1. All eleven are `Accepted`, and each header says what its acceptance covers

`Accepted` is a claim about a **decision**, not about code. Because the eleven
did not earn the status in one way, each carries a header line naming what is
implemented and what remains open — `Implemented:` for the ten whose claim is
about code, and `Scope of acceptance:` for ADR-0019, whose acceptance *is* a
go/no-go answer rather than a description of a diff.

Three of the eleven would have been actively misleading as a bare `Accepted`:

- **ADR-0011 and ADR-0012 have zero lines of code.** No `src/usecase/`, no
  `src/gui/`, no `RpcTransport`/`tauri`/`AskUserQuestion`, no `TranscriptEntry`,
  no FTS index. `src/agent_transcript.rs` is the pre-existing Claude-only reader
  these ADRs already describe as existing; it is not ADR-0012's model. Accepting
  them records a desktop GUI architecture and a SQLite transcript index as
  *decided*. Their `FEATURE-MATRIX` rows say **"Not implemented"** in the row
  itself, on ADR-0021's precedent, so the index cannot be read as a claim that a
  Tauri app exists.
- **ADR-0004 shipped only its bridge.** `protected_branches` and the hardcoded
  floor (`is_protected_branch`) are real; the committed `.flk.toml` policy layer
  this ADR decided has **zero** occurrences in `src/`. Its header says so, and
  its row says so.
- **ADR-0019's acceptance is the go decision, and the go decision is scoped to
  opt-in.** All five decisions shipped behind a default-off flag while the ADR
  still read `## Decision (proposed)` and closed with a no-go on the default —
  code and go/no-go inverted. Accepting settles *whether to build the opt-in
  layer*. The no-go on making it the default is **still in force** and the row
  says so, because "Accepted" on that ADR read alone invites exactly the wrong
  inference.

### 2. ADR-0016 moved because ADR-0023 answered what ADR-0022 was protecting

ADR-0022 §1 held ADR-0016 at `Proposed` for one specific reason: accepting it
**keys the `adr-matrix` gate toward a design "whose §1, §3, §5 and §6 are all
still open"**, manufacturing pressure toward a closure being withheld on
purpose. §1–4 and §6 have shipped since (#381, and the panel in #534). **ADR-0023
answered §5 and repaired §3's retention premise**, so the objection that
withheld acceptance is spent.

ADR-0016's header records that, so a reader who remembers ADR-0022 §1 sees why
the status moved rather than assuming the two records simply disagree. ADR-0023's
own implementation is **not** yet in the tree — this sweep moved statuses, it did
not ship code — and ADR-0016's header says that too.

### 3. ADR-0022 §4's "only one" claim is stale, and this ADR corrects it instead of editing it

ADR-0022 §4 reads: *"11 of 20 ADRs are `Proposed`, and ADR-0016 is the only one
implemented."* Both halves are now false — **six** had shipped code at the moment
of this sweep, not one.

A decided ADR is immutable (`docs/adr/README.md:3`), so §4 stands uncorrected in
place and ADR-0022 remains readable as written, including the reasoning that
turns out to have been too narrow. ADR-0023 §7 already flagged the claim as stale
and recorded that an amendment was owed; this is that amendment. The
`Implemented:` convention §4 established is now moot as a *status* mechanism —
every ADR it would have flagged is `Accepted` — but it survives as the header
field the ten code-bearing ADRs use to say what their acceptance covers.

## What was explicitly not done

- **No ADR was renumbered, superseded or deleted**, and no body decision was
  rewritten except ADR-0019's two headings that framed its own text as a
  recommendation (`## Decision (proposed)`, `## Recommendation`) — a decided ADR
  cannot keep proposing itself.
- **ADR-0006 stays `Accepted` and unsuperseded-whole.** ADR-0008 supersedes its
  *addressing* only, where it assumed a server-local pane id is an address; its
  repo-scoped target tier stands. ADR-0009 supersedes ADR-0008's *transport*
  half only; ADR-0008's delivery model stands. Partial supersession is recorded
  in each ADR's `Issues:` header and is not a status.
- **No code changed.** This is a records sweep. Where a claim and the tree might
  disagree, the tree was read and the claim adjusted — never the reverse.

## Consequences

**Gained.** `Proposed` means one thing again: not decided. Eleven designs that
have been in production are recorded as decisions, each with its open question
named in its own header rather than discoverable only by diffing the tree. And
the `FEATURE-MATRIX` index stops implying a Tauri app and an FTS index exist.

**Given up.** Six ADRs moved status in one commit, so a future reader diffing
`main` sees eleven status lines change at once and no per-ADR history. The
`Implemented:` headers are the mitigation: each says what moved and why, at the
ADR, rather than in a commit message nobody reads later.

**Still open, and named above.** ADR-0017 §5 (whether to suppress the paste when
the target pane has flock's MCP), ADR-0004's committed policy layer (decisions
2, 4 and 5), ADR-0011 and ADR-0012 in their entirety, and ADR-0019's default.

Refs #544.
