# ADR 0052 — Structured harness profiles separate operator authority, immutable assets and session storage

- Status: Proposed
- Date: 2026-11-08
- Issues: [#3066](https://github.com/gerchowl/flock/issues/3066)
- Decision owner: operator
- Implemented: none. This record proposes the provisioning contract and ordered
  implementation slices. It changes no runtime behavior.
- Amends on acceptance: ADR-0036 §1 isolation and hot reload, and ADR-0032 §3
  structured-session context replenishment. Composes with ADR-0049 requests,
  operator bindings and the shared authority resolver. Existing accepted ADRs
  remain unchanged by this proposal.

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
other MCP children never receive that credential. Slice 1 allows flock alone.
Do not use `--add-dir` as an implicit asset-loading escape hatch.

Every delivery claim is gated by a discovery receipt for the exact absolute
binary, version and binary digest, bundle digest, argv/settings digest and
platform. A minimum version check is insufficient. A binary or flag change
invalidates the receipt. Fail closed if required assets are absent or unexpected
ambient hooks, skills, plugins, servers or tools appear; enumerate and explicitly
account for built-ins and managed policy. `system/init` names are evidence but
must be supplemented by actual tool calls and skill/agent marker observations.

The sandboxed probe must:

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
new starts/resumes immediately and stops affected running sessions; provider-side
revocation is also required because a leaked token can be used outside flock.
Rollback may reuse an old bundle but never a revoked credential or grant.

Use account-wide reservations, including starts, forks and resumes across nodes,
with idempotent acquisition, lease generation and release after verified exit.
Enforce delegated node quotas whose sum cannot exceed the account cap, or an
authoritative shared allocator; never infer an account budget from node-local
counts. Refuse admission when budget authority is unavailable. Allocation authority
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

Authorize the proposed digest and credential generation **before shutting down**
a healthy process. Verify bytes and reserve budget, then wait for idle (no started
input or open control request), close stdin, verify writer and descendants exited,
transfer the lease and resume the same session. Recheck binding revision, grants,
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

## Ordered implementation slices

Every numbered row is a separate PR with **at most 400 production lines**,
excluding tests and doc comments. Each begins with the stated failing test or
probe against the prior build, then records the passing evidence. If the minimum
implementation exceeds the cap, split it into additional ordered PRs at the same
phase; do not remove an authority check to fit. Dependencies stay serial.

| Slice | Bounded deliverable | Failing-first acceptance |
| --- | --- | --- |
| 1. Claude binding and discovery | Operator-adopted profile bindings, per-session writable storage, one credential reference and flock-only MCP; sandboxed discovery runbook/receipt under production flags. Candidate plugin/agents fixtures are probe-only until slice 3. | Real admission-path tests refuse relay access, sibling-profile selection, account usage disclosure, tampered bundles and unauthorized reload before secret access/shutdown. Two sessions share an account but no writable directory. Probe explicit tools/MCP, plugin/agents loading, changed/removed skills on resume, credential inheritance and transcript/sidecar import. Delivery remains disabled if the probe cannot establish it. |
| 2. Bundle adoption | Canonical digest/manifest verification, reviewed operator adoption, atomic publication and retained previous version. | Traversal, escaping/swap symlinks, changed bytes, forbidden hook/env metadata and unpinned executables refuse adoption or launch; prior version survives failure. |
| 3. Claude bundle delivery | Implement only probe-established plugin/agents flags, explicit tools and generated MCP merge with reserved flock entry. | Production-path discovery matches the manifest and rejects ambient assets, duplicate MCP names and tools outside the approved set. A real call reaches each authorized server without sharing flock's lease credential. |
| 4. Credential/account admission | Recipient/generation fences, revocation/rotation and distributed budget reservations. | Concurrent cross-node starts cannot exceed cap; stale generation and revoked recipients fail; lost allocator fails closed and usage read remains separately scoped. |
| 5. Server policy and hooks | Map the security hooks into permission/admission/attribution/audit evaluation; inventory receipt gates cutover. | A real control request hits the resolver, refuses out-of-scope deletion/managed-path writes and escalation, and emits attributed audit; unknown hooks block migration. |
| 6. Reload/context | Authorized digest transition, hash diff, activation rollback and server replenishment. | Denied reload preserves the old writer; same-name edits are detected; removed tools fail on invocation; resume/compaction injects current context exactly once. |
| 7. Migrate: import | Verified PTY writer shutdown, discovered project key, transcript/sidecar staging and durable journal. | An active writer, ambiguous transcript or interrupted copy cannot publish a partial import; sandbox native resume reads the imported conversation. |
| 8. Migrate: activate | New lease activation, mailbox ownership commit and crash/rollback reconciliation. | Failure at each transaction boundary leaves exactly one owner/writer, preserves pending delivery IDs and can recover from retained data. |
| 9. GUI start/restart (S17) | Existing GUI command/modal patterns resolve authorized bindings and show activation outcomes. | GUI-to-server start/restart selects only target-granted digests; stale/revoked selection is visibly refused without an orphan process. |
| 10. Profile/usage display (S17) | Project resolved binding, digest, generation and authorized usage without secret paths/bytes. | Profile selection alone reveals no account aggregate or sibling usage; explicit account-read grant changes only the authorized projection. |
| 11. Codex adapter | Private home, separate auth/refresh contract, generated config and thread resume, after a pinned discovery probe. | Unexpected skill sources and concurrent refresh ownership fail safely; actual app-server resume preserves the intended thread. Split the rollout importer into a following capped PR if needed. |
| 12. Dogfood | Opt-in staged fleet cutover with inventory/discovery receipts, one session first and rollback drill before expansion. | Isolated end-to-end rehearsal demonstrates failed activation, revocation and recovery before any operator-authorized live cutover. |

This ADR's first PR is docs only. The first implementation slice establishes
Claude admission and evidence; bundle rollout, fleet migration and GUI exposure
follow those gates. Dedicated-user infrastructure is a prerequisite for claiming
trust-domain isolation, regardless of which slice first needs it.

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
