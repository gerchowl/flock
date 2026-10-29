# ADR-0042: Typed configuration across files, env, scopes and clients

- Status: Accepted (owner decision 2026-10-29)
- Date: 2026-10-28
- Issues: [#2367](https://github.com/gerchowl/flock/issues/2367),
  [#2719](https://github.com/gerchowl/flock/issues/2719),
  [#2720](https://github.com/gerchowl/flock/issues/2720)
- Decision owner: repository owner, [acceptance on #2724][acceptance]
- Accepted: 2026-10-29; all recommended choices C1–C13.
- Scope of acceptance: the choices and limits, not their implementation. C1–C13
  retain the options previously labeled D1–D13 in this draft.
- Builds on: ADR-0041 (accepted config v2), ADR-0004 (project policy), ADR-0036
  §1a and §5–§6 (operator identity and wire), ADR-0028 §3a (durable
  persistence), ADR-0029 (typed CLI errors).
- Implemented: none of this extension. The existing inventory and safe
  projection are foundations, not this API or scope resolver.

## 1. Relationship to accepted decisions

ADR-0041 already accepts env above files, redacted reads, operator writes and
honest reload outcomes. This record does not reopen those decisions or replace
its implementation issues. The owner accepted the additional end state requested
by #2367 on 2026-10-29: generated schema/documentation, env representation for
every key, four explicit scopes, runtime overrides and audited writes. Until the
implementation and migration gates pass, existing scalar admission and env
blocklist behavior remain in force. Sections 2–4 extend ADR-0041 §3's design for
source restrictions; §§5–6 refine its write contract. ADR-0004's policy domains
and safety floors stay.

The owner's [2026-10-25 decision][no-secret-export] already closes ADR-0041's
raw-export question: **no caller may export raw secrets**, including a local
operator. That is a binding input, not a new recommendation. Accepted records
remain unchanged; this record records the later ruling.

## 2. Verified baseline and root causes

The source baseline is dev `b2519c2c4`. Relevant functions, rather than
incidental line offsets, identify the current paths:

| Path | Current behavior and limitation |
| --- | --- |
| `config::io::load_layered`, `deep_merge_tables` | Defaults, generic env, selected base file, adjacent overlay. Tables merge recursively; file arrays append. Missing/invalid overlay handling retains the existing base path. |
| `config::io::{config_path, config_overlay_path}` | `FLOCK_CONFIG_PATH` selects one base plus its adjacent overlay; otherwise the config directory uses XDG/HOME. There is no implemented four-scope file stack. |
| `config::env::{walk_scalar_leaves, env_override_table, is_blocklisted}` | Walks populated serialized defaults, admits scalars, omits absent optional leaves and collections, and deliberately blocks security/structured policy and several paths. |
| `config::inventory`, `inventory::projection` | Reviewed core/imported declarations, defaults, sensitivity and reload ownership already exist under #2384, with drift checks and mandatory safe projection. `core.json` is reviewed metadata, not a public schema API. |
| `app::config_io::{resolve_write_target, write_config_update}` | Regular base file or adjacent managed-file overlay; settings updates currently read/edit/directly write. This is not an atomic, audited, concurrent GUI transaction. |
| `App::{apply_config_from_disk, apply_live_config}` | Valid sections copy into live consumers; invalid sections retain running values. Aggregate Applied/Partial/Failed and some restart diagnostics exist; per-key consumer outcomes are not yet universal. |
| `web::{parse_args, parse_args_with_config}`, `main`, `client::run_client_with_mode` | Particular commands add flags and web CSV env exceptions; attaching clients also load local config. A server setting is not proof that a client applied it. |

The drift comes from independent declaration/default walks, source codecs and
consumer application paths. Adding one GUI serializer or more env exceptions
would preserve those causes. The schema must describe actual declarations and
consumer ownership, including absent values, not reverse-engineer defaults.

## 3. One typed schema and one resolver

**Decision C1 — accepted 2026-10-29:** Chosen: Rust declarations plus reviewed
field metadata form one coordinated schema registry, used by parsing,
validation, source mapping, redaction, reload classification and generated
reference docs. Cost: migrate metadata by section, audit imported/manual serde
representations and maintain generator and negative drift tests. A
derive/descriptor must not infer sensitivity from names. Rejected alternative:
keep the current Rust types plus independent GUI/docs catalogs. It was rejected
because additions can silently diverge across catalogs. “One schema” means Rust
types + a reviewed metadata registry + enforced drift checks, not that Rust type
declarations alone encode sensitivity or reload policy.

Every registered field has a stable canonical path, typed shape, optionality,
compiled default factory, constraints, aliases, admitted scopes, merge rule,
sensitivity, write authority, consumer and apply timing. Custom serializers need
an explicit adapter; unknown representations fail schema generation. New fields
without classification or reload ownership fail CI. Migrate the reviewed #2384
catalog into these descriptors instead of maintaining a second public catalog.
Existing compiled defaults/validators remain the implementations the descriptors
reference; metadata must not copy their numbers or implement parallel
validators.

Generate safe schema/response DTO TypeScript using the existing `ts-rs` feature
and wire generation/check workflow, and generate key/env/reload reference tables
from the same descriptors (#504). Do not derive a raw `Config` export. The safe
projection remains mandatory before serialization. TypeScript alone does not
validate untrusted input: the server validates against the Rust descriptors. For
this config DTO, integers outside JavaScript's exact range use an explicit
decimal-string variant, with matching TS types and numeric limits. This is not a
change to existing wire numeric fields. No silent rounding.

Schema version and capability negotiation distinguish unknown keys/operations
from invalid values. A schema version identifies shape, not a grant to mutate. A
`config.get` view contains resolved candidate, running value, source/provenance,
shadowing and apply outcome. Redacted, absent, unset-in-layer and explicit
optional null are distinct tagged states. Redaction also covers defaults, user
map keys, source identifiers and errors; neither values nor secret
lengths/hashes leak.

### Scope and precedence

**Decision C2 — accepted 2026-10-29:** Chosen: resolve an explicit context `{
node_id?, project_id?, agent_id? }`, normalized to an authenticated NodeId, not
a caller's cwd or a display label. Cost: identity plumbing and tests at every
consumer, rather than a global config object reused for all panes. Schema
admission restricts where each key makes sense; a node listener cannot be
configured at agent scope. Rejected alternative: infer scope from cwd, pane
selection or a display label. It was rejected because reconnects, renamed panes
and another client can silently change the target.

| Scope | Identity, ownership and storage |
| --- | --- |
| Global | Operator preferences in the selected base/overlay. Loaded locally on each node; “global” does not imply replication or authority over another node. |
| Node | Stable node identity, local node override file in the operator-owned config directory. No display-name or route-alias lookup. |
| Project | Stable registered repository identity, using ADR-0004's main-checkout/common-directory `.flk.toml` and `.flk.local.toml` policy sources. Only repo-domain keys are admitted. Repo content cannot supply commands or user preferences merely by naming a key. |
| Agent | Registered local agent identity with generation checks, operator-owned overrides outside the checkout. Harness kind alone is not an agent identity or authority. Launch-affecting values apply to future launches, not a running process's environment. |

### Home gateway and missing identity

The GUI talks to **one home server**, as directed by #2293 and specified in
heron's [#2727 draft](https://github.com/gerchowl/flock/pull/2727), reviewed at
`5c8662bf`. Config read/write requests for another NodeId go through that home
and the existing mesh routing foundation; the GUI holds no per-node sockets or
tokens. The destination owns its schema, config, revisions, audit and reload.
The home returns destination results without relabeling them as home-node state.

Effective remote authority is the intersection of the home token's current
scopes, the destination's explicit gateway grant and the local verb/key/scope
checks. A `read` grant is needed for remote reads and `operate` for remote
writes; home `operate` alone grants nothing at a destination. Forward attested
identity `(home_node_id, principal)` and stable mutation identity, never a
client bearer or reusable destination credential. Revalidate grants at the
destination; resume of an uncertain write uses the same mutation ID, not a fresh
automatic dispatch. No automatic fleet-wide replication or fan-out is implied by
global scope.

Normalize `node_id` before scope lookup: an omitted request NodeId binds to the
serving home's established identity, not whichever node a GUI row happens to
show. If the serving identity is absent, config.get/set refuse with the
registered `identity_unknown` (`ErrorCode::IdentityUnknown`), including for an
explicitly named remote target. Do not mint an identity, substitute a hostname,
interpret `None == None` as local, or fall back to a local file. Static local
config.schema reads without a destination can still return the classified schema
with `serving_node_id: null` and identity unavailable; they make no claim about
an effective scoped value.

This is a real startup state: `App.node_id` is optional.
`App::local_structured_message` in `src/app/api/structured_messages.rs` refuses
missing or mismatched node identity with `IdentityUnknown`; its
`verified_queue_custody` path also checks that option. Older paths in
`src/app/api/messages.rs` return “mesh node identity unavailable”; those strings
are evidence of the state, not new config error codes.

For an enabled routed read (and writes only after the gate below), a request
NodeId different from the serving home with no route returns registered
`server_unavailable` with the safe target NodeId and route-unavailable context.
An incompatible peer keeps `peer_incompatible`; absent grant keeps
`not_authorized`. Absence of a route, config-capable destination or identity
never redirects the operation onto the home. Until the gateway config verb is
implemented, return the version-gap/unsupported capability refusal rather than
ask the GUI to connect directly. The dedicated adapter slice below gates
activation.

Remote config writes specifically stay **unsupported** until this config ADR's
target-authorization design is implemented, in addition to the
#2293/#1380 gateway foundation. Until then, the home refuses remote config.set
with the existing `server_version_gap` unsupported-capability response (ADR-0029
exit 78), before routing, admission or mutation. Do not reuse an agent-only
refusal or treat a valid home operate token as sufficient. Missing serving
identity is still `identity_unknown`; no home config file may be a fallback.

**Decision C13 — destination config grants — accepted 2026-10-29:** Chosen:
require a config-specific grant binding the enrolled home, namespaced operator
principal, destination NodeId, read/set verbs, allowed configuration scopes/IDs,
file/runtime layers and an explicit canonical-key set. New schema keys are
denied until added to the grant. Cost: destination-side grant administration,
revocation fencing and schema-aware admission tests. Rejected alternative: reuse
an agent/workspace grant or trust home operate alone. It was rejected because
neither authorizes node-global config or a new scope/key. The design is
accepted; no such grant implementation exists today. Destination revocation
fences new mutations and cached results; an already admitted write can remain
outcome-unknown and is never undone by claiming revocation cancelled it. Remote
read grants must also explicitly admit config reads and still expose only the
safe projection. No remote config capability is advertised before these checks
exist.

Project authority and admission are checked before evaluating that layer. A
newly cloned repository cannot grant its own config write authority. Agent
overrides end at retirement unless an explicit persistent launch profile is
selected; they must never attach to a later generation with a reused display
name.

**Decision C7 — source layer before scope — accepted 2026-10-29:** Chosen: for
keys admitted at each scope, source layer wins first. Cost: a specific agent
file cannot bypass a broader deployment env/flag value; the GUI must explain the
shadowing. Rejected alternative: resolve each scope completely, then let the
most specific scope win. It was rejected because an agent file could override
deployment env, contradicting the intended 12-factor control at the target node.

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

Scope targeting within each source is explicit:

| Source | Target and tie-breaking |
| --- | --- |
| File | Global < node < project < agent among admitted scopes; local overlay wins within each scope. |
| Canonical/legacy env | Belongs to the serving process's node context, before any more-specific project/agent env entries. |
| `FLOCK_CFG_OVERRIDES` | Node entries merge into that same node env tier; overlap with a canonical/legacy node key is rejected even if equal. Project and agent entries follow in specificity order, all still below every explicit flag. |
| Explicit flags | Ordinary flags target the node. Typed override flags carry explicit registered project/agent IDs to target those scopes; no cwd inference. Duplicate same-key/same-context flags refuse. Within flags, node < project < agent. |
| Runtime | Each set/reset carries the normalized node plus explicit project/agent context and current agent generation when applicable. Node < project < agent within runtime; an omitted project/agent never edits whichever pane is focused. |

Only flags supplied to the process owning the configuration contribute to its
flag layer. Client/CLI invocation flags are not silently forwarded as startup
flags for a server; an explicit config.set remains a file/runtime mutation. Only
admitted keys can use those scopes. A GUI request aimed at a remote node is not
a way to change the home's env/flags: only the destination's own startup sources
participate. For example, node env beats agent file, a node flag beats agent
env, and a node runtime value beats an agent flag under C7/C8. Source admission
and immutable safety floors still apply before selecting a winner.

Canonical project discovery, symlink/common-directory checks and write-target
selection are shared loader/writer code. The request supplies registered IDs and
an enumerated layer, never an arbitrary path. Relative paths resolve against the
owning file directory; env/flag/runtime paths must be absolute or an explicit
schema-defined reference. Reload may not reinterpret a path against a new cwd.

### Complete 12-factor mapping

**Decision C3 — accepted 2026-10-29:** Chosen: every configurable value has a
generated, collision-free canonical env mapping, including optional values, maps
and arrays. Use `FLOCK_CFG__` followed by uppercase canonical path segments
separated by `__`; canonical segments use `[a-z][a-z0-9_]*`, with a
generation-time collision check. For example `ui.sound.enabled` maps to
`FLOCK_CFG__UI__SOUND__ENABLED`. Cost: new codecs, blocklist replacement review,
migration of legacy spellings and larger deployment environments. This changes
no process environment remotely. Rejected alternative: retain scalar-only env
admission. It was rejected as the complete design because absent options and
collections would still require files. It remains a valid intermediate release
under ADR-0041 until these adapters ship.

**Decision C9 — canonical env namespace — accepted 2026-10-29:** Chosen: use the
`FLOCK_CFG__` prefix and a `__` path separator described above. Forbid `__`
inside a canonical segment, and forbid leading/trailing underscores, in addition
to its alphabet constraint so splitting the separator round-trips unambiguously.
Cost: another namespace to document and migrate. Rejected alternative: extend
`FLOCK_<PATH>` without a distinct separator. It was rejected because segment
underscores can collide with path separators; silently changing a legacy
variable's meaning would be unsafe. Legacy names remain explicit generated
aliases.

Env scalars use the descriptor's strict codec: booleans `true`/`false`, bounded
numbers, enum spellings and strings. Collections and optional non-string values
use typed JSON; an optional string uses a tagged JSON optional value so literal
`"null"`, empty and absence stay distinct. Dynamic map entries and list items
are represented by their containing collection's env value, not by arbitrary
env-name fragments. The schema documents that mapping for every nested member. A
read-only computed value is not a configurable key and must be labeled as such.

Unset means inherit; empty is a real string only for types that admit it.
Clearing an optional or collection is explicit typed input, never a parse
fallback.

**Decision C11 — collection merge semantics — accepted 2026-10-29:** Chosen:
collection env/flag/runtime values replace that collection; existing file arrays
append. Cost: source-specific behavior must be visible in schema/provenance and
tested. Rejected alternative: append in every layer. It was rejected because an
env deployment cannot fully specify or clear an ordinary collection. Rejected
alternative: replace file arrays too. An implicit change was rejected because
existing overlays rely on append. A future file-format migration needs its own
acceptance under #2720. Until then, a collection editor must not pretend an
empty overlay clears the inherited list. Removal deletes that layer's entry and
reveals the lower value. For append-only file collections the editor must edit
an authorized owning layer or refuse a replacement it cannot represent; never
append duplicates to simulate replace.

**Decision C10 — scoped env document — accepted 2026-10-29:** Chosen:
environment values belong to the serving process's node context. Use an
operator-owned, typed `FLOCK_CFG_OVERRIDES` JSON document for explicit
node/project/agent entries with registered context IDs and canonical key/value
entries. Its position within the env tier is specified in the source-targeting
table above. Same-context duplicates and overlap with canonical/legacy node env
keys refuse, never depend on iteration order. Cost: a structured deployment
value plus context validation and tooling. Rejected alternative: encode
arbitrary context IDs into many env names. It was rejected because escaping, ID
churn and name collisions make it fragile. Rejected alternative: permit only
node env values. It was rejected as the complete design because scoped env
control would be unavailable. Child/model environment is not an input source for
the server. Unknown contexts produce a safe diagnostic without allocating an
agent or discovering a remote node. This document uses the same codecs and
authority gates, not another config language.

An env mapping grants no special authority. Source admission still enforces
bootstrap-only keys, credential-reference types, project policy domains and
safety floors. Replacing today's blocklist requires a reviewed input adapter for
each previously blocked key; no “unblock all” implementation. Empty legacy web
CSV allowlists stay unset as ADR-0041 requires. Canonical typed input that would
silently disable an existing identity/origin gate is refused by policy
validation. “Every key is representable” does not mean every value or scope is
valid.

Legacy `FLOCK_<PATH>` names and documented aliases remain during migration.
Canonical and legacy values for the same key must agree or yield a typed
conflict; there is no ambiguous winner. CLI flags carry explicit-presence
information so a parser default does not override a file/env value. The same
schema supports typed config override flags without requiring one bespoke CLI
option per key.

### Runtime overrides

**Decision C4 — accepted 2026-10-29:** Chosen: an explicit `config.set` target
distinguishes `file` from `runtime`; runtime overrides are process-lifetime
operator state above flags. Cost: another visible provenance layer and explicit
reset semantics. Rejected alternative: write every edit into a file. It was
rejected because temporary experiments would become persistent and silently
alter managed deployment intent.

**Decision C8 — runtime above flags — accepted 2026-10-29:** Chosen: an
authorized explicit runtime override wins over startup flags, for keys admitted
to live runtime edits. Cost: deployment flags are no longer absolute for mutable
preferences; show both sources and provide reset. Rejected alternative: keep
flags above runtime. It was rejected because the operator could submit a valid
temporary edit that can never take effect while the process runs. Bootstrap-only
keys and security floors remain non-overridable; this decision does not make
them mutable. Never silently persist a runtime toggle or claim it survives
restart. Runtime edits and resets are validated, audited and serialized with
file edits. Reload retains them; restart clears them. A reset reveals the lower
layer and returns its outcome. Bootstrap/restart-only settings may be saved to a
file, but cannot be claimed as running changes through runtime overrides.
Existing TUI toggles remain persistent until explicitly migrated; do not change
their lifetime by accident.

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
revision conflict, write/audit failure and apply rejection. Do not publish
ad-hoc string codes from this ADR. Clients branch on typed causes, not current
`Diagnostic::Cause` display labels. Errors never echo submitted content.

**Decision C5 — accepted 2026-10-29:** Chosen: credentials are references only.
A typed reference selects an allowlisted existing credential resolver, with no
raw-secret input or export route. Cost: migrate legacy arbitrary argv/env/URL
strings that may embed credentials; references cannot be safely inferred from
such strings. Rejected alternative: accept raw secrets on writes but redact
reads. It was rejected because new secret-bearing config files, logs and GUI
forms expand exposure. Existing literal legacy fields remain migration inputs
only; the no-raw-export owner ruling is binding whichever input migration is
chosen. Even references are redacted when their locator reveals sensitive
information. Neither `config.get` nor `config.schema` dereferences them.
Credential resolution happens only at the authorized consuming boundary, not
while previewing a setting. Raw bearer issuance and token files remain in their
existing private stores.

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
- MCP channel options have startup captures even when server message policy is
  live.
- `security.agent_account` and queue worker storage policy require their actual
  startup owners to restart; copying a section is not application evidence.

The GUI shows saved, resolved candidate and running values separately. Each
affected section reports `applied`, `restart_required` or `rejected(typed
cause)` using ADR-0041 §5 and #2064/#2066. Next-attach/next-launch boundaries
and mixed consumers are explicit details, never a false “applied to existing
sessions”. Restart-required differences remain visible on repeated reloads. A
file write masked by env/flags/runtime reports saved-but-shadowed and its
winning source.

## 5. One GUI/socket write transaction and audit trail

`config.schema` and `config.get` use read authorization on both socket and S7
WS. `config.set` requires the local operator identity or an authenticated
operator principal with `operate`; a GUI that also reads needs `read`
separately. Agent principals, `send` and `answer` do not gain config authority.
The [#2585 ruling](https://github.com/gerchowl/flock/issues/2585) admits
structured sends under the sending token's own principal; it does not grant
operator/config authority. Its post-MVP implementation remains separate; the
stricter current send behavior stays until that slice lands. Scopes are
rechecked under the existing revocation discipline immediately before mutation;
cached results require current authorization too. No transport-specific
backdoor.

**Decision C6 — accepted 2026-10-29:** Chosen: optimistic revision checks and a
single serialized writer per owning store, with a durable mutation audit. Cost:
journal recovery, conflict UX and failure injection tests. Rejected alternative:
last-writer-wins with best-effort logging. It was rejected because it loses
concurrent edits and can leave a durable mutation without a trustworthy audit
outcome. Rejected alternative: reject every external-edit race by claiming
filesystem compare-and-swap. That claim was rejected: uncooperative editors do
not obey the writer lock, as documented below.

A write supplies schema version, context, target layer, expected revision,
mutation UUID and typed set/remove entries. Revisions are opaque tokens, not
public hashes of secret-bearing documents. Stable principal + mutation UUID
correlates retries after disconnect. Bound frame, change count, journal
retention and concurrent work through named limits; defaults and ceilings must
be declared and fail-first tested in #2387, not left as an unlimited resource.

The writer performs these steps:

1. Authenticate and check source/scope/target permission. Resolve the
   managed-file target through `resolve_write_target`, including symlink and
   absent-base overlay behavior, without creating files on refusal. Reserve
   bounded admission.
2. Under the store's serialization lock, compare the expected revision, reread
   and prepare the complete candidate through the shared resolver/validators. A
   competing API or detected external edit yields a revision conflict.
   Uncooperative editors are not protected by the advisory lock; rechecking
   reduces but cannot remove their final rename race. Document this limit and
   offer the transactional editor instead of promising cross-process
   compare-and-swap.
3. Persist a private audit intent with mutation ID, stable actor ID, target IDs,
   canonical key IDs, revision and operation kinds. Never include values, raw
   user map keys, secret references or document hashes. Fsync the intent first.
4. For a file edit, write a private same-directory temporary file, fsync it,
   rename atomically and fsync the directory (ADR-0028 §3a). Preserve
   managed-base routing and comments; never follow a swapped symlink or use a
   supplied path. A failure after rename is outcome-unknown, not proof of
   rollback. Runtime mutation instead publishes the validated in-memory layer
   under the same lock.
5. Apply through existing reload owners and durably record the redacted outcome
   before replying. Saved-but-unapplied remains saved; no automatic rollback
   that overwrites a newer edit. If audit completion fails, expose the
   unresolved mutation ID, not an unaudited success. Initial audit failure
   prevents mutation.

Audit and config are not one filesystem transaction. Recovery reconciles pending
intents using private bounded recovery metadata tied to the intended revision;
that metadata must be sufficient to distinguish pre-write, post-rename and later
superseding edits. It is never part of a config export or public audit. An
ambiguous external edit remains explicitly unresolved; never replay an uncertain
mutation blindly. The API exposes status by mutation ID to an authorized
operator. Persisted unknown/newer records are skipped/logged/preserved raw per
ADR-0028 §3a, not strictly deserialized as one list. Audit remains metadata-only
and private: directories use mode `0700`, records mode `0600`, and no
client-supplied path chooses their location.

`config.set` never changes process env/flags, installs dependencies, starts a
program or restarts a server. Restart is a separate explicit operator action.
Reads remain usable when a writer is waiting; no lock wait holds a scarce I/O
permit. All waits and responses are bounded by shared documented operation
limits.

## 6. Migration and rollout

Implement incrementally; no flag day or “schema present therefore editable”
gate.

1. Reuse the completed #2384 inventory/projection, close remaining reader
   evidence gaps (#2431, #2499, #2539), then add generated descriptors/docs/TS.
   Unsupported inputs and apply timing stay visible in the schema until their
   slice lands.
2. Ship #2385/#2386 read/provenance APIs before writes or precedence change.
   Show current sources and a redacted old/new precedence comparison in the CLI
   as well as GUI. Never expose an unredacted debug/export mode.
3. Prepare #2387's scalar writer foundations, but activate no writer adapter
   until affected per-consumer reload outcomes are wired and tested. ADR-0041 §5
   and #2387 explicitly depend on #2064 and #2066. Preserve the TUI write target
   and migrate settings/config editing onto the shared writer only after this
   gate; a valid parse or file save cannot substitute for consumer application.
4. Release #2388's accepted env-above-file flip with explicit changed release
   notes, startup/reload/fallback tests and legacy CSV/alias policy. Audit
   inherited deployment env first. Do not silently flip precedence at connection
   time.
5. After their implementation prerequisites, add scope resolution,
   optional/collection codecs and reference migrations (#2719/#2720). Existing
   file arrays still append. Do not rewrite user files automatically; provide
   validated, redacted migration plans. Rollback follows the explicit
   compatibility-export plan below; do not claim that already-shipped binaries
   can be taught to reject a new format.
6. Add #2389's schema-driven GUI: explicit node/scope/layer, shadowing, revision
   conflicts, restart boundaries and unresolved write outcomes. Old servers
   refuse unsupported verbs with the existing version-gap contract; no GUI file
   fallback.

### Reversible migration and its limits

**Decision C12 — downgrade policy — accepted 2026-10-29:** Chosen: future
schema-aware binaries and managed launchers refuse unsupported config versions
before writing or starting consumers. Cost: a blocked launch/downgrade requires
an explicit legacy restoration instead of silently losing configuration.
Rejected alternative: warn and ignore unsupported keys. It was rejected for new
binaries because the visible configuration would not describe the running
behavior. Already-shipped binaries cannot be changed: permissive Config paths
silently discard unknown keys from the parsed model, and a later rewrite may
persist that loss. Strict sections may instead reject the new input. This
decision cannot prevent someone launching such a binary directly.

Ship a compatibility-tooling release before activating new scopes/reference
formats (#2720). While files still use the old format, the operator runs the
planned `flk config export --legacy --target-version <version>` command and
keeps an owner-controlled filesystem backup of the original legacy files plus
deployment env/flags. An old installed binary without the exporter needs that
tooling release first; no nonexistent old command is assumed. The export is
always redacted for every caller. It provides only safe representable legacy
settings and a loss manifest, with omitted/redacted/unsupported entries outside
the TOML value map. Never write a redaction sentinel as a working setting or
call a partial export a complete backup. File backups are operator-managed
recovery of preexisting files, not a new API path returning raw secrets. Never
log or attach them to a report.

Before enabling the new format, verify the manifest and a restart with the
target legacy version in an isolated instance. If required values cannot be
represented without redaction or a newer reference resolver, automatic rollback
is unavailable: retain the original private legacy files/credential setup or
supply those values out of band through the legacy operator workflow. Do not add
an unredacted export to make rollback appear automatic. Versioned migration must
not overwrite the only private legacy copy. Concurrent edits require a new
comparison/manifest.

To roll back: stop managed consumers, restore the original legacy files and
their env/flag deployment, remove new canonical/scoped override variables from
that legacy launch, then start the target version. This is an operator
procedure, not a restart performed by config.set. Explicitly lost on downgrade
are new node/ project/agent overrides, newly typed optional/collection/reference
representations, runtime overrides (already process-lifetime), candidate/running
provenance and new audit/recovery status. A target predating #2388 restores
env-below-file ordering; a newer legacy target retains its own advertised order.
The loss manifest must name the chosen version's precedence, not assume all
legacy releases agree. Old binaries may drop unknown keys if pointed at migrated
files. They must not overwrite new files or resolve pending new-format mutation
intents; retain that state separately for a later supported restart. No
downgrade promises an equivalent active config.

### Gateway rollout boundary

Remote config forwarding is part of the #2293/#1380 home-gateway design, tracked
with #2366/#2367 and #2719; it is not a direct-GUI-socket alternative. Implement
local config adapters first, then route the same typed verbs with explicit
destination config grants under C13 and bounded replies before enabling remote
config controls. Remote writes remain unsupported until C13 is implemented,
regardless of generic agent-command routing being available. Config addressing
and failure semantics are fixed above; mesh envelope changes belong to #1380's
separately approved VERSION exception, not to this docs PR. Cross-node secrets
synchronization is not a config export feature. This record changes no VERSION.

### Healthy-wait inventory shared with ADR-0045

Accepted ADR-0045 in [#2732](https://github.com/gerchowl/flock/pull/2732), at
`8a08fc94`, defines these registry entries for ADR-0042. GitHub healthy waits
have a four-hour default and a configurable 24-hour ceiling. Receipt and
agent-phase waits retain one-hour defaults and the same 24-hour ceiling. The
named two-hour no-change limit is separate from those maximum-wait settings.
All durations are integer milliseconds. G/N/A denotes global, node and
operator-owned agent contexts; project files cannot change supervision timing.

| Accepted registry key | Default / allowed range | Scope / consumer and reload |
| --- | --- | --- |
| `work.loops.wait.github_max_ms` | 14400000; 60000–86400000 | G/N/A; PR/CI/landing-batch healthy-wait maximum from original entry; reload may shorten, never extend that wait |
| `work.loops.wait.no_subject_change_ms` | 7200000; 60000–7200000 | G/N/A; all supported healthy waits, from last verified check/job/status change; polls and replay never renew it |
| `work.loops.wait.receipt_max_ms` | 3600000; 60000–86400000 | G/N/A; typed pending receipt subject with fresh producer coverage; original-entry maximum |
| `work.loops.wait.agent_phase_max_ms` | 3600000; 60000–86400000 | G/N/A; pending assigned work with fresh phase coverage; original-entry maximum |

`work.loops.wait.no_subject_change_ms` defaults to two hours, allows one minute
to two hours, and applies to all supported healthy waits from the last verified
check/job/status change. Polls and replay never renew it. Source staleness,
errors and terminal observations retain ADR-0045's earlier exits. Reload may
shorten an active wait, never extend its original maximum; re-entry, restart
and handoff cannot renew it. Record key types, units, constraints, provenance
and actual consumer outcomes through the shared descriptors. These entries are
accepted design, not keys already implemented in Config.

## 7. Ordered implementation slices and fail-first evidence

Each row is a separate PR with a **hard cap of 400 production lines**, tests
excluded. The smaller estimates are budgets, never permission to exceed 400.
Repeat metadata conversion by section/imported family; if a row cannot fit,
split it before implementation. Tests exercise a real loader, dispatch, consumer
or file boundary where that is the behavior being claimed; no sleeps as
synchronization.

| Order | Slice and tracking | Production budget | Fail-first evidence |
| --- | --- | --- | --- |
| 1 | Descriptor/generator foundation, then per-family metadata conversion (#504, #2385) | 250–350 per PR | Add an optional/imported field without metadata: coverage fails; generated docs/TS differ until regenerated. |
| 2 | Safe socket schema/get and provenance (#2385) | 250–350 | Real socket read distinguishes candidate/running/redacted/null and leaks no secret probe in replies or diagnostics. |
| 3 | S7 read adapters and capability negotiation (#2386) | 150–250 | Read token succeeds; missing read refuses; older schema/verb refuses without allocation or weaker fallback. |
| 4a | Typed scalar write preparation, revisions and managed target resolution (#2387) | 250–350 | Invalid/stale edit leaves files/runtime untouched; symlink base selects overlay without modifying its target. |
| 4b | Atomic persistence and external-edit detection (#2387, #657) | 250–350 | Inject failures at fsync/rename; previous or new complete TOML survives, and uncertainty is reported honestly. |
| 4c | Audit intent/completion and bounded recovery (#2387) | 250–350 | Crash after rename before completion leaves a queryable pending mutation; failed intent prevents the write; no secret in audit. |
| 4d | Per-consumer reload outcome adapters, one family per PR (#2043, #2064, #2066) — prerequisite to writer activation; ADR-0041 §5 | 200–350 | Real consumer retains startup-only value; repeated reload still reports restart; mixed consumers never all claim live apply. |
| 4e | Shared authorized writer adapters/TUI migration, gated on 4d (#2387 depends on #2064) | 250–350 | Actual operate/read/agent/revoked calls prove authority; concurrent clients get revision conflict, not lost updates. |
| 5 | Accepted precedence flip and legacy aliases (#2388) | 150–300 | File+env fixture changes winner at startup/reload and invalid-overlay fallback; empty CSV never disables a gate. |
| 6a | Destination config grant model and revocation, under accepted C13; before remote write activation (#2719, #2367) | 200–350 | Home operate/agent grant alone cannot set remote config; key/scope/layer restrictions and revocation fence a racing mutation. |
| 6b | Home-gateway config routing, after 6a and #2293/#1380 plus local adapters (#2719, #2367) | 200–350 | Actual home-to-owner dispatch preserves destination result; absent write capability is unsupported, no route is server_unavailable, missing identity is identity_unknown, and the home file never changes for a remote edit. |
| 7a | Stable context IDs and scope admission (#2719, #127) | 200–300 | Wrong node/generation refuses; project file cannot change user preferences or weaken branch floors. |
| 7b | Scoped file/source resolution (#2719) | 250–350 | Real files prove node env beats agent file, overlay ordering and common-directory project authority. |
| 7c | Runtime set/reset lifetime (#2719) | 200–300 | Real API reload retains runtime winner; reset reveals lower layer; new process has no transient override. |
| 8a | Optional/scalar canonical env codecs (#2720) | 200–300 | Absent optional defaults accept typed input; empty/null/unset differ; ambiguous aliases refuse. |
| 8b | Collection mapping/admission (#2720) | 250–350 | Real loader replaces env collection but preserves file append; duplicate scoped entries and policy-weakening empties refuse. |
| 8c | Secret-reference adapters, one consumer family per PR (#2720) | 200–350 | Literal secret input refuses; authorized consumer resolves reference; every read/export/audit stays redacted. |
| 8d | Safe legacy export/loss manifest and managed downgrade gate, before new format activation (#2720) | 200–350 | Export never returns secrets or redaction sentinels as values; loss manifest names unsupported scopes; supported launcher refuses a newer format without writes; real legacy loader restores the retained legacy config. |
| 9 | GUI read editor, then write/reload/conflict UX (#2389) | 250–350 per PR | Actual API-backed editor shows saved-but-shadowed/restart/unknown outcome; denied edit does not mutate. |

Row 8d is a pre-activation gate for the new formats prepared in 8a–8c;
preparatory code is not permission to migrate an operator's only config copy. No
rows above assert APIs exist today. The audit/recovery and GUI rows are blocked
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
last-writer-wins updates and blanket env unblocking were rejected: they retain
drift, leak risk or authority ambiguity. Implementing C1–C13 costs more review
and more small PRs; the alternative would make those costs recurring user
failures rather than explicit migration work.

[acceptance]:
  https://github.com/gerchowl/flock/pull/2724#issuecomment-6485265870

[no-secret-export]:
  https://github.com/gerchowl/flock/issues/2367#issuecomment-6395396399
