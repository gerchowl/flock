# ADR-0042: One typed configuration schema across files, env, scopes and operator clients

- Status: Proposed
- Date: 2026-10-28
- Issues: [#2367](https://github.com/gerchowl/flock/issues/2367), [#2719](https://github.com/gerchowl/flock/issues/2719), [#2720](https://github.com/gerchowl/flock/issues/2720)
- Decision owner: repository owner
- Builds on: ADR-0041 (accepted config v2), ADR-0004 (project policy), ADR-0036 §1a and §5–§6 (operator identity and wire), ADR-0028 §3a (durable persistence), ADR-0029 (typed CLI errors).
- Implemented: none of this extension. The existing inventory and safe projection are foundations, not this API or scope resolver.

## 1. Relationship to accepted decisions

ADR-0041 already accepts env above files, redacted reads, operator writes and
honest reload outcomes. This record does not reopen those decisions or replace
its implementation issues. It proposes the additional end state requested by
#2367: generated schema/documentation, env representation for every key, four
explicit scopes, runtime overrides and audited writes. These additions require
owner acceptance. Until then, ADR-0041's scalar admission and env blocklist remain
in force. On acceptance, §§2–4 below extend ADR-0041 §3's source restrictions;
§§5–6 refine its write contract. ADR-0004's policy domains and safety floors stay.

The owner's [2026-10-25 decision](https://github.com/gerchowl/flock/issues/2367#issuecomment-6395396399)
already closes ADR-0041's raw-export question: **no caller may export raw secrets**,
including a local operator. That is a binding input, not a new recommendation.
Accepted records remain unchanged; this record records the later ruling.

## 2. Verified baseline and root causes

The source baseline is dev `b2519c2c4`. Relevant functions, rather than incidental
line offsets, identify the current paths:

| Path | Current behavior and limitation |
| --- | --- |
| `config::io::load_layered`, `deep_merge_tables` | Defaults, generic env, selected base file, adjacent overlay. Tables merge recursively; file arrays append. Missing/invalid overlay handling retains the existing base path. |
| `config::io::{config_path, config_overlay_path}` | `FLOCK_CONFIG_PATH` selects one base plus its adjacent overlay; otherwise the config directory uses XDG/HOME. There is no implemented four-scope file stack. |
| `config::env::{walk_scalar_leaves, env_override_table, is_blocklisted}` | Walks populated serialized defaults, admits scalars, omits absent optional leaves and collections, and deliberately blocks security/structured policy and several paths. |
| `config::inventory`, `inventory::projection` | Reviewed core/imported declarations, defaults, sensitivity and reload ownership already exist under #2384, with drift checks and mandatory safe projection. `core.json` is reviewed metadata, not a public schema API. |
| `app::config_io::{resolve_write_target, write_config_update}` | Regular base file or adjacent managed-file overlay; settings updates currently read/edit/directly write. This is not an atomic, audited, concurrent GUI transaction. |
| `App::{apply_config_from_disk, apply_live_config}` | Valid sections copy into live consumers; invalid sections retain running values. Aggregate Applied/Partial/Failed and some restart diagnostics exist; per-key consumer outcomes are not yet universal. |
| `web::parse_web_args`, `main`, `client::run_client_with_mode` | Particular commands add flags and web CSV env exceptions; attaching clients also load local config. A server setting is not proof that a client applied it. |

The drift comes from independent declaration/default walks, source codecs and
consumer application paths. Adding one GUI serializer or more env exceptions
would preserve those causes. The schema must describe actual declarations and
consumer ownership, including absent values, not reverse-engineer defaults.

## 3. One typed schema and one resolver

**(Recommended) Owner decision D1:** Rust declarations plus reviewed field metadata
form one schema registry, used by parsing, validation, source mapping, redaction,
reload classification and generated reference docs. Cost: migrate metadata by
section, audit imported/manual serde representations and maintain generator and
negative drift tests. A derive/descriptor must not infer sensitivity from names.

Every registered field has a stable canonical path, typed shape, optionality,
compiled default factory, constraints, aliases, admitted scopes, merge rule,
sensitivity, write authority, consumer and apply timing. Custom serializers need
an explicit adapter; unknown representations fail schema generation. New fields
without classification or reload ownership fail CI. Migrate the reviewed #2384
catalog into these descriptors instead of maintaining a second public catalog.
Existing compiled defaults/validators remain the implementations the descriptors
reference; metadata must not copy their numbers or implement parallel validators.

Generate safe schema/response DTO TypeScript using the existing `ts-rs` feature
and wire generation/check workflow, and generate key/env/reload reference tables
from the same descriptors (#504). Do not derive a raw `Config` export. The safe
projection remains mandatory before serialization. TypeScript alone does not
validate untrusted input: the server validates against the Rust descriptors.
For this proposed config DTO, integers outside JavaScript's exact range use an
explicit decimal-string variant, with matching TS types and numeric limits. This
is not a change to existing wire numeric fields. No silent rounding.

Schema version and capability negotiation distinguish unknown keys/operations
from invalid values. A schema version identifies shape, not a grant to mutate.
A `config.get` view contains resolved candidate, running value, source/provenance,
shadowing and apply outcome. Redacted, absent, unset-in-layer and explicit optional
null are distinct tagged states. Redaction also covers defaults, user map keys,
source identifiers and errors; neither values nor secret lengths/hashes leak.

### Scope and precedence

**(Recommended) Owner decision D2:** resolve an explicit context
`{ node_id, project_id?, agent_id? }`, not a caller's cwd or a display label.
Cost: identity plumbing and tests at every consumer, rather than a global config
object reused for all panes. Schema admission restricts where each key makes
sense; a node listener cannot be configured at agent scope.

| Scope | Identity, ownership and storage |
| --- | --- |
| Global | Operator preferences in the selected base/overlay. Loaded locally on each node; “global” does not imply replication or authority over another node. |
| Node | Stable node identity, local node override file in the operator-owned config directory. No display-name or route-alias lookup. |
| Project | Stable registered repository identity, using ADR-0004's main-checkout/common-directory `.flk.toml` and `.flk.local.toml` policy sources. Only repo-domain keys are admitted. Repo content cannot supply commands or user preferences merely by naming a key. |
| Agent | Registered local agent identity with generation checks, operator-owned overrides outside the checkout. Harness kind alone is not an agent identity or authority. Launch-affecting values apply to future launches, not a running process's environment. |

No automatic fleet forwarding or global synchronization is introduced. The API
addresses the serving node; another node requires its own authenticated endpoint
and scope check. Project authority and admission are checked before evaluating
that layer. A newly cloned repository cannot grant its own config write authority.
Agent overrides end at retirement unless an explicit persistent launch profile is
selected; they must never attach to a later generation with a reused display name.

For keys admitted at each scope, source layer wins first:

```text
defaults < file < env < explicit flags < runtime
```

Within one source layer, specificity is global < node < project < agent, and a
scope's local file overlay wins over its base. Thus node env beats agent file;
agent specificity cannot smuggle a lower source above an env/flag deployment
choice. Not every scope exists for every key. Additive protected-branch floors
and authorization remain constraints outside this preference ordering: no layer
can disable them. This explicitly reconciles ADR-0004's file policy with the
owner's accepted env-above-file order.

Canonical project discovery, symlink/common-directory checks and write-target
selection are shared loader/writer code. The request supplies registered IDs and
an enumerated layer, never an arbitrary path. Relative paths resolve against the
owning file directory; env/flag/runtime paths must be absolute or an explicit
schema-defined reference. Reload may not reinterpret a path against a new cwd.

### Complete 12-factor mapping

**(Recommended) Owner decision D3:** every configurable value has a generated,
collision-free canonical env mapping, including optional values, maps and arrays.
Use `FLOCK_CFG__` followed by uppercase canonical path segments separated by `__`;
canonical segments use `[a-z][a-z0-9_]*`, with a generation-time collision check.
For example `ui.sound.enabled` maps to `FLOCK_CFG__UI__SOUND__ENABLED`.
Cost: new codecs, blocklist replacement review, migration of legacy spellings and
larger deployment environments. This changes no process environment remotely.

Env scalars use the descriptor's strict codec: booleans `true`/`false`, bounded
numbers, enum spellings and strings. Collections and optional non-string values
use typed JSON; an optional string uses a tagged JSON optional value so literal
`"null"`, empty and absence stay distinct. Dynamic map entries and list items are
represented by their containing collection's env value, not by arbitrary env-name
fragments. The schema documents that mapping for every nested member. A read-only
computed value is not a configurable key and must be labeled as such.

Unset means inherit; empty is a real string only for types that admit it. Clearing
an optional or collection is explicit typed input, never a parse fallback.
Collection env/flag/runtime values replace that collection. Existing file-array
append behavior stays until a separately advertised file-format migration; a
collection editor must not pretend an empty overlay clears the inherited list.
Removal deletes that layer's entry and reveals the lower value. For append-only
file collections the editor must edit an authorized owning layer or refuse a
replacement it cannot represent; never append duplicates to simulate replace.

Environment values belong to the serving process's node context. Project/agent
scoped env input may be supplied by an operator-owned, typed
`FLOCK_CFG_OVERRIDES` JSON document containing registered context IDs and canonical
key/value entries. Same-context duplicates or conflicts with a canonical env key
are rejected, not resolved by enumeration order. Child/model environment is not
an input source for the server. Unknown contexts produce a safe diagnostic without
allocating an agent or discovering a remote node. This document uses the same
codecs and authority gates, not another config language.

An env mapping grants no special authority. Source admission still enforces
bootstrap-only keys, credential-reference types, project policy domains and
safety floors. Replacing today's blocklist requires a reviewed input adapter for
each previously blocked key; no “unblock all” implementation. Empty legacy web
CSV allowlists stay unset as ADR-0041 requires. Canonical typed input that would
silently disable an existing identity/origin gate is refused by policy validation.
“Every key is representable” does not mean every value or scope is valid.

Legacy `FLOCK_<PATH>` names and documented aliases remain during migration.
Canonical and legacy values for the same key must agree or yield a typed conflict;
there is no ambiguous winner. CLI flags carry explicit-presence information so a
parser default does not override a file/env value. The same schema supports typed
config override flags without requiring one bespoke CLI option per key.

### Runtime overrides

**(Recommended) Owner decision D4:** an explicit `config.set` target distinguishes
`file` from `runtime`; runtime overrides are process-lifetime operator state above
flags. Cost: another visible provenance layer and explicit reset semantics. Never
silently persist a runtime toggle or claim it survives restart. Runtime edits and
resets are validated, audited and serialized with file edits. Reload retains them;
restart clears them. A reset reveals the lower layer and returns its outcome.
Bootstrap/restart-only settings may be saved to a file, but cannot be claimed as
running changes through runtime overrides. Existing TUI toggles remain persistent
until explicitly migrated; do not change their lifetime by accident.

## 4. Validation, secret references and apply timing

Validation is structural, then field/cross-field, then source/scope/authority,
then consumer preparation. API writes reject the whole requested edit before
persistence on an invalid key, stale context, wrong type or cross-field failure.
Manual file reload keeps today's section-level rejection, preserving independent
valid sections. Warnings/clamps remain explicitly modeled legacy behavior, not
silently converted to stricter rules by schema generation.

Failures are typed and versioned. Reuse suitable registered `api::error_code`
variants and `cli::exit` constants (ADR-0029); any new config-specific code is
registered with a retry class in its implementation slice. Required distinct
causes include unknown key/version, invalid type/value, scope/authority refusal,
revision conflict, write/audit failure and apply rejection. Do not publish ad-hoc
string codes from this Proposed ADR. Clients branch on typed causes, not current
`Diagnostic::Cause` display labels. Errors never echo submitted content.

**(Recommended) Owner decision D5:** credentials are references only. A typed
reference selects an allowlisted existing credential resolver, with no raw-secret
input or export route. Cost: migrate legacy arbitrary argv/env/URL strings that
may embed credentials; references cannot be safely inferred from such strings.
Even references are redacted when their locator reveals sensitive information.
Neither `config.get` nor `config.schema` dereferences them. Credential resolution
happens only at the authorized consuming boundary, not while previewing a setting.
Raw bearer issuance and token files remain in their existing private stores.

Until migration, existing potentially secret-bearing values stay redacted for
all callers. Classifying a string as a reference does not sanitize embedded
credentials. New typed API/env input to such fields must use an approved
reference-aware adapter or refuse the edit, never accept literals as a shortcut.
The secrets inventory covers map keys, command argv, child env, diagnostics,
provenance, audit and rejected input, reusing #2384's fail-closed projection.

Every key declares actual consumers and one of: live, next client attach, next
pane/agent launch, command restart, or server restart. Mixed consumers have
separate outcomes. Examples are evidence, not universal section labels:

- `title.enabled` has a local attaching-client veto; the server cannot assert
  that a remote client's local config changed.
- `spawn.env` governs subsequent starts/resumes; current child env is captured.
- MCP channel options have startup captures even when server message policy is live.
- `security.agent_account` and queue worker storage policy require their actual
  startup owners to restart; copying a section is not application evidence.

The GUI shows saved, resolved candidate and running values separately. Each
affected section reports `applied`, `restart_required` or `rejected(typed cause)`
using ADR-0041 §5 and #2064/#2066. Next-attach/next-launch boundaries and mixed
consumers are explicit details, never a false “applied to existing sessions”.
Restart-required differences remain visible on repeated reloads. A file write
masked by env/flags/runtime reports saved-but-shadowed and its winning source.

## 5. One GUI/socket write transaction and audit trail

`config.schema` and `config.get` use read authorization on both socket and S7 WS.
`config.set` requires the local operator identity or an authenticated operator
principal with `operate`; a GUI that also reads needs `read` separately. Agent
principals, `send` and `answer` do not gain config authority. Scopes are rechecked
under the existing revocation discipline immediately before mutation; cached
results require current authorization too. No transport-specific backdoor.

**(Recommended) Owner decision D6:** optimistic revision checks and a single
serialized writer per owning store, with a durable mutation audit. Cost: journal
recovery, conflict UX and failure injection tests. Last-writer-wins is simpler but
loses edits and cannot honestly distinguish an uncertain write from an applied one.

A write supplies schema version, context, target layer, expected revision,
mutation UUID and typed set/remove entries. Revisions are opaque tokens, not
public hashes of secret-bearing documents. Stable principal + mutation UUID
correlates retries after disconnect. Bound frame, change count, journal retention
and concurrent work through named limits; defaults and ceilings must be declared
and fail-first tested in #2387, not left as an unlimited resource.

The writer performs these steps:

1. Authenticate and check source/scope/target permission. Resolve the managed-file
   target through `resolve_write_target`, including symlink and absent-base overlay
   behavior, without creating files on refusal. Reserve bounded admission.
2. Under the store's serialization lock, compare the expected revision, reread
   and prepare the complete candidate through the shared resolver/validators.
   A competing API or detected external edit yields a revision conflict. Uncooperative
   editors are not protected by the advisory lock; rechecking reduces but cannot
   remove their final rename race. Document this limit and offer the transactional
   editor instead of promising cross-process compare-and-swap.
3. Persist a private audit intent with mutation ID, stable actor ID, target IDs,
   canonical key IDs, revision and operation kinds. Never include values, raw
   user map keys, secret references or document hashes. Fsync the intent first.
4. For a file edit, write a private same-directory temporary file, fsync it,
   rename atomically and fsync the directory (ADR-0028 §3a). Preserve managed-base
   routing and comments; never follow a swapped symlink or use a supplied path.
   A failure after rename is outcome-unknown, not proof of rollback. Runtime
   mutation instead publishes the validated in-memory layer under the same lock.
5. Apply through existing reload owners and durably record the redacted outcome
   before replying. Saved-but-unapplied remains saved; no automatic rollback that
   overwrites a newer edit. If audit completion fails, expose the unresolved
   mutation ID, not an unaudited success. Initial audit failure prevents mutation.

Audit and config are not one filesystem transaction. Recovery reconciles pending
intents using private bounded recovery metadata tied to the intended revision;
that metadata must be sufficient to distinguish pre-write, post-rename and later
superseding edits. It is never part of a config export or public audit. An ambiguous
external edit remains explicitly unresolved; never replay an uncertain mutation
blindly. The API exposes status by mutation ID to an authorized operator. Persisted
unknown/newer records are skipped/logged/preserved raw per ADR-0028 §3a, not
strictly deserialized as one list. Audit remains metadata-only and private: directories use mode `0700`, records
mode `0600`, and no client-supplied path chooses their location.

`config.set` never changes process env/flags, installs dependencies, starts a
program or restarts a server. Restart is a separate explicit operator action.
Reads remain usable when a writer is waiting; no lock wait holds a scarce I/O
permit. All waits and responses are bounded by shared documented operation limits.

## 6. Migration and rollout

Implement incrementally; no flag day or “schema present therefore editable” gate.

1. Reuse the completed #2384 inventory/projection, close remaining reader evidence
   gaps (#2431, #2499, #2539), then add generated descriptors/docs/TS. Unsupported
   inputs and apply timing stay visible in the schema until their slice lands.
2. Ship #2385/#2386 read/provenance APIs before writes or precedence change. Show
   current sources and a redacted old/new precedence comparison in the CLI as well
   as GUI. Never expose an unredacted debug/export mode.
3. Ship #2387's admitted scalar writer first, with atomic/audit/revision foundations
   split below. Preserve the existing TUI write target. Adopt that writer from
   settings and config editing rather than leaving competing direct writers.
4. Release #2388's accepted env-above-file flip with explicit changed release notes,
   startup/reload/fallback tests and legacy CSV/alias policy. Audit inherited
   deployment env first. Do not silently flip precedence at connection time.
5. After D2–D5 acceptance, add scope resolution, optional/collection codecs and
   reference migrations (#2719/#2720). Existing file arrays still append. Do not
   rewrite user files automatically; provide validated, redacted migration plans.
   An old binary cannot consume new scopes/references: declare minimum version,
   detect the format before launch, and refuse downgrade rather than losing values.
6. Add #2389's schema-driven GUI: explicit node/scope/layer, shadowing, revision
   conflicts, restart boundaries and unresolved write outcomes. Old servers refuse
   unsupported verbs with the existing version-gap contract; no GUI file fallback.

No mesh config RPC or fleet fan-out is included. Remote forwarding remains tracked
by #2366/#2367 and requires a separate accepted target-authorization design.
Cross-node secrets synchronization is not a configuration export feature. This
record changes no mesh VERSION or handshake field.

## 7. Ordered implementation slices and fail-first evidence

Each row is a separate PR of at most approximately 400 production lines, tests
excluded. The estimates are budgets, not permission to exceed the cap. Repeat
metadata conversion by section/imported family; if a row cannot fit, split it
before implementation. Tests exercise a real loader, dispatch, consumer or file
boundary where that is the behavior being claimed; no sleeps as synchronization.

| Order | Slice and tracking | Production budget | Fail-first evidence |
| --- | --- | --- | --- |
| 1 | Descriptor/generator foundation, then per-family metadata conversion (#504, #2385) | 250–350 per PR | Add an optional/imported field without metadata: coverage fails; generated docs/TS differ until regenerated. |
| 2 | Safe socket schema/get and provenance (#2385) | 250–350 | Real socket read distinguishes candidate/running/redacted/null and leaks no secret probe in replies or diagnostics. |
| 3 | S7 read adapters and capability negotiation (#2386) | 150–250 | Read token succeeds; missing read refuses; older schema/verb refuses without allocation or weaker fallback. |
| 4a | Typed scalar write preparation, revisions and managed target resolution (#2387) | 250–350 | Invalid/stale edit leaves files/runtime untouched; symlink base selects overlay without modifying its target. |
| 4b | Atomic persistence and external-edit detection (#2387, #657) | 250–350 | Inject failures at fsync/rename; previous or new complete TOML survives, and uncertainty is reported honestly. |
| 4c | Audit intent/completion and bounded recovery (#2387) | 250–350 | Crash after rename before completion leaves a queryable pending mutation; failed intent prevents the write; no secret in audit. |
| 4d | Shared authorized writer adapters/TUI migration (#2387) | 250–350 | Actual operate/read/agent/revoked calls prove authority; concurrent clients get revision conflict, not lost updates. |
| 5 | Per-consumer reload outcome adapters, one family per PR (#2043, #2064, #2066) | 200–350 | Real consumer retains startup-only value; repeated reload still reports restart; mixed consumers never all claim live apply. |
| 6 | Accepted precedence flip and legacy aliases (#2388) | 150–300 | File+env fixture changes winner at startup/reload and invalid-overlay fallback; empty CSV never disables a gate. |
| 7a | Stable context IDs and scope admission (#2719, #127) | 200–300 | Wrong node/generation refuses; project file cannot change user preferences or weaken branch floors. |
| 7b | Scoped file/source resolution (#2719) | 250–350 | Real files prove node env beats agent file, overlay ordering and common-directory project authority. |
| 7c | Runtime set/reset lifetime (#2719) | 200–300 | Real API reload retains runtime winner; reset reveals lower layer; new process has no transient override. |
| 8a | Optional/scalar canonical env codecs (#2720) | 200–300 | Absent optional defaults accept typed input; empty/null/unset differ; ambiguous aliases refuse. |
| 8b | Collection mapping/admission (#2720) | 250–350 | Real loader replaces env collection but preserves file append; duplicate scoped entries and policy-weakening empties refuse. |
| 8c | Secret-reference adapters, one consumer family per PR (#2720) | 200–350 | Literal secret input refuses; authorized consumer resolves reference; every read/export/audit stays redacted. |
| 9 | GUI read editor, then write/reload/conflict UX (#2389) | 250–350 per PR | Actual API-backed editor shows saved-but-shadowed/restart/unknown outcome; denied edit does not mutate. |

No rows above assert APIs exist today. The audit/recovery and GUI rows are blocked
on their corresponding writer/read foundations; no temporary parallel store or
unbounded writer is authorized to get an earlier checkbox green.

## 8. Pre-mortem and alternatives

| Failure | Prevention and observable evidence |
| --- | --- |
| Schema looks complete but omits optional/imported/map keys | Declaration coverage and fail-closed adapters; a mutation test adds an unclassified leaf and must fail. |
| Env flip silently changes security or routing | CLI provenance comparison, explicit migration release, empty-list policy and source-admission tests before rollout. |
| GUI says “saved” and user assumes “running” | Candidate/running/outcome separation, persistent restart notice and shadowed-layer explanation tested through actual consumers. |
| Read, audit or parser error leaks a credential | Mandatory projection for every caller, reference-only new input, distinctive probes across all output/error/audit sinks. |
| A project or agent scope widens authority | Stable IDs, generation checks, repo-domain admission, immutable safety floors and target-node authorization. |
| Two windows overwrite edits or a crash loses the audit | Opaque revisions, one bounded writer, durable intent/completion and crash tests; state the external-editor race rather than conceal it. |
| Runtime toggle unexpectedly becomes persistent or resets on reload | Explicit target/lifetime, tested reset/reload/restart transitions, provenance visible in every client. |
| Generator becomes a second config system | Reference compiled defaults/validators, migrate existing catalog, and test generated artifacts from the same descriptors. |

Keeping only scalar env keys is the smaller accepted ADR-0041 increment, not the
complete end state. A GUI-specific store, direct `Serialize<Config>` export,
last-writer-wins updates and blanket env unblocking are rejected recommendations:
they retain drift, leak risk or authority ambiguity. Implementing D1–D6 costs more
review and more small PRs; the alternative would make those costs recurring user
failures rather than explicit migration work.
