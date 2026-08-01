# ADR 0006 — Pane-to-pane message addressing: structured targets, not a fourth colon grammar

- Status: Accepted
- Date: 2026-08-01
- Issues: #175 (M1/M2, US-3); constrained by ADR-0001 (no cross-host
  push/broadcast) and ADR-0005 (durable event log as the audit substrate).
- Decision owner: operator (phase order waiver 2026-08-01); design per the
  Phase-3 planning round recorded on #175.

## Context

The epic sketches message addressing as `<pane>` → `<repo>:<pane>` →
`<server>:<repo>:<pane>`, widening with scope, and flags "check ADR-0003
before committing to `:`". ADR-0003 turned out to be about the `flock`→`flk`
binary rename — but the underlying worry is real, because `:` is already
load-bearing in three grammars:

- public pane ids: `<workspace>:p<n>` (`flock:p1`),
- fleet member labels: `<server>:<branch>` (`sage:main`),
- agent-source labels: `flock:claude`, matched by target resolution today.

A free-text `<repo>:<pane>` tier layered onto the same flat string that
`resolve_terminal_target` already parses would make `flock:claude` (label),
`flock:p1` (pane id), and `flock:main` (member label) collide with
`flock:<pane>` (repo tier) — exactly the mis-parse §8.1 tells us to test
against.

## Decision

- **The wire form is structured.** `msg.send` takes a tagged enum, not a
  string:

  ```json
  {"to": {"type": "pane", "pane": "flock:p1"}, ...}
  {"to": {"type": "repo_pane", "repo": "flock", "pane": "p1"}, ...}
  ```

  `pane` runs through the existing target grammar (terminal id, public pane
  id, unique agent name). `repo_pane` resolves `repo` against workspace
  worktree membership (`repo_name`, `dir:<name>` fallback) and then resolves
  `pane` within only those workspaces.

- **The CLI keeps ergonomics without wire ambiguity.** `flk msg send`
  accepts `--repo <name> --pane <p>` as the canonical form and a positional
  `<repo>:<pane>` shorthand that splits on the first `:` only when the left
  side matches a known repo name; otherwise the whole string is treated as a
  bare pane target. Ambiguity is an error, never a guess.

- **Cross-host addressing (`<server>:<repo>:<pane>`) is explicitly out** —
  it contradicts ADR-0001's no-push rule and waits for the persistent-slot
  successor (#75/#139/#152). Nothing in the wire shape forecloses adding a
  `server` field later.

- **Sender identity is inferred, never claimed** (P3): the server stamps
  `from_pane`/`from_repo` from API-peer process ancestry (the same seam the
  pane report verbs use). The stamp is routing and audit metadata; no code
  path branches on it for authorization.

## Bounded dedupe

Correlation-id dedupe survives restarts via the event-log seed, but both
windows are bounded: the in-memory seen-set keeps the newest 4096 ids, and
the boot seed only sees what ADR-0005 log rotation retained. A duplicate
older than both can be accepted again; reply routing for evicted ids returns
`message_not_found`. This is deliberate — unbounded dedupe is a disguised
database, and the epic's §9 rules that out.

## Consequences

- No fourth `:` grammar; §8.1's label-collision tests stay meaningful.
- Scripts get deterministic addressing; humans get the shorthand.
- The `repo` tier gives US-3 ("ask a peer in another repo") its natural
  spelling while same-repo asks keep the short form.
