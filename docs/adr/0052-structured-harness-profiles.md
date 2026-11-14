# ADR 0052 — Structured harness profiles separate operator authority, immutable assets and session storage

- Status: Accepted (2026-11-11)
- Date: 2026-11-08
- Issues: [#3066](https://github.com/gerchowl/flock/issues/3066)
- Decision owner: operator
- Implemented: slice 1, disabled admission guard. Production execution remains
  unavailable until slice 17.
- Amends: ADR-0036 §1 isolation and hot reload, and ADR-0032 §3
  structured-session context replenishment. Composes with ADR-0049 requests,
  operator bindings and the shared authority resolver. Existing accepted ADRs
  compose with this decision.

## Evidence and scope

The owner chose ADR first, then a Claude-only first slice. The consolidated
spike round on the issue and its two full runtime/security reviews agree that
an account profile cannot also be the authority boundary, asset installer and
conversation store. Both reviews inspected `0a2ad5df`; the code references
below were checked against this task's `origin/dev`, `1c3fd2d3b`.

- [`src/structured/launch.rs`](../../src/structured/launch.rs): `RunAs::admit`
  refuses `RunAs::Agent`; operator mode is an explicit exception. The persistent
  `ProfileConfigDir::for_operator` directory is keyed by profile name alone.
  `LaunchProfile` generates typed settings and isolation flags, with no explicit
  `--tools` list. `register_flock_mcp` replaces the generated MCP file with
  flock alone. `configure_environment` passes `CLAUDE_CODE_OAUTH_TOKEN`.
- [ADR-0036](0036-structured-agent-sessions.md) §1, E5/E6 and A.11 establish
  hook/config isolation, the danger of copying interactive refresh credentials,
  and the messaging-socket probe. **A.11 used `--tools Bash,Read`; production
  passes no `--tools`. It did not validate production tool availability.**
  Its slash-command count does not prove a selected skill bundle loads.
- [ADR-0049](0049-agent-types-workflows-and-one-authority-model.md) makes agent
  types requests, never grants, and operator bindings non-agent-editable.
  [ADR-0032](0032-compiled-agent-context.md) pins compiled context by content
  hash but assigns resume/compaction replenishment to SessionStart hooks.
- [`src/agent_resume.rs`](../../src/agent_resume.rs) discovers a unique native
  transcript across recorded profile stores and the default store. Its project
  directory encoding is harness-internal; migration must retain the discovered
  project key rather than reproduce an encoding algorithm.
- The fleet account tooling manages operator profiles and distributes a
  per-account setup-token through sops. Its profile roster, secret references
  and credential transport are inputs to an operator binding, never bundle
  contents. No owner account names, emails, machine identities or secret values
  belong in public manifests, tests or audit examples.

The [Claude CLI reference](https://code.claude.com/docs/en/cli-reference),
[skills documentation](https://code.claude.com/docs/en/skills) and
[subagent documentation](https://code.claude.com/docs/en/sub-agents) identify
explicit delivery mechanisms. Their interaction with the exact production
isolation flags is a discovery question, not established by those documents.
The CLI reference states that restricted mode removes command-executing built-ins
unless explicitly named in `--tools`; the `default` preset is insufficient.
No new harness probe was run for this ADR.

## Decision

### 1. Three independent records

| Record | Meaning and owner | May contain |
| --- | --- | --- |
| Operator binding | Operator-issued authority for a repository, node and agent/type identity, with revision and revocation | Account alias, credential reference and generation, authorized bundle digests, target scope, grants, ceilings and budget allocation |
| Agent type | Adopted declarative request under ADR-0049 | Requested account alias and exact bundle digest, alongside existing capability/model/preset requests |
| Bundle | Immutable content-addressed assets, explicitly reviewed and adopted by the operator | Skills, agent definitions, MCP declarations and pinned executables/dependencies, typed settings, immutable compiled-context inputs and manifest |

“Profile” is a presentation name for a resolved binding plus bundle, never a
new source of authority. An alias locates a binding; guessing it grants nothing.
A launch records type hash, binding revision, bundle digest, credential generation
and effective grants. Requests must be satisfied within the intersection of
operator grant, target scope, parent ceiling and run ceiling (ADR-0049).
Missing authority refuses before secret resolution, writes or process creation.
Editing repository configuration, a skill or a type cannot adopt a bundle,
select another credential or widen permission policy.

Bindings are durable server-owned operator state, separate from repository and
agent-editable configuration. Adoption, selection, revocation and reload use the
same authenticated-principal resolver at admission and mutation. A profile-select
grant names the target repository/node/agent and allowed binding/digest. An
agent may request an allowed selection through that grant; binding creation,
credential changes and grant widening remain operator-only. Existing operator-only
resume authority in ADR-0036 §1a remains in force.

Account-wide usage has a separate account-scoped read grant. Sharing an account,
knowing an alias, reading one's own session usage or holding a profile-select
grant never grants usage visibility into sibling agents. Redact unavailable
fields rather than leaking account labels, totals or other sessions in errors.
Preserve authenticated origin through every relay hop: pending and enrolled
relays never become the local operator, including through local dispatch or
forwarded profile APIs (the GHSA-6795-cfg2-rhhw confusion class).

### 2. Adoption and storage

Bundle adoption verifies a canonical manifest and every file's content hash,
including executable dependencies and compiled-context artifacts. The digest
covers relative paths, file kinds, executable modes and bytes; ordering is
canonical. Reject absolute paths, traversal, duplicate normalized paths, special
files and symlinks escaping the bundle root. Resolve and verify the staged tree
before publication without a symlink-swap window. Launch verifies the adopted
bytes again; tampering refuses, never triggers repair or a download.

Adoption is an operator-reviewed executable supply-chain operation: MCP servers
and skill scripts execute code, and skill prose redirects model behavior.
Floating package runners, network fetches and install scripts at launch are
forbidden. Any installation/build occurs in a separate reviewed adoption step
with pinned inputs. Settings are schema-checked; hooks, credential fields,
arbitrary environment injection and permission widening are rejected, including
in plugin and agent-definition metadata. The generated policy stays authoritative.
Keep the previous verified bundle and adoption receipt for rollback; a human
name moving to a new digest is never automatic authorization to run it.

Bundle lifecycle is `staged → verified → adopted`, with a terminal **revoked**
digest tombstone at the operator authority. Verification alone never authorizes
execution. Revocation increments a durable policy revision and records reason,
principal, operation ID and affected consumers. A revoked digest cannot be
re-adopted, selected, resumed, imported for execution or used for rollback; retain
its bytes only as non-executable quarantined evidence. A corrected bundle needs
a new digest and fresh review. Garbage collection never discards tombstones.

On revocation, fence admission first, then quarantine every consuming session:
hold its input/mail, withdraw tool/MCP authority, terminate the harness and all
supervised descendants, verify exit and publish a stopped/quarantined outcome.
Notify the operator and affected session viewers; audit requested, acknowledged,
expired, stopped and unreachable states separately. Retained transcripts are
untrusted recovery evidence, not permission to resume. An operator may inspect or
export them, but resuming historical malicious instructions requires a separately
reviewed recovery path into a fresh session, never an ordinary rollback.

**Fleet fence protocol (also applies to credentials and bindings).** One
operator-owned authority serializes policy revisions and issues authenticated
execution permits bound to node/runtime identity, session/lease generation,
binding revision, bundle digest and credential generation. Permit lifetime is
at most 60 seconds; renew at most every 15 seconds. A node accepts only current
policy, persists the highest revision and tombstones, and rechecks at every
start/resume/reload activation. Unknown state, clock uncertainty, restart or
inability to renew refuses new admissions. Use a monotonic local deadline
bounded by the signed expiry and measured request interval; reboot requires a
fresh permit rather than reconstructing elapsed time from wall-clock guesses.

Revocation commits immediately **at the authority** and pushes a fence to nodes;
it cannot promise instantaneous delivery through a partition. A supervisor
outside the harness's trust domain checks expiry, fences tools/input and kills
consumers on expiry even without the server connection. Allow at most 10 seconds
for forced process-tree termination after the deadline: an unreachable consumer
must be stopped within 70 seconds of its last permit issuance. Test that bound
with authority loss and an uncooperative child. If that independent watchdog or
OS containment cannot establish the bound on a platform, profile execution is
unsupported there. Revocation status remains pending/unreachable until an exit
acknowledgment or trusted OS fence proof arrives; expiry is not fabricated proof
of exit. Rejoining nodes synchronize tombstones before obtaining any new permit;
stale snapshots never resurrect revoked authority. Compromise of the supervisor
or recipient OS is outside this timing guarantee; provider credential revocation
and operator recovery remain necessary.

Use separate storage roots with separate lifetimes:

- Immutable `bundles/<digest>/` and content-addressed context artifacts are
  operator-owned and read-only to the harness. Retain referenced old versions.
- Writable `sessions/<session-id>/` holds that conversation's private
  `CLAUDE_CONFIG_DIR`, transcripts, native registries, plans and resume sidecars.
  Stable session identity survives process and lease generations. Two sessions
  using the same account never share this directory.
- Generated launch files belong to a process/lease generation and survive until
  supervision has reaped all relevant descendants. They include typed settings
  and the lease-bound MCP configuration; they are not bundle content.

Persist adoption/binding/import records with temp-file fsync, rename and
parent-directory fsync. In lists of persisted records, skip, log and retain raw
unknown or invalid entries per ADR-0028; refuse only launches depending on an
unusable record. Garbage collection must retain rollback and live references.

Mode 0700 separates users, not processes of one UID. Today `RunAs::Agent`
refuses, so admitted agents run with the operator UID and its filesystem access.
Per-session paths improve correctness but provide no hostile sibling isolation
in that mode. Distinct account trust domains require the privileged dedicated-user
launcher, OS-enforced access separation between domains, protected credential and
bundle roots, controlled tool-child environment, process ancestry/lease enforcement
and sandboxed filesystem access. A single shared agent UID alone is insufficient.
The operator-mode exception must remain explicit; no fallback from dedicated mode.
Operator mode can read sibling credential files through the retained home and UID
(`launch.rs` environment allowlist); API refusals cannot prevent that. It is
restricted to the existing explicitly trusted single-account baseline and synthetic
probe fixtures. New profile activation requires the numbered containment slices
below; untrusted bundles or multi-account use must never use operator mode.

### 3. Claude delivery is explicit and probed

Do not rely on files dropped into `CLAUDE_CONFIG_DIR` being discovered. Retain
stream-json I/O, stdio permission requests, generated `--settings`,
`--setting-sources local`, `--restricted` and `--strict-mcp-config`. Refuse an
unexpected messaging socket as ADR-0036 requires. Supply an explicit `--tools`
list derived from the approved launch contract; tool presence is not permission.
Do not widen tools silently to make a skill work.

Candidate delivery is a minimal hook-free plugin through `--plugin-dir` for
skills, explicit `--agents` for agent definitions, and one generated
`--mcp-config`. Merge adopted MCP declarations with the server-generated flock
entry instead of replacing either side. Reserve the flock name, reject collisions
and duplicate names, pin commands, and provide only approved environment fields.
The flock entry retains its owning server endpoint and current lease credential;
other MCP children never receive that credential. Initial mechanism probes allow
flock alone except for explicit synthetic delivery fixtures.
Do not use `--add-dir` as an implicit asset-loading escape hatch.

Every delivery claim is gated by a bundle qualification receipt for the exact
absolute binary, version and binary digest, adopted bundle digest, canonical
launch-template digest and platform/containment-policy digest. A minimum version
check is insufficient.
A binary, fixed asset or security-relevant launch-template change invalidates
the qualification receipt; dynamic context follows the separate validation below.
Fail closed if required assets are absent or unexpected
ambient hooks, skills, plugins, servers or tools appear; enumerate and explicitly
account for built-ins and managed policy. `system/init` names are evidence but
must be supplemented by actual tool calls and skill/agent marker observations.

**Receipt canonicalization, schema version 2.** Build a typed launch template,
then hash deterministic JSON with sorted object keys and ordered argv tokens.
Preserve argument presence, order, arity and every constant value; reject duplicate,
unknown or unclassified flags. Do not hash a shell command or redact arbitrary
substrings. The closed list of variable slots is:

| Variable slot | Required validation before substituting a typed slot marker |
| --- | --- |
| `--session-id` or the ID operand of `--resume` | Valid UUID, equal to the admitted session and lease. Start and resume are distinct contract variants, each probed. |
| Generated `--settings` / `--mcp-config` paths | Server-created regular files under this process generation's private root, no escaping symlinks; hash the parsed contents as specified below. |
| `CLAUDE_CONFIG_DIR`, synthetic/contained `HOME`, generation temp root | Server-selected private roots with verified ownership, mode and containment domain; relative layout and root role stay bound by the contract. |
| cwd and generated context-file paths | cwd must resolve to the bound repository/worktree within the authorized filesystem scope. Fixed bundle context bytes remain bound by the qualification digest; dynamic compiled artifacts use the typed context slot and per-launch validation record below. Only validated storage roots and permitted dynamic context vary. |
| Flock MCP endpoint and lease credential field | Endpoint must resolve to the owning node's attested server in its allowed socket root; credential must match this session/generation and be emitted only into flock's entry. Receipt stores the slot type, never secret bytes or their hash. |

No other value is variable. In generated JSON, hash all typed settings, permission
rules, tools, agent definitions, plugin layout, MCP names/commands/arguments and
non-secret environment values. Replace only the exact schema fields for the slots
above, after validation; bind executables/dependencies and bundle paths by content
digest plus relative path. The account token is absent from the description:
its transport and recipient policy are bound, its generation is checked against
current authority separately. Pin the environment allowlist and resolved executable
search paths; arbitrary PATH, external MCP endpoints or plugin paths cannot be
normalized away. Node-specific contracts get separate probe receipts where their
policy differs. Effective managed settings are included, not silently ignored.

A qualification probe and production session may differ in their validated
identifiers/roots and schema-validated dynamic context, but must have identical
canonical templates. Keep non-secret slot-validation results, exact context hashes
and contract version in each launch record. Failing-first tests prove a
fresh UUID/private path can reuse the receipt, while altered tool names, settings,
plugin bytes, MCP command/endpoint ownership, escaping roots, missing flags or
unrecognized slots cannot. Unknown template versions refuse. A qualification
receipt proves discovery only; current grants, revocation, budgets and containment
are checked independently on every activation.

Synthetic mechanism probes must:

1. Use synthetic home, cwd, settings, skills, agents and transcripts, with unique
   markers in both allowed and forbidden ambient locations. Start each probe
   session in a fresh config directory; never point at an operator profile.
2. Run the real production flags, including the explicit tools list, permission
   handshake and generated flock entry against an isolated server. Test each
   plugin/agents delivery mechanism and its combined invocation, MCP callability,
   forbidden ambient sources, and absence of a second messaging input door.
3. Resume after changing bytes under an unchanged skill name and after removing
   a skill/tool. Record delivered bytes, current executable tool availability and
   historical visibility separately. Test transcript plus sidecar import and two
   independent sessions sharing one account without shared writable files.
4. Test setup-token inheritance in Bash/tool children and MCP/subagent children
   using a synthetic canary first. Record presence only, never the value. A
   minimal authenticated run may use an explicitly supplied probe credential;
   never copy interactive auth/keychain files or print environment contents.
5. Clear inherited `FLOCK_*` variables (prefer `env -i` with an explicit PATH),
   use a short isolated XDG/socket root and assert the endpoint is the probe's.
   Bound every stage; fail on producer death, unexpected output or silence at
   the fail ceiling. Delete probe config on close after reaping children.

**Three separate evidence records avoid a bootstrap dependency.**

1. A **mechanism receipt** uses the synthetic fixtures above to establish the
   pinned adapter's flag, permission, isolation, discovery, import and resume
   behavior. It cannot qualify real assets or authorize production execution.
   A mechanism receipt eligible for qualification must come from a run under
   verified executable custody and the target containment policy; early fixture
   output before those prerequisites exist cannot serve as that receipt.
2. A **bundle qualification receipt** executes the actual reviewed, verified and
   operator-adopted bytes under the contained pre-activation mode below. It binds
   every skill, agent definition, MCP executable/dependency, fixed setting and
   fixed context artifact in that bundle. Adoption records content eligibility;
   qualification adds delivery evidence. Neither enables production by itself.
3. A **context validation record** binds the exact immutable compiled artifacts
   for each launch, resume or replenishment to their input versions, layer hashes,
   compiler digest and session/boundary. Dynamic task, goal and snapshot content
   is validated here, rather than forcing a new model probe for every status edit.

The canonical template's dynamic context slot pins the compiler/validator version,
layer placement, source-authority rules, schema, byte bounds and nonce framing.
It permits only compiler-derived layers/fields declared dynamic in that schema;
it cannot replace fixed role text, introduce tools/settings/hooks or substitute
another bundle. Bundle-provided fixed context files remain hashed in the bundle.
Qualification exercises actual fixed inputs and representative dynamic output
through this same compiler/validator and observes delivery. At runtime compile
from authorized sources, validate pinned repo/goal/workflow revisions and framing,
publish content-addressed files, and record their exact hashes before submission.
Subsequent snapshot changes produce new validation records, not a new bundle
qualification. Changing the compiler, slot schema, fixed role/assets or delivery
mechanism requires requalification. Workflow-pinned inputs cannot be reclassified
as dynamic to evade ADR-0049. Missing optional data follows ADR-0032; malformed or
out-of-scope context is held/refused, never silently accepted as a variable slot.
Tests must distinguish an allowed snapshot edit from an injected role/tool change.

**Contained qualification while production is disabled.** After content adoption,
an operator may preview and commit a `profile.qualify` operation for that exact
digest, template variant and node containment policy. It obtains a separate,
single-use `mode: qualification` permit; production permits still require the
enabled switch. This permit requires the mechanism receipt, immutable executable
custody, dedicated-user/OS containment, server hook policies, credential/budget
fences, explicit tools and context validation/replenishment to be ready. It
cannot be requested by an agent or relay and cannot be converted to a production
session, run or mailbox owner. It has a finite operator-reviewed probe budget,
the usual short renewable execution permits, a hard operation deadline and an
isolated supervision journal. Expiry, cancellation and revocation reap children
and release capacity only after exit proof.

Mount the adopted bytes read-only into a fresh sandbox and run the actual
production template and fixed settings/assets. Use a disposable worktree/home,
a fresh private config, fixture transcripts and an isolated flock server, never
the live server, operator profile, mailbox or fleet workspace. Supply an expressly
authorized qualification credential reference under the same transport/recipient
policy, charge its account reservation, and permit only the reviewed sandbox
filesystem and network destinations. Do not copy operator auth or secrets.
Real MCP executables are part of the test; an external service that cannot be
isolated without changing bound commands/settings remains unqualified. Substituting
an MCP stub or different fixed role can prove a mechanism but cannot certify that
bundle. A failed qualification leaves adopted data available for inspection but
ineligible for activation; it does not relax flags or auto-enable anything.

This provides the sequence **review/verify → adopt without execution → contained
qualification → per-launch context validation → explicit production enablement**.
Qualification does not require a prior receipt for itself, only the earlier
mechanism and safety prerequisites. It records actual observed assets, tool calls,
context delivery and all custody/contract digests; failures cannot publish a
passing receipt. The same path qualifies a newly adopted version before reload.

**Immutable executable custody.** The existing runtime's timed version recheck
(`src/structured/runtime/mod.rs`, `run`) acknowledges a check/exec replacement
window; another hash immediately before `Command::new` does not close it. Before
qualification or production spawn, the privileged launcher must acquire the
mechanism-verified executable by verified handle and execute that same object where the
platform supports it, or execute a concrete path in a verified immutable store
whose file and every path component are protected against replacement for the
entire verification-to-exec interval. Hold custody through descendant exit;
include executable dependencies/interpreter in the protected closure. Resolve
mutable install aliases only when acquiring custody, never again at spawn, and
record the actual object digest against the receipt. Store updates publish a new
object rather than modifying a held one; garbage collection retains held objects.
A path/version check or agent-owned mode bits alone are insufficient. A platform
without a demonstrated handle-exec or protected-store guarantee refuses launch.
Failing-first tests replace both a file and parent symlink between verification
and spawn: the verified object executes or launch refuses; the substitute never
runs. This is an implementation gate, not a claim about today's runtime.

This record specifies the gate; it does not claim it passed. A failed candidate
requires an evidence-backed delivery revision before deployment.

### 4. Credential lifecycle and account admission

Secret references stay outside bundles and type definitions. Resolve one approved
Claude setup-token reference only after authority and budget admission. Read
through the existing private-file checks, redact/zeroize in memory and never put
secret bytes on argv, in events, receipts or persisted launch metadata. Preserve
the delivery seam so token transport can change after the inheritance probe.

The operator declares authorized recipient nodes/runtime identities, credential
generation, expiry and account-wide concurrency budget. Sops protects distribution,
not a compromised recipient: every recipient of shared token bytes shares that
account's compromise/revocation boundary. Never clone interactive access/refresh
credentials (ADR-0036 E6). Rotation prepares a new generation for approved
recipients, verifies availability, switches new admissions, drains or explicitly
restarts old-generation sessions and revokes the old generation. Revocation fences
new starts/resumes at the authority immediately and stops affected running
sessions through §2's acknowledged-or-expiring permit fence. Unreachable nodes
have the same explicit 70-second bound, not an instantaneous guarantee.
Provider-side revocation is also required because a leaked token can be used
outside flock. Rollback may reuse an old bundle but never a revoked digest,
credential or grant; it requires fresh permits under current policy.

Use account-wide reservations, including starts, forks and resumes across nodes,
with idempotent acquisition, lease generation and release after verified exit.
For the initial four-node fleet use the same authoritative owner for policy and
budget allocation; a partition never elects a replacement allocator. Never infer
account capacity from node-local counts or permit expiry. A missing exit proof
keeps the slot charged until recovery or trusted OS fencing proves the writer
cannot run. Refuse admission when budget authority is unavailable.

**Reload at the cap uses a replacement ticket, not a second slot.** While the old
writer runs, the allocator records an idempotent successor ticket on its existing
account reservation, bound to the same session and expected old/new generations.
The ticket cannot execute. After verified old process-tree exit, atomically mark
the old generation spent and transfer that one slot to the successor, rechecking
policy/permit before spawn. Old and new writers can never hold executable rights
together. Refused preflight leaves the old writer/slot intact. A failed successor
retains the ticket/slot for retry or an authorized rollback only after its own
exit is verified; explicit cancellation releases it. Account changes require a
separate reservation in the new account before shutdown and cannot borrow the
old account's slot. Partition or uncertain exit leaves the transition pending,
with no successor activation or capacity reuse. Allocation authority
and aggregate usage-read authority remain distinct. Tool-child inheritance is
unverified today; if the token reaches children, treat them as credential
recipients and block rollout across trust domains until transport or OS controls
contain that exposure.

### 5. Hook inventory and disposition

Inventory taken 2026-11-08 from the owner's claude-config checkout at
`0f8d7d499c5c9b29b093957a8dd0814989fe28dc` (README and `hooks/`), the
fleet-generated shared settings, and the local effective settings' hook entries.
Only event/function identities are recorded here; deployment paths and identities
are deliberately omitted. Optional shared hooks are listed even when inactive.

| Existing hook/function | Structured-session disposition |
| --- | --- |
| `goal-s1` Stop: continuation, acceptance checks/review, question classification, parked waits and compaction handoff | Replace with server-observed `result`, durable goal/work supervision and bounded check execution (ADR-0030/0045). Acceptance evidence and escalation ceilings are server decisions; model markers cannot authorize completion or widening. Drop the detached PTY poller/input injector because the server owns the queue and wakes. |
| `goal-s1` SessionStart: goal re-injection; UserPromptSubmit: armed banner | Server replenishes compiled context at resume/compaction; GUI/server goal projection replaces the advisory banner. Drop duplicate banner injection. |
| `flock-agent-state` SessionStart/UserPromptSubmit/Stop: identity, profile recall, status and mailbox delivery | Use lease/process attestation, typed events, session metadata and the server input queue. Admission, attribution and durable audit are server enforcement; drop hook reports as competing authorities. |
| `scrub-gate` PreToolUse Bash: deny uncertain or out-of-tree deletion, direct safe cleanup | Preserve deny policy in server permission evaluation and OS filesystem controls, including realpath/symlink escape checks. Unknown analysis refuses; a shell parser is not a sandbox. Keep reviewed safe-cleanup tooling only if explicitly adopted. Never drop this protection merely because hooks are disabled. |
| Nix-managed-file guard PreToolUse Edit/Write/NotebookEdit | Server permission policy rejects writes through managed/store paths; OS read-only protection remains. Drop the hook's source-file search hints as advisory implementation detail. |
| `watch-nudge` PreToolUse Bash: waiting/polling advice | Drop the fail-open prose nudge; adopted role guidance plus server waits/subscriptions already owns this behavior. It grants no authority. |
| Optional `watcher-s1` PostToolUse Bash: masked-pipeline judge | Drop the advisory hook classifier; preserve actual child exit evidence and bounded server-run checks. A pipeline's printed success must not count as verified acceptance evidence. |

No hook is imported implicitly from user/project settings, plugins or agent
frontmatter. The adoption manifest records the hook inventory revision and the
server-policy mapping. Permission decisions use ADR-0049's shared resolver;
escalation ceilings, authenticated attribution, admission and audit cannot depend
on hook success. Probe that every action requiring these deny policies reaches
server evaluation, including harness auto-approved paths. If it does not, enforce
the restriction through OS controls or refuse that tool/preset; disabling hooks
must not silently remove a protection. Before migrating a node, re-inventory its
effective settings; unclassified hooks block that node's cutover until explicitly mapped or dropped
with a reason. This inventory is not a claim that every fleet node is identical.

### 6. Reload and context replenishment

**Workflow pins precede reload authority.** For a workflow-owned session, compare
the complete proposed type/bundle/role/context closure and pinned repo inputs to
the run's stored closure before reserving a replacement or stopping its writer.
ADR-0049 W2 adoption/pinning rule 5 forbids changing that closure in flight.
Operator approval, bundle adoption or a newer discovery receipt cannot override
it. Refuse any closure-changing reload or rollback with the run unchanged; require
cancellation, adoption of the new workflow closure and a new run, with landed
effects recognized by reconciliation. A same-closure restart or credential rotation
may proceed only within current and pinned grants/policy, with the required fresh
permits and receipts. Revocation still stops the old run; it does not authorize
migration to replacement content. Standalone sessions may adopt a new authorized
type/bundle through the transition below, recording both hashes. Fleet rollout
previews mark incompatible workflow consumers as requiring cancel/new-run rather
than scheduling in-place reload. Test a changed role/bundle pin is refused before
shutdown, and that a same-closure restart preserves the run's `def_hash`.

Authorize the proposed digest and credential generation **before shutting down**
a healthy process. Verify bytes and acquire §4's non-executable replacement
ticket on the existing slot (or a separate new-account reservation), then wait
for idle (no started input or open control request), close stdin, verify writer
and descendants exited, transfer reservation and lease, and resume the same
session. Revocation bypasses the idle wait and uses §2's forced-stop deadline.
Recheck binding revision, grants,
revocation and digest at activation; authorization may have changed while waiting.
An unauthorized reload leaves the old process untouched. Activation failure keeps
rollback records and does not publish a replacement as active.

Compare manifest content hashes as well as `system/init` names, and record the
old/new digest, tool inventory and policy revision. A same-name skill edit must
produce a diff. Send an ordered reload notice after successful activation.
Resume retains historical skill text and tool schemas; a removal notice does not
erase them. Enforce removed capability denial outside the model, and require a
fresh session when historical instructions themselves are unacceptable. ADR-0036
E12/E14 cache measurements for particular MCP changes do not predict arbitrary
skill replacement costs; compaction remains a separate deliberate action.

For structured sessions the server replaces ADR-0032's SessionStart dependency:
keep stable layers pinned to immutable artifacts and replenish the appropriate
layers 2–5 or 3–5 from server state after resume/compaction, before subsequent
work input. Record context hashes and the boundary/generation to avoid duplicate
injection on recovery. Frame text from other producers with per-item random nonce
delimiters and validated identifiers; metadata belongs in headers, not bodies.
Missing optional context follows ADR-0032's explicit missing-source behavior;
missing authority never becomes optional context.

### 7. Migration transaction (`flk agent migrate`, piece 1)

Operator-authorized migration changes execution ownership, preserving conversation
identity. Freeze new PTY input, stop the actual harness writer and verify its
process identity and exit; a closed pane or signalled shell is insufficient.
Discover the unique source transcript using recorded store evidence, retaining
session ID, cwd and the **discovered project key**. Ambiguous/missing evidence
refuses without guessing or choosing the first match.

Stage the native transcript and required sidecars in the session store, validating
paths and IDs, then atomically publish a complete import with a durable migration
journal. No two processes may write one session. Acquire the new lease and activate
structured resume under the approved binding/digest. Commit mailbox ownership only
after successful activation; preserve pending messages and delivery IDs so recovery
cannot silently lose or double-submit them. Retain source data, sidecars, old
ownership and journal for rollback. Crash recovery reconciles each journal phase;
rollback restores one writer/owner only after the replacement is verified dead.
Never remove source evidence as part of an unverified cutover.

### 8. Codex is a later adapter

Codex gets a private `CODEX_HOME`, provider-specific auth and refresh ownership,
generated configuration and app-server `thread/resume`. Claude setup-token or
keychain conclusions do not establish Codex's credential contract. Discover
effective skills through `skills/list` (including refresh with `forceReload`)
and reject unexpected repository, home, administrator or bundled sources;
`CODEX_HOME` alone is not a discovery boundary. Pin the binary and probe delivery,
resume and token/refresh concurrency independently. Migration requires its own
native rollout importer. No Codex support is implied by the Claude slices.

### 9. Operator command and recovery contract

These are target CLI/API contracts. Slice 1 exposes only `profile.enable`, which
refuses execution while the build is incomplete; the transactions below are not
available yet. One operator controls the authority; ordinary relay or harness
principals cannot invoke these mutations. Authenticated operator transport remains
required on remote nodes.

| Operation | CLI | API transaction |
| --- | --- | --- |
| Review adoption | `flk profile plan adopt --source <staged-dir>` | `profile.plan { action: adopt, source }` verifies pinned inputs and produces an immutable review plan |
| Qualify adopted bytes | `flk profile plan qualify --bundle <digest> --nodes <node-ids> --credential-ref <probe-ref> --deadline <duration>` then `flk profile qualify --plan <id> --expect <digest>` | `profile.qualify` issues only the contained qualification permit and records observed evidence; it cannot enable production |
| Adopt reviewed bytes | `flk profile adopt --plan <plan-id> --expect <plan-digest>` | `profile.adopt` commits precisely the reviewed digest |
| Bind target authority | `flk profile plan bind --repo <repo-id> --nodes <node-ids> --agents <agent-ids> --account <alias> --bundle <digest> --credential-ref <ref> --generation <n> --grants <grant-ids> --budget <allocation-id> --preset <ceiling>` then `flk profile bind --plan <id> --expect <digest>` | `profile.plan` / `profile.bind` records explicit targets, grants, ceilings and budget; never an inferred fleet-wide wildcard |
| Rotate credential | `flk profile plan rotate --account <alias> --generation <n> --credential-ref <ref>` then `flk profile rotate --plan <id> --expect <digest>` | `profile.rotate` stages recipients and coordinates canary, drain, switch and old-generation revocation |
| Roll back content | `flk profile plan rollback --binding <id> --bundle <previous-digest>` then `flk profile rollback --plan <id> --expect <digest>` | `profile.rollback` is a fresh authorized reload to an eligible retained digest, not restoration of old grants or credentials |
| Emergency revoke | `flk profile revoke --bundle <digest> --reason <text>` or `--account <alias> --generation <n>` or `--binding <id>` | `profile.revoke` durably commits the tombstone/fence before dispatching stops; does not wait for a preview, reachable consumers or provider API availability |
| Enable reviewed execution | `flk profile plan enable --nodes <node-ids>` then `flk profile enable --plan <id> --expect <digest>` | `profile.enable` requires the complete slice-17 readiness set and current permits; missing prerequisites refuse even for the operator |
| Set account capacity | `flk profile plan budget --account <alias> --cap <n>` then `flk profile budget --plan <id> --expect <digest>` | `profile.budget` records the account cap and named allocations; a reduction below charged slots drains before new admission |
| Fleet status | `flk profile status --fleet [--operation <id>] --format json` | `profile.status` shows revisions, digest/generation, permit expiry, slot/ticket, per-node acknowledgment and recovery phase |
| Recover partial operation | `flk profile recover <operation-id> --retry` or `--cancel` | `profile.recover` reconciles observed writer/lease state before retrying or cancelling; it never clears a revocation |

Plan output shows the exact content/policy diff, hook disposition, discovery and
containment receipts, recipient set, affected sessions, capacity, predecessor
revision and rollback eligibility, including workflow closure conflicts requiring
a new run. It contains secret-reference identifiers only,
never token bytes or secret file contents. Plans expire after five minutes;
commit verifies the plan digest, caller, staged bytes and expected policy revision
with compare-and-swap. Changed or expired plans require a new preview. Every
mutation returns a durable operation ID; repeats with that ID are idempotent,
and all authority is rechecked at each effect fence. Reading the fleet-wide
account fields requires the separate account-usage/read grant; unauthorized
status projections omit those fields without disclosing sibling identities.

For roughly ten agents on four nodes, adoption/preflight fans out concurrently
without stopping agents; activation runs one canary, then one replacement at a
time. The status command is the single progress view: prepared, draining,
activating, verified, failed, unreachable or quarantined per consumer. Stop
expansion on any failure. A distributed rollout is not falsely reported atomic:
retain the verified predecessor on unaffected nodes, fence failed consumers, and
show precisely which revision each node runs. Credential rotation does not revoke
the predecessor until all old recipients acknowledge replacement or are fenced;
excluded old recipients must be fenced too. An operator may instead emergency-revoke
it with the explicit outage consequence.

After a timeout or restart, `status --operation` reads the durable journal and
`recover --retry` resumes only unfinished effects after process/lease inspection.
Cancellation stops expansion; rollback of already activated consumers is an
explicit reviewed operation subject to current revocations and replacement
rules. An unreachable node remains pending and consumes its reserved capacity;
recovery waits for exit/fence proof. Emergency revocation never rolls back on a
partial failure. Status separately reports provider-side credential revocation
pending/confirmed so a local tombstone is not mistaken for provider invalidation.

## Ordered implementation slices

Every numbered row is a separate PR with **at most 400 production lines**,
excluding tests and doc comments, and the stated failing-first evidence against
the prior build. Dependencies listed below are mandatory; later work cannot
activate early.
If a row still needs more than 400 production lines, split it into further
inactive PRs before briefing implementation, preserving all dependency gates.

**Inactive until proven safe.** Slice 1 adds an operator-only
`structured_profiles.enabled` binding-state switch, default false, and a common
admission guard. All new profile spawn/resume/reload, migration, GUI and adapter
paths must use it before secret access, process creation or mailbox transfer.
It cannot be enabled from agent-editable files. Slices 1–16 cannot enable
production. Preparation APIs verify/store reviewed data without executing it;
synthetic mechanism fixtures never use live operator profiles or the live server.
Authenticated mechanism runs use an explicitly authorized probe credential.
Slice 16 adds only
§3's operator-authorized contained qualification mode, after adoption and all
safety prerequisites. It can execute actual adopted assets in its isolated probe
domain while production remains disabled, and can never publish a production
session or transfer mailbox ownership. Its separate admission test proves this.

Slice 1 holds the switch in server memory only; slice 2 persists it with the binding records.

Existing trusted single-account launch behavior stays on its existing path;
profile operations never fall back to it on denial. Slice 17 is the only initial
production activation path: its readiness predicate requires all slices 1–16,
including explicit tools, delivery, context validation/replenishment, immutable
executable custody and qualification of the exact bundle/template. Check that
predicate both on enable and every admission, even with the switch true.
Disabling execution applies §2's runtime fence to existing consumers. Before
slice 17 production readiness is unconditionally false, with a test proving even
the operator cannot enable an incomplete build. Synthetic evidence alone never
satisfies bundle qualification; dynamic validation never changes workflow pins.
Workflow pin checks belong to common admission before the first workflow-owned
activation. Until slice 18, production reload/rollback and operations requiring
replacement refuse as unavailable during preflight, before stopping a healthy
consumer; slice 18 adds orchestration, not a late exception to pin enforcement.

| Slice | Prerequisites and bounded deliverable | Failing-first acceptance |
| --- | --- | --- |
| 1. Disabled admission guard | Existing authority resolver. Add default-false operator switch and shared profile admission guard; no new launch behavior. | Every new lifecycle route refuses before secret/process/mailbox effects; operator cannot enable an incomplete build, and agent/relay cannot enable it or fall back to legacy launch. |
| 2. Binding records | 1. Persist operator-only target bindings/revisions for one Claude credential reference; resolution returns data only. | Actual request path rejects relay mutation, sibling-profile selection and usage disclosure; resolution cannot spawn or read secrets. |
| 3. Per-session storage | 1–2. Allocate private native session roots and retain durable session identity; no launch. | Same-account fixtures cannot share writable roots; failed allocation preserves prior state; execution stays disabled. |
| 4. Mechanism probes | 1 and 3. Hermetic synthetic plugin/agents/MCP/import fixtures and schema version 2 templates; no real-bundle qualification. | Valid fresh identifiers match; altered tools/settings/assets or invalid roots do not. Exercise resume edits/removals, import and token canary; prove no live profile/server access. |
| 5. Bundle verification | 1. Canonical manifest/path/content verification and staging only. | Traversal, escaping/swap symlinks, tampering, hooks/env injection and floating dependencies fail; no executable effects. |
| 6. Adoption operations | 2 and 5. Plan/adopt, retained predecessor and terminal digest tombstones; adoption needs verification/review, not a qualification receipt. | Exact reviewed bytes adopt while production stays disabled; revoked content cannot be re-adopted or rolled back, including after restart. |
| 7. Executable custody | 1 and 5. Acquire/protect the exact executable and dependencies through spawn/exit, using harmless fixtures. | Replace file/parent symlink after checking: verified object runs or admission refuses, never the substitute. Unsupported platform stays disabled. |
| 8. Dedicated-user launcher | 1, 3 and 7. Privileged launch/cleanup and process identity seam, using harmless fixtures only. | Operator fallback and wrong identity/ancestry refuse; all descendants are reaped; unsupported platform stays disabled. |
| 9. OS containment | 4 and 7–8. Isolate trust domains/home, protect binding/bundle/token roots and control tool-child environment. | Actual tool/subagent/MCP fixtures cannot read sibling secrets/operator state, alter adopted bytes or escape authorized paths; canary recipients match policy. |
| 10. Fleet execution fences | 6–9. Authenticated permits, monotonic watchdog, pushed revocation and tombstone synchronization. | Partitioned/uncooperative fixtures stop within 70 seconds; stale/restarted nodes cannot activate or roll back revoked content; expiry alone is not exit proof. |
| 11. Credentials/budgets | 2 and 9–10. Recipient/generation admission, rotation/revocation and owner-held account reservations. | Cross-node starts stay within cap; stale credentials refuse; missing exit proof retains capacity; usage stays separately scoped. |
| 12. Replacement tickets | 10–11. Transfer one same-account slot and recover partial replacement operations. | At cap, transfer follows verified exit; repeats, partition and failed successors never produce two executable generations. |
| 13. Server hook policies | 4 and 9–11. Permission/attribution/admission/audit enforcement and per-node inventory receipt. | Actual prompted/auto-approved fixture paths cannot bypass deletion/managed-file denial or ceilings; unknown hooks block execution. |
| 14. Explicit Claude delivery | 4–7 and 13. Assemble explicit `--tools`, plugin/agents flags and generated MCP merge in the inactive adapter; baseline variant is flock-only. | Missing/altered tools or settings refuse; actual fixture calls/discovery match the template, collisions/ambient sources refuse and flock credentials stay scoped. Production remains disabled. |
| 15. Context validation/replenishment | 4, 6 and 13–14. Typed dynamic slot, compiler validation records and server delivery after resume/compaction before next work. | A permitted snapshot edit validates without requalifying fixed assets; injected role/tool changes refuse. Resume/compaction replenish once; incomplete validation holds input. Production remains disabled. |
| 16. Contained bundle qualification | 1–15. Operator plan/qualify permit executes actual adopted bytes with isolated server/credentials/workspace, recording qualification and context evidence. | An adopted non-synthetic bundle qualifies while production is off; unadopted/tampered/revoked bytes, missing safety receipt or unsafe endpoint refuse. Qualification cannot enable production or own mail; hard deadline reaps children. |
| 17. Claude production activation | 1–16. Enable plan checks complete readiness and exact bundle/template qualification plus current context validation; start with one account and flock-only MCP. | Removing explicit tools, replenishment, custody, qualification or any safety receipt refuses enable/start/resume. Actual admission rejects tampering, relay/sibling access, usage disclosure and unauthorized reload before secret/shutdown effects. |
| 18. Authorized reload | 12 and 15–17. Hash transition, rollback checks and workflow-closure guard; every new bundle qualifies first. | Changed workflow closure refuses before shutdown; same-closure restart retains `def_hash`. Standalone authorized edits diff, removed tools refuse and revoked predecessor never rolls back. |
| 19. Migrate import | 15 and 17–18. Verified PTY writer exit, discovered project key, transcript/sidecar staging and journal. | Active/ambiguous writer or interrupted copy cannot publish partial import; isolated native resume reads imported evidence. |
| 20. Migrate activation | 19. New lease, mailbox commit and crash/rollback reconciliation through the same guard. | Every interrupted boundary preserves one writer/owner and pending delivery IDs; disabled/revoked/missing-context state prevents activation/mail transfer. |
| 21. GUI controls (S17) | 17–20. Existing start/restart command/modal patterns and recoverable operation status. | GUI honors target grants and complete readiness; workflow conflicts show cancel/new-run, not an in-place replacement. |
| 22. Profile/usage projection (S17) | 11, 17 and 21. Display binding/digest/generation and authorized usage. | Selection alone reveals no sibling/aggregate usage; account-read grant changes only the authorized projection. |
| 23. Codex adapter | 9–17, plus separate Codex auth/mechanism/qualification evidence. Private home, generated config, refresh ownership and thread resume; adapter stays inactive until qualified. | Unexpected skills, concurrent refresh ownership or missing containment/custody refuse; actual thread resume preserves identity/context. |
| 24. Codex native importer | 19–20 and 23. Separate rollout/sidecar discovery and import adapter. | Partial/ambiguous import or a live writer refuses; recovery proves one owner and preserves the native thread. |
| 25. Dogfood | 14–22, with 23–24 only for Codex consumers. Isolated recovery drill then operator-authorized staged canary. | Demonstrate real-bundle qualification before enablement, replenishment, immutable workflow pins, revocation/partition bounds and rollback at cap before fleet expansion. |

The original ADR PR was docs only. These caps are planning constraints, not measured
implementation sizes: each implementation brief must enumerate touched functions
and a production-line estimate, subdividing oversized rows into inactive dependent
PRs before work begins. Qualification is the sole pre-activation execution mode
for actual bundle assets. Dedicated-user containment, explicit tools, executable
custody and context replenishment are prerequisites for both qualification and
production, not promises to add after enablement.

## Alternatives and consequences

A mutable account directory containing credentials, skills and transcripts would
make configuration edits authority changes and couple unrelated session writers.
Reusing the operator's profile would also reintroduce ambient hooks and refresh
credential conflicts. Neither is accepted. Automatic bundle fetching at launch
would make review and reproducibility depend on changing external content.

Explicit bindings and discovery receipts add operational work, and resumed
conversations retain historical instructions even after a bundle update. In
return, the operator can identify exactly what was authorized and delivered,
rotate an account without rewriting assets, and roll back content without
reviving revoked authority. Adoption does not replace OS containment.
