# ADR 0021 — A model is a named capability the agent chooses from a declared set, and a tier is a property of the task rather than of the call

- Status: Accepted
- Date: 2026-10-03
- Issues: #430 (the registry and matrix). Supersedes nothing; **extends** ADR-0002
  (config layering) and is constrained by ADR-0005 (durable event log as the audit
  substrate), ADR-0010 decision 6 and ADR-0015 §5 (what may reach a third party, and
  who decides).
- Decision owner: operator. Design from a fresh-context investigation that verified
  the caller count against the tree rather than the issue's list, and from the
  operator's ruling that an agent **may** choose its model and tier — with the
  correction below, which the ruling did not state and which changes the design.

## Context

#430 wants one registry of models, bound to tasks by a tier matrix, so that features
needing a model call stop each shipping their own endpoint/key/budget config. The
investigation it produced found something more fundamental:

- **There are zero callers today.** No `[models]`/`[tasks]` key exists;
  `KNOWN_TOP_LEVEL_CONFIG_KEYS` (`src/config/io.rs:5`) has 23 entries, neither among
  them. #249 and #431 are both open, and the issue's other candidates are not issues.
- **The default build has no HTTP client.** `cargo tree -e normal --depth 1` is 20
  crates with none of them an HTTP client; `hyper` in `Cargo.lock` is a transitive of
  the optional `axum` behind `--features web`. All egress is
  `TracedCommand::new("curl", …)`, at six sites.
- So a registry today would be a config namespace with no reader — the shape ADR-0002's
  drift guard was written to kill.

That last point is why this ADR is not simply "implement #430". The question worth
deciding is what the *first* slice is, and the operator's ruling supplies the answer
that makes one exist.

**The ruling: an agent may choose its model and tier.** That is a real loosening of
ADR-0015 §5's posture ("whether an agent should ever [act on a third party] is left
open and is the operator's call, not a gap to be closed by exposing this"), and it is
accepted here deliberately.

The investigation had flagged the opposite as the safe default, on the grounds that
`src/spawn/allowlist.rs:115-131` puts a model endpoint on the `GH_TOKEN` side of the
blast-radius line rather than the `ANTHROPIC_API_KEY` side — "the child acting as the
operator toward a THIRD party, which is outside the blast radius the `Agent-Run:`
trailer can bound." The operator's position is that the distinction is not the one
that matters, and this ADR accepts that: **the boundary is the registry, not the
caller.** An agent choosing among models an operator has declared is making a
selection within an authorised set, not naming an endpoint.

What made this safe to accept is a fact already in the codebase rather than a new
posture: the MCP surface **already tells agents how to choose**, everywhere it offers
a choice. `schema_agent_start` publishes `"enum": AgentKind::supported()` with "A
closed set" (`src/mcp/tools.rs:307-334`), and `flock_agent_start` is a capability an
agent may invoke. The complaint that motivated the operator's ruling — that the MCP
never informs an agent how to choose — is therefore **already the design's pattern**,
and extending it to models is the consistent move rather than a concession in it.

## Decision

### 1. Models are declared once, by the operator; the agent names one

```toml
[models.haiku]
provider    = "anthropic"
model       = "claude-haiku-4-5-20251001"
api_key_env = "ANTHROPIC_API_KEY"   # the NAME of a variable, never a secret
timeout     = "5s"

[models.kev]
provider    = "openai-compatible"   # any /v1/chat/completions
endpoint    = "http://forge:8080/v1"
model       = "kev"
timeout     = "3s"
```

`api_key_env` is a name. The secret never enters config, and the key is read from the
**server's** environment, never from a caller's — the same rule as ADR-0014 §3, for
the same reason.

### 2. A task declares its tier; the agent may choose the model within it

```toml
[tasks.turn_summary]      # #249
tier   = "off"
budget = { calls_per_hour = 120 }
```

The tier is a property of the **task**, and this is the correction to the operator's
ruling. If an agent could choose its *tier*, it could promote a task the operator set
to `off` into one that egresses — and `off` is the operator's statement that nothing
leaves the host. So:

| tier | who sets it | meaning |
| --- | --- | --- |
| `off` | operator | the task does nothing and nothing leaves the host |
| `deterministic` | operator | pure code path only |
| `model` | operator | the deterministic path runs first; the model **enriches** its output |

**An agent chooses which declared model, within the tier the operator set.** That
keeps the operator's ruling intact in substance — the agent picks, from an authorised
set, with a budget it did not set — while preserving the one boundary whose violation
is invisible: an egress gate the caller can open.

### 3. `model` tier fails toward deterministic, and the failure is visible

On any error, timeout, budget exhaustion or missing key: fall back to the
deterministic output, **mark it `degraded`**, and log it.

"Visible" is the load-bearing word, and the investigation found that nothing needed
for it exists today:

- **A render surface.** `TranscriptDetail` (`src/agent_transcript.rs:364`) is
  `Reply | Collapsed | Full` — no provenance axis. #249's own pitfalls ask for
  summarized entries to be "visually distinguishable from verbatim ones", so a log
  flag does not satisfy that issue's own motivating requirement.
- **A durable event on the output's record**, not only on the call event, because
  ADR-0005's `EventKind::is_persisted` (`src/api/schema.rs:1800`) is an exhaustive
  match and a new kind must be added there deliberately.
- **A real counter.** `src/costs/mod.rs` is the cautionary tale: an entire accountant
  module carrying `#![allow(dead_code)]` because the event log holds no token counts,
  refusing to invent dollar figures. **A `calls_per_hour` budget with nothing to count
  against is that mistake wearing a config key.** So budgets arrive with the counter,
  or not at all.

### 4. The MCP surface publishes the set, so choosing is informed

An agent may only name what it can see, exactly as with `AgentKind::supported()`. The
registry is exposed as a closed set in the tool schema plus a read-only view:

```
flock_models_list   →   name · provider · endpoint · whether a key is present · tiers in use
flock_task_summary  →   task · tier · budget remaining · whether degraded
```

A caller that cannot enumerate the set cannot meaningfully choose within it. This is
the existing `tools/list` convention, and it is why the operator's "the MCP never
informs about it" complaint is answered by mechanism rather than by documentation.

### 5. Config placement, under ADR-0002

Both namespaces live in the global stack — `Default < FLOCK_* < config.toml <
config.local.config.toml`. Endpoint and key-name are host facts. ADR-0004 is
`Proposed` and scoped to "repo-domain keys only", so it is not the layer.

The env layer needs a **decided** posture, not a default:
`walk_scalar_leaves` (`src/config/env.rs:149`) recurses into tables and skips arrays,
so `[models.haiku].timeout` would synthesise `FLOCK_MODELS_HAIKU_TIMEOUT` for free,
while a map keyed by *model name* cannot. That is ADR-0002's 2026-09-08 amendment
situation with `[spawn.env]` exactly: put `models` in `BLOCKLIST_PREFIXES`
**as documentation of a decision**, or a reader will conclude the omission was an
oversight.

Unknown-key handling follows the existing convention: `deny_unknown_fields` is used
nowhere in `src/`, and `unknown_top_level_section_diagnostic` (`src/config/io.rs:575`)
emits "unknown config section; ignoring section". A declared-but-unknown key is a
**diagnostic, not a parse error** — including a task naming a model that is not
declared, which `flock_models_list` surfaces as a dangling reference rather than
failing config load.

### 6. The first slice is a caller, not a registry

With zero callers, the registry is a namespace with no reader. So the landing order
is fixed and not negotiable:

1. **A caller exists in-tree**, with written acceptance criteria. #249 is the
   candidate.
2. `[models.*]` as **data only** — no trait, no HTTP client, no budget, no matrix
   view, no agent-facing tool. It is a config namespace with exactly one reader.
3. `config check` learns dangling references, as diagnostics.
4. **Then** the call path, `ModelCalled` on the durable event, the `degraded` render
   surface, the counter, and `flock_models_list` / `flock_task_summary`.

The registry-as-data slice is defensible **because a caller exists**. Without one it
is step 2 with nothing reading it.

### 7. `provider` is a closed set, and `openai-compatible` is a named shape

`provider` is a closed enum with server-side request assembly — the same posture as
`AgentKind` (ADR-0020), for the same reason: a free string either names an endpoint or
reaches a shell, and a new provider is a reviewed variant rather than a config value
that changes where fleet content goes. `openai-compatible` means any
`/v1/chat/completions`; it is one variant with a configurable base URL, not a
per-vendor free string.

## Consequences

**Gained.** A single place answers "what does flock send where, and with whose key" —
which #430 identifies as currently unanswerable and which grows with every added
feature. An agent can choose its model from a set the operator published, with a
budget it did not set, and every attempt is on the durable log.

**Given up.** The operator's ruling as literally stated: an agent cannot raise its own
tier. That is the deliberate correction in §2, and it is the price of keeping `off`
meaning what it says.

**Deliberately not settled here.** The specific providers beyond
`anthropic`/`openai-compatible`, and the per-provider request shapes — each is a
variant with its own assembly, added when a caller needs it. And whether an agent may
ever *introduce* a task: it may not; tasks are operator-declared, so an agent cannot
create a task in order to obtain a tier.

## Consequences for #430

#430 is **not implemented by this ADR**. It is sequenced by §6, and its first slice
waits on a caller. The issue's `[models]`/`[tasks]` shape is adopted; the parts of it
that would have put egress control in the caller's hands are not.