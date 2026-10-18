# ADR 0029 — The agent-facing CLI keeps one contract: a send submits and says whether it did, an exit code means one thing on every verb, output is selectable, and errors are stable JSON

- Status: Accepted
- Date: 2026-10-11 (accepted 2026-10-11)
- Scope of acceptance: the decisions in this record, as the operator answered
  them on 2026-10-11 (PR #1006, "Owner decisions"). Acceptance claims no code.
- Issues: #1001 (areas 2 and 4), #995 (submit by default), #996 (field
  selectors and stable errors). Builds on #639 / #640 / #783–#786 (guarded
  submit and confirmed outcomes), #850 (injection lock), #868 (client/server
  skew). Consumed by ADR-0028 §8 (external channels).
- Decision owner: operator.
- Amendment (2026-10-13, #1162): `pane run` without a guard, `pane send-text`
  and `agent send --no-submit` still type with no idle requirement and no
  confirmation, but into an agent pane whose composer holds a draft (or that
  an injection holds) they refuse before typing with `input_not_empty` /
  `injection_pending`, exit 10. `--raw` (socket `raw: true`) opts out. "Stays
  for shells" below now reads: unchanged for shells, draft-safe for agents.
- Proposed amendment (2026-10-16, #1586): §9 describes guarded operations
  on an existing draft. §9 is **not accepted or implemented**; the acceptance
  of §§1–8 does not authorize its pending owner decisions.
- Implemented: none. Slices (§8) filed 2026-10-11: C1 #1017, C4a #1018, C5 #1021, C2 #1024, C3 #996, C4b #995, C4c #1025, C4d #1026.

## Context

Agents are the main users of `flk`. The toolchain survey (#987; 21 days, four
hosts) measured how they use it:

- **Submitting is a two-step workaround.** 704 commands run `flk agent send`
  and then `flk pane send-keys … Enter`; 1,294 send a bare Enter. `--submit`
  appears 32 times against 951 sends. Only 19 of 951 sends exit non-zero.
  The owner reports prompts left sitting unsubmitted in an agent's composer.
  The brief template that drives this fleet still says "send fallback for
  `input_not_empty` / `not_settled` = `pane send-text` + `send-keys Enter`".
- **JSON is parsed by hand.** 1,795 commands pipe flk JSON into ad-hoc
  `python3 -c` or `jq` to pull one field (usually `agent_status`); 50 of them
  crashed because the CLI printed an error or plain text where JSON was
  expected.
- **MCP argument shapes trip agents.** `flock_msg_send` failed 8 of 91 times
  on the shape of `to` (a bare pane id where a tagged object is required).

What the code does today (verified at `ab54ba61`):

- **Plain `agent send` types without submitting**, observes for 2 s and exits 0
  on `delivered`. `--submit` runs the guarded submit: four outcomes, `accepted`
  and `observed_accepted` (exit 0), `unconfirmed` (exit 8) and `abandoned`
  (exit 9). Refusals before anything is typed (`not_settled`,
  `input_not_empty`, `operator_active`, `injection_pending`,
  `unknown_composer`, …) come back as an error envelope with exit **1**, the
  same code as a transport failure. `pane run` without `--if-input-empty`
  types, sleeps 120 ms and presses Enter with no confirmation.
- **Exit codes collide across verbs.** `8` means `unconfirmed` for `agent send`
  but `not_started` for `delegate wait`; `4` means `gone` for waits but
  `refused` for `msg`. There is no central table: `effect_exit_code`,
  `delegate::exit`, `settled::exit`, `msg::exit_for` and
  `compatibility::EXIT_CODE` (78) each define their own.
- **Output.** Most verbs print the raw socket envelope as compact JSON
  (`{"id","result":{…}}`) on stdout. Errors print as a JSON envelope
  (`{"id","error":{"code","message"}}`) **on stderr** with exit 1, so a pipe
  into a JSON parser sees empty input and crashes. `delegate status` prints
  text unless `--json`. There is no `--field` and no global `--format`.
- **Error codes** are string literals spread across `src/app/api/*.rs`, with no
  registry and no documentation. MCP maps them into `data.refusal`.
- **Targets.** `agent.*` verbs resolve a pane id, terminal id or agent name;
  `pane get`, `pane rename`, `pane submit` and a few more take a pane id only.

A sandbox run for ADR-0028 also reproduced the owner's symptom once: a guarded
`agent send --submit` of a three-row prompt left the text in the composer,
unsubmitted. The call's return value was lost to output buffering, which is
itself the point: the caller's only defence is an exit code it can trust.

## Decision

### 1. A send to an agent submits, and the result says what happened to the composer

- **`flk agent send <target> <text>` submits by default**, through the guarded
  submit (#639, with #850's lock and draft stash). `--no-submit` types only and
  is the explicit opt-out. `--submit` stays accepted as a no-op alias for one
  release, with a deprecation line on stderr.
- **MCP has no type-only send.** `flock_pane_submit` already submits; any new
  agent-facing tool that puts text into a pane submits and confirms.
- **The question an outcome answers is "what is in the composer now, and is it
  safe to retry?"** Every send-like verb reports one of:

  | Outcome | Composer afterwards | Safe to retry? | Exit |
  | --- | --- | --- | --- |
  | `accepted`, `observed_accepted` | empty, a new turn confirmed | no (done) | 0 |
  | `refused` (reason: `not_settled`, `input_not_empty`, `operator_active`, `injection_pending`, `unknown_composer`, `restart_pending`, …) | untouched, nothing typed | yes, later | 10 |
  | `abandoned` (reason …) | restored to what it was (the text was removed or never committed) | yes | 9 |
  | `unconfirmed`, reason `typed_not_submitted` | **holds the sent text** | no: inspect or clear first | 8 |
  | `unconfirmed`, reason `confirm_timeout` / `owned_composer_not_visible` | unknown | no: inspect first | 8 |
  | `unconfirmed`, reason `paste_write_incomplete` | partial paste written; remaining bytes may arrive later or be discarded | no: recover the writer/session, then inspect or clear first | 8 |
  | `unconfirmed`, reason `paste_written` | full paste written, acceptance unknown after a guard interrupted submission | no: inspect or clear first | 8 |

  `typed_not_submitted` is new and is the case the survey and the owner
  describe: the text was observed in the composer and no turn started after
  the Enter retry. **On the wire it is a `reason`, not a new `outcome`**: an old
  client maps every outcome other than `unconfirmed` and `abandoned` to exit 0
  (`effect_exit_code`), so a new outcome string would read as success to every
  CLI and MCP process older than the server. Carried under `outcome:
  "unconfirmed"`, it keeps exit 8 on old clients and gains its name on new
  ones. The caller's action is the same either way: look before retrying.
- **`refused` is a client-side name.** The server keeps returning refusals as
  error envelopes with today's codes; the new client maps those codes to the
  `refused` outcome and exit 10. Nothing changes on the wire.
- **A send never reports success it did not observe.** Plain `--no-submit`
  reports `delivered` only on an observed change, as today.
- **`pane run` without a guard** stays for shells (it is how a human script
  types a command), and is documented as not for agents. `pane send-keys Enter`
  stays a raw key verb; docs and skills stop recommending it for submission.
- **Multi-row text** goes through bracketed paste when the harness supports it
  and is otherwise refused (`unsafe_text`) rather than typed line by line.

### 2. One exit-code table for every agent-facing verb

A single registry (`src/cli/exit.rs`) owns every code. Existing codes keep
their meaning where they do not collide; collisions are resolved once:

| Exit | Meaning | Retry class | Used by |
| --- | --- | --- | --- |
| 0 | success, effect observed | — | all |
| 1 | unclassified failure (transport, internal) | maybe | all |
| 2 | usage error | never (fix the call) | all |
| 3 | the other side answered "no / not now" (BLOCKED reply, deferred, expired) | after reading the answer | `msg`, `wait reply`, `delegate wait` |
| 4 | target not found or gone | never (re-resolve) | all |
| 5 | finished without the expected result (no sentinel, no result) | after inspecting | `delegate wait` |
| 6 | the agent needs a human or is blocked (was 3 for `agent wait` / `wait agent-status`) | after the human acts | waits, `delegate wait` |
| 7 | stalled (working, no progress past the stall window) | after inspecting | waits |
| 8 | effect uncertain: text may be in the composer (`typed_not_submitted`, `unconfirmed`) | inspect first | send-like verbs |
| 9 | effect abandoned, composer restored | yes | send-like verbs |
| 10 | refused before acting, nothing changed | yes, later | send-like verbs, guarded verbs |
| 11 | delegate not started (was 8) | after inspecting | `delegate start/wait` |
| 12 | `--field` path missing in the result | never | all (§3) |
| 13 | refused by policy or scope (`msg` refused, channel scope, ADR-0014 boundary; was 4 for `msg`) | never without a change | `msg`, channel adapters, `question answer` (`question_closed`) |
| 14 | dispatched mutation timed out (`request_outcome_unknown`) | inspect state first, never automatically | socket requests |
| 75 | the client gave up reconnecting to its server (`RECONNECT_GAVE_UP_EXIT_CODE`, #436; EX_TEMPFAIL) | yes, once the server is back | the attached client and launcher |
| 78 | client/server protocol gap (#868) | after a handoff | all |
| 124 | timeout before dispatch, or a read/idempotent timeout | yes | waits and socket requests |

- **Four changes break scripts that branch on the old code**, and all four
  are listed in the release's changelog under *changed*:

  | Verb | Case | Old | New |
  | --- | --- | --- | --- |
  | every send-like and guarded verb | refused before acting (`not_settled`, `input_not_empty`, `operator_active`, …) | 1 | 10 |
  | `agent wait`, `wait agent-status` (`settled::exit::BLOCKED`) | the agent is blocked on a dialog | 3 | 6 |
  | `delegate start` / `delegate wait` | not started | 8 | 11 |
  | `msg` waits and status | refused | 4 | 13 |

  The 1 → 10 move is the widest: any `|| retry` written against today's exit 1
  starts seeing 10 for the cases it actually meant. The 3 → 6 move aligns the
  plain waits with `delegate wait`, which already uses 6 for the same signal
  (`AGENT_BLOCKED`) and 3 for a BLOCKED *reply*; today the same agent state
  yields 3 or 6 depending on the verb. All four ship together in one release
  (C4c), each with a one-release stderr notice naming the old code when the new
  one is returned. The survey shows real usage only for `delegate wait` and the
  plain waits (343 blocking waits in 21 days), and their blocked branches are a
  small share.
- Every verb's `--help` lists the exit codes it can return, generated from the
  registry.
- MCP returns the same retry class in `data.retry` and the exit code in
  `data.exit`, beside the existing `data.refusal`.

#### Client operation allowances (#921, #2189)

Creation and cleanup requests (`worktree.create`, `worktree.kill`,
`workspace.create`, `workspace.close`, `agent.start`) share the named
`CLIENT_WORK_ALLOWANCE` through `client_work_allowance` in both delegate and
the CLI operation table. Its 60 seconds include transport overhead and describe
client patience, not a server execution or cancellation guarantee. Explicit
caller deadlines may be shorter. After dispatch, expiry is outcome unknown,
exit 14, and must not trigger an automatic retry. Server-side bounds require
cancellation and partial-state cleanup semantics, tracked separately in #2209.
Input writes instead reserve their server write budget, confirmation window
and transport margin. Live handoff reserves its synchronous phase bounds.

### 3. Output: the result on stdout, selectable; diagnostics on stderr

- **`--format json|text`** is global. Each verb keeps its current default (most
  print JSON today; changing that would break the 1,795 pipes the survey
  counted). `--json` stays as a synonym for `--format json`.
- **The JSON success shape does not change**: `{"id": …, "result": {…}}`.
  Scripts reading `d["result"]["agent"]["agent_status"]` keep working.
- **`--field <path>`** is global and repeatable. The path is rooted at
  `result` (`agent.agent_status`, `agent.state.phase`, `agents[].name`), dotted,
  with `[]` to map over an array and `[n]` to index. Each value prints on its
  own line: strings raw, other values as compact JSON. A missing path prints
  the §4 error envelope and exits 12; it never prints an empty line that a shell
  test would read as a value. `--field` implies JSON parsing internally and
  ignores `--format text`.
- **stdout carries only the result.** Progress, warnings, deprecation notices
  and skew notices go to stderr, always.

### 4. Errors: a stable envelope, on stdout when the caller asked for machine output

- When `--format json`, `--json` or `--field` is given, an error prints **on
  stdout** as

  ```json
  {"id": "…", "error": {"code": "agent_not_found", "message": "…",
   "retry": "never", "exit": 4, "data": {…}}}
  ```

  and a one-line human message goes to stderr. The exit code comes from §2.
  `id` is the failed request's id; a failure the client finds before or
  without a request (no server socket, a usage error) carries `"id": null`,
  so the key is always present (#1024).
  Without machine output, errors are a human line on stderr, as for any CLI.
- **Error codes are a registry**, an enum with `as_str()`, exit code and retry
  class, replacing the string literals. The full list is generated into the
  CLI reference docs. A drift-lock test pins the set: a new code is an addition,
  a rename needs an alias that keeps the old string for one release, and a
  removed code fails the test.
- **The skew error stays `server_version_gap`, exit 78** (#868), and gains the
  envelope.

### 5. Arguments: accept the obvious shape, refuse with the expected one

- Every verb that takes an agent or pane target resolves pane id, terminal id
  and unique agent name through the one resolver
  (`App::resolve_terminal_target`), including today's pane-id-only verbs.
- MCP `flock_msg_send` accepts a bare string for `to` and resolves it the same
  way (pane id, terminal id, agent name, then fleet agent id); the tagged
  object stays accepted. An ambiguous or unknown string is refused with
  `agent_target_ambiguous` / `msg_target_not_found` and the candidate list.
- A refused argument names the expected shape in `message` and in
  `data.expected`, so the caller's next call can be right.

### 6. External channels use the same contract

ADR-0028 §8's channel adapter reports outcomes to a remote sender from this
table: `queued`, `woken` (wake `accepted`), `read`, `replied`, and
`unconfirmed` / `abandoned` / `refused` as themselves. Scope refusals are exit
and code 13 (`channel_scope_refused`).

### 7. Mixed fleets

Requests carry no client protocol version, so every change in this ADR is either
client-side or additive on the wire:

- **Client-side only:** exit-code and error-code mapping (C1, C4c), the error
  envelope on stdout, `--format` and `--field` (C2, C3), the `agent send` default
  (C4b: the new client calls the guarded submit, which every server since
  v1.0.0 has), and MCP's bare-string `to` (the bridge converts it to the tagged
  object before sending, so old servers accept it).
- **Additive on the wire:** `reason: "typed_not_submitted"` under `outcome:
  "unconfirmed"` (C4a; old clients still exit 8), and `data.retry` / `data.exit`
  in MCP errors (produced by the bridge, which is the client binary).
- **Server behaviour a new client may lean on:** name resolution for today's
  pane-id-only verbs (C5). Against an older server the new client resolves the
  name through `agent.get` first and sends the pane id.
- No slice here needs a protocol bump on its own; C5's resolver change is a
  relaxation, not a new shape.

### 8. Slices

| # | Slice | Depends on | Replaces / amends | Skew (§7) | Acceptance |
| --- | --- | --- | --- | --- | --- |
| C1 | Exit-code registry and error-code enum with retry class; `--help` exit sections generated; drift-lock tests. No code changes value yet | — | — | Client-side | Every existing code path returns the same code as before; a test lists every code and fails on an unregistered literal |
| C2 | Error envelope on stdout under `--json` / `--format json` / `--field`, human line on stderr; global `--format`; MCP `data.retry` / `data.exit` | C1 | #996 (errors half) | Client-side | `flk agent get nosuch --json \| python3 -c 'import json,sys; json.load(sys.stdin)'` succeeds and exits 4; plain `flk agent get nosuch` prints one human line |
| C3 | Global `--field <path>` with `[]` / `[n]`, exit 12 on a missing path | C2 | #996 (selector half) | Client-side | `flk agent list --field 'agents[].name'` prints one name per line; a missing path exits 12 with the envelope |
| C4a | `typed_not_submitted` detection in the guarded submit, as a `reason` under `unconfirmed` | — | #995 (outcome half) | Additive; old clients exit 8 | Sandbox probe with a composer that swallows Enter: `unconfirmed` / `typed_not_submitted`, exit 8, on both the new client and the last released client |
| C4b | `agent send` submits by default; `--no-submit`; `--submit` becomes a deprecated no-op | C1 | #995 (default half) | Client-side | A send into a settled empty composer is `accepted`, exit 0; `--no-submit` types only |
| C4c | The four exit-code moves of §2 with one-release stderr notices and the changelog table | C1 | — | Client-side | Each moved case returns its new code and names the old one on stderr |
| C4d | Docs, `architect` skill, brief template and watch-nudge text move off `send-keys Enter` and the `send-text` fallback | C4b | survey item 2 | — | `rg 'send-keys .* Enter'` finds no agent-facing recommendation in docs or skills |
| C5 | Uniform target resolution for pane-id-only verbs; MCP `to` accepts a bare string; `data.expected` on shape refusals | — | survey item 11 | New client pre-resolves against old servers | `flk pane get <agent-name>` works; `flock_msg_send {to: "w3:p2"}` delivers |

### 9. Proposed amendment: guarded operations on an existing draft (#1586)

**Status: proposed, docs only. Decision owner: operator.** This section belongs
here because it extends §1's composer outcomes, §2's exits, §§3–5's CLI contract
and §7's compatibility rules. It uses the existing guarded-submit funnel in
`src/app/guarded_submit.rs`; it does not amend the spawn permissions in
ADR-0014. #1586 tracks implementation and every capability restriction below.
The #1582 refusal guidance remains manual until a reviewed implementation lands.
This proposal composes with ADR-0028 §5a's accepted completed-write contract;
its named capability, error schema and budget details still require the owner
rulings marked there and are not implemented by recording this amendment.

#### 9.1 Explicit observation, then a conditional action

Proposed commands (not available today):

```text
flk agent draft inspect <target> --json
flk agent draft clear <target> --if-revision <opaque-token> --json
flk agent draft submit <target> --if-revision <opaque-token> --json
```

Use §5's existing resolver for pane id, terminal id and unique agent name.
There are no new pane aliases, free-form key sequences, text arguments,
`--raw`, `--force`, implicit inspection or automatic clear-before-send flags.
`send` continues to send a new prompt; `draft submit` acts on the existing
composer without appending or retyping it. `clear` moves a draft into the
existing recovery stash rather than permanently deleting it. Delegate callers
still resolve the draft before `delegate send <name> --brief FILE`; switching
verbs never bypasses a draft guard.

`inspect` returns the existing success envelope with `result.draft` containing
`pane_id`, `agent_id`, `revision`, `visible_bytes`, `capabilities` and `reason`.
`capabilities` has separate `clear` and `submit` booleans for the editor;
these describe support, not caller permission. Unsupported or empty
composers have a null revision and an explicit reason. The byte count is only
visible UTF-8 evidence, never an authorization check or a total for hidden
content. No composer excerpt, content hash or draft text enters this response,
errors, MCP metadata, audit events or diagnostic logs.

The revision is a cryptographically random server-issued handle with at least
128 bits of entropy into bounded, ephemeral server state, not a digest that
reveals a guessable secret. Fail closed if secure randomness is unavailable.
Lookup is keyed by the resolved pane and handle together; a handle from another
pane cannot resolve here. Bind it to the server instance, durable agent id,
pane/terminal and child launch generation,
harness, composer revision, screen geometry, cursor and selection state, and
the generations of operator input and all queued pane writes. Bind any required
session identity too. A composer revision increases on every observed change,
including A → B → A; equal byte counts or equal final text do not revive it.
Unrelated output outside the composer need not invalidate it unless it changes
a guard or the visible controls. Propose a 60-second inspect-to-act lifetime,
giving a human time to review and invoke the explicit action. It is separate
from the mutation lease and grants no grace against a changed revision: every
action compares again. Restart or handoff invalidates it immediately.
Bound outstanding records per pane; capacity exhaustion refuses a new
observation explicitly, and expiry or eviction invalidates the old handle.
A token is a precondition, **never authority**. Do not persist or log it.

#### 9.2 Revalidate under the shared injection lease

Both mutations require an explicit, unexpired revision. Resolve the target,
authorize the caller, acquire the same exclusive injection funnel as guarded
send, then re-read the current detection buffer and all bound generations.
Require an applied detection-sample receipt covering all PTY output received
so far, following ADR-0028 §1b: the sampled output cursor must cover current
parser progress, with no received output still pending parsing or detection.
Capture text, styles, geometry and cursor together; a recent timestamp or an
old classified status is insufficient. Pending output requires a new completed
sample within the action budget, otherwise refuse before writing. Compare the
receipt and bound generations immediately before the tracked PTY write under
the same serialization that excludes competing pane writers. Pending bytes,
operator input, a different target or any changed guard invalidate the attempt before
keys are queued. Do not use the user's scrollable viewport. Recheck between
stages and before Enter. Disabling the optional send injection lock must not
disable this lease; if exclusivity cannot be established, refuse.

**Proposed budget, subject to owner acceptance:** inspection tokens last at most
60 seconds; mutation admission starts one monotonic action clock. Reserve a
15-second caller cap, a write deadline at admission +10 seconds (including
preflight, recovery storage, lease waiting, queues and all encoded writes),
confirmation ending by admission +12 seconds, and a 1-second reply margin.
The draft-specific lease ends by admission +12 seconds; do not reuse the
existing seven-second `INJECTION_LEASE` unchanged for this operation. Each
write uses the remaining time to the same absolute write deadline, never a
fresh ten seconds. Confirmation has at most two seconds and still ends by
+12 seconds; clear read-back additionally keeps its existing shorter
`DRAFT_STEP` limit. Shorter caller/transport budgets reduce the write deadline
and confirmation allowance while reserving reply time; require the write
budget to be strictly below the remaining caller cap minus the reply margin,
as ADR-0028 §5a requires. Refuse before dispatch if no positive budget fits.
No stage, handoff or retry resets a deadline. These are proposed budgets for
new verbs, not changes to current send timeouts. Enable neither mutation until
the §1b sample contract and the full §5a completed-write capability exist.

Require the existing settled/idle, no-dialog, no-restart, recent-operator-input
and session gates. A turn beginning, loss of composer visibility or a changed
child invalidates the observation. The first implementation accepts only a
fully visible, single, unwrapped plain ASCII editor row whose complete contents,
cursor-at-end position and absence of selection/chips can be established.
Preserve leading spaces in the internal snapshot; do not authorize from today's
trimmed `composer_contents` or `visible_bytes`. Refuse multiline/wrapped or
clipped drafts, non-ASCII editing units, paste/image chips, hidden selections,
ambiguous hint text, menus and unsupported harnesses. Each capability must be
proved separately; reading a draft does not establish that clearing is safe.

This is a compare-and-act on **observed** state, not an atomic transaction with
the harness. A PTY cannot prove that an unobserved application-side change did
not occur before it consumed a key. Serialize server input, invalidate on known
input/output generations, and report post-write uncertainty honestly. Never
claim full logical-buffer equality from a screenshot. If a harness cannot
establish the required projection or key behavior, leave that capability
unavailable; do not substitute blind control keys. Stronger guarantees require
an authoritative harness adapter, tracked under #1586 before widening support.

#### 9.3 Per-harness actions and failure recovery

These are candidate sequences for the narrow shape above, encoded through the
runtime's terminal key encoder, not literal escape strings or shell commands:

| Harness | Clear candidate | Submit candidate | Enablement requirement |
| --- | --- | --- | --- |
| Codex | End, then one Backspace per verified ASCII editing unit | Enter once | Real isolated composer proves End/Backspace semantics, empty read-back and turn evidence; chips and wrapped rows refuse |
| Claude Code | End, then one Backspace per verified ASCII editing unit | Enter once | Same proof, with faint suggestions distinguished from input and trust/permission dialogs refused |
| opencode | End, then one Backspace per verified ASCII editing unit | Enter once | Same proof, with editor boundaries and idle footer visible; busy/menu footer refuses |

Existing `clear_keys` uses End plus character-count Backspaces and eight slack
Backspaces for stashing a one-row draft. That is implementation evidence, not
certification for these new verbs. Do not inherit the slack heuristic, use
Ctrl-C (which can cancel a turn), Ctrl-U (cursor-relative), select-all shortcuts
or assume a new harness version preserves editing behavior. A harness lacking
proof returns `draft_action_unsupported`, zero keys, and manual UI guidance.
This design PR claims no new live-harness validation or detection changes.

For **clear**, durably secure the recoverable draft in the existing private
stash before the first destructive key. If that fails, write nothing. Consume
the revision at action start, send the one bounded clear sequence once, then
require a timely full §5a write receipt, then read back an empty recognized
composer within the shorter `DRAFT_STEP` and §9.2 action ceilings. Do not retype
or automatically restore over unexpected content.
Report `cleared` only after empty read-back, with a recovery reference and the
existing `flk pane stash show <pane_id>` / `restore <pane_id>` commands; restore
retains its empty-composer guard. Retain recovery data across partial writes,
server death and handoff. Existing explicit stash reads can reveal content to
an authorized local user; count-only mutation responses must not do so.

For **submit**, secure recovery data before Enter, consume the revision and
admit one §5a write ticket containing exactly one encoded Enter and no draft
text. "Exactly one Enter" means one such payload, never a second dispatch or
semantic retry; necessary partial syscalls finish only that ticket within its
deadline. Queue admission is not delivery. Require its timely full-byte receipt
and separate existing turn/session and composer-change evidence within §9.2's
confirmation ceiling. This does not promise exactly-once harness execution.
`accepted` or `observed_accepted` requires the existing acceptance evidence; an empty editor
alone is insufficient. Do not retry Enter automatically on this path. An
unchanged or unreadable composer without confirmed acceptance is `unconfirmed`;
the caller must inspect and obtain a fresh revision before any later action.
Once any bytes may have reached the PTY, never return a pre-action refusal.
Drop the submit-only recovery copy only after a timely full write receipt and
confirmed acceptance have both been recorded with the terminal audit outcome.
Do not delete a pre-existing clear stash. On uncertain or failed submission,
retain recovery until explicit operator restoration or deletion, including
across restart/handoff; retention of data does not extend the write lease.
Today `KeptDrafts` is memory-only with handoff export; crash durability and bounded
recovery admission are additional #1586 requirements, not existing guarantees.
Refuse before writing if bounded recovery storage is exhausted; never silently
evict an unresolved draft to admit a new action.

Both actions record an attempt id and action, identities, outcome/reason and
recovery reference without draft contents. Keep crash recovery separate from
success evidence. Before releasing the lease, account for held operator input
using existing replay rules; held input arriving during the operation prevents
an unconditional claim that the composer is still empty after replay. Partial
or unobserved writes report uncertainty and retain recovery data. Require
ADR-0028 §5a's full `pane_input_write_ack` contract, including exactly one
bounded terminal resolution by the owning writer. Cancellation sets the gate's
cancelled state and discards every unsent byte before replying, including after
a partial write; zero-byte cancellation removes live payloads from all queues.
Resolve/cancel before held-input replay or lease release, within the absolute
write deadline. Report held input not written if safe replay cannot be proved.
A lost receipt returns `input_write_unknown` without a no-ghost-write claim;
the owning writer still must resolve/cancel by its deadline. Never retain an
open-ended recovery blocker waiting for a runtime to settle or a writer to be
fenced. Producer death or unexpected state ends the bounded wait honestly,
following ADR-0037 §1; data recovery is separate from mutation serialization.

#### 9.4 Refusals, outcomes and mixed versions

Use §§2–4's envelopes, selectable output and human stderr. New codes below are
**proposed registry additions**, not strings to scatter through handlers.
Register their exits/retry classes centrally before implementing them; MCP
preserves structured detail without overwriting its registered classification.

| Condition | Code or outcome | Exit / retry |
| --- | --- | --- |
| Missing/malformed revision or conflicting flags | `invalid_params`, with `data.expected` | `USAGE` (2), never |
| Unknown/ambiguous target | Existing resolver error and candidates | Existing registered exit; missing target is `GONE` (4) |
| Agent caller or unattested mutation caller | `operator_only` or proposed `draft_caller_unattested` | `POLICY_REFUSED` (13), never without authority change |
| Changed, expired, consumed or evicted revision | Proposed `draft_revision_mismatch`, fresh inspection remedy | `REFUSED` (10), later after new inspection; never retry same token |
| Unsupported shape/harness or empty draft | Proposed `draft_action_unsupported` / `draft_empty` | `REFUSED` (10), manual resolution / fresh inspection |
| Busy lease, active operator, unsettled or unreadable editor | Existing `injection_pending`, `operator_active`, `not_settled`, `unknown_composer` | `REFUSED` (10), later |
| Restart/session/child change | `draft_revision_mismatch`, existing guard reason in data | `REFUSED` (10), inspect/re-resolve first |
| Observation capacity exhausted | Proposed `draft_observation_capacity` | `REFUSED` (10), later after expiry/capacity recovery |
| Recovery storage fails before any keys | Proposed `draft_recovery_unavailable` | `REFUSED` (10), later after storage recovery |
| Timely full write receipt plus clear observed empty or submit accepted with evidence | `cleared`; `accepted` / `observed_accepted` | `SUCCESS` (0), done |
| Zero-byte cancellation; all payloads removed and ticket irrevocably cancelled | §5a `input_write_cancelled`, exact zero count | `REFUSED` (10), later after inspection and a fresh token; nothing written |
| Positive-byte partial write or late completion, including late full write; remainder discarded | §5a `input_write_failed`, exact count and recovery reference | `UNCONFIRMED` (8), `after_inspecting`; no automatic full/suffix retry |
| Timely receipt `Ok` without submit acceptance evidence, or clear without empty read-back | `unconfirmed`, action-specific reason and recovery reference | `UNCONFIRMED` (8), `after_inspecting` |
| Transport/server loss after dispatch without an authoritative outcome | Local §5a `input_write_unknown`, null counts; effect unknown | `UNCONFIRMED` (8), `after_inspecting`; never assume zero or resend automatically |
| Server lacks these methods/capability | `server_version_gap` | `PROTOCOL_GAP` (78), after handoff |

Exit 9 is reserved for an attempt whose original composer was demonstrably
restored; this first design never promises that restoration automatically.
Exit 124 must not disguise an uncertain mutation as a safely retryable wait.
Use `DeliveryAttempt` evidence for submit outcomes; clear has its own `cleared`
result, not a new send outcome that an old client might mistake for success.
New methods require explicit capability discovery and fail closed against old
servers. Require `pane_input_write_ack` at the destination and every forwarding
hop, plus contract-bound draft method tags that an old owner rejects before
writing even after a stale capability lookup. Follow §5a's single end-to-end
deadline and disable generic transport retries. No fallback to legacy write
methods, `pane send-keys` or `send --raw`. Implement protocol
version policy relative to the latest release, without changing mesh VERSION 5.
No existing request, outcome, command or refusal is reinterpreted by this PR.

#### 9.5 Authority and MCP

Following ADR-0014 §§1/6/7, narrow operations and server-side authorization
carry the policy; CLI flags and MCP builders cannot grant permission.
Recommended initial policy: only an attested operator in an operator-owned,
non-agent pane may clear **or submit** a draft. Deny target agents, parents,
registered delegates and lineage peers: lineage is not consent. Deny unknown
credentials and unattested callers rather than treating the spawn classifier's
`Operator { pane: None }` as human evidence. No `--operator` flag, claimed pane
id, revision token or display name grants authority.

#1177 documents that agents currently run as the operator's Unix user:
process ancestry is only cooperative attribution, not a security boundary
today. Stronger attestation depends on #1426's session-lease caller identity
and #1629's kernel peer-UID check with the dedicated agent user, following
ADR-0036 §1a and #1177's isolation decision. Carry trusted peer credentials and
lease identity through parked continuations; deny the agent UID even with
forged operator ancestry, and never turn an unknown UID into operator authority.
PID reuse introduces a TOCTOU risk: bind PID to process start time and pane/lease
incarnation, then revalidate live bindings and authority before mutation
admission, including after parking. A bare PID or cached ancestry is insufficient.

This operator-pane-only recommendation is narrower than spawn's unattested
operator behavior and still needs owner approval. Stronger identity/isolation
must be integrated before presenting these mutations as an enforced boundary;
ancestry alone cannot certify a human keyboard or hostile same-UID code.
External-terminal grants and agent delegation require separate owner rulings
under #1586, rather than inferring permission from sender identity.

Propose `flock_agent_draft_inspect` as a read-only MCP tool, with the same target
resolution and existing pane-read access rules; it returns only the count,
capabilities and observation token. Keep both mutations out of the closed MCP
tool table in the first implementation (`not_exposed_via_mcp`), since an MCP
request is not human consent. Direct socket calls still meet the server-side
authority check. A future narrowed delegated capability needs a separate owner
ruling and tests under #1586; do not expose a wrapper around a raw key verb.

#### 9.6 Acceptance plan (red before implementation)

The following are implementation acceptance cases, **not passing tests claimed
by this docs-only PR**. Capture each expected failure on the implementation
branch before adding the producer; assert actual byte logs and outcomes, not
only constructed state. Existing generic send tests cannot establish these
new operations. Use isolated servers/PTYs with fictional fixtures, never a live
operator profile. No new sleeps, retries or widened deadlines.

| Case | Required observation / failure proof |
| --- | --- |
| Commands and output | Today the three verbs are absent; future plain/JSON/field tests pin usage, resolver behavior, error envelope and stderr remedy |
| Compare-and-act | A fresh applied sample covering all received output is required; pending parsing/detection or stale receipts cannot authorize keys. Observed same-length replacement, observed A → B → A, input queued before screen paint, resize, cursor/selection change, restart, target retarget and expired/consumed token all refuse with zero keys |
| Serialization | A competing send, raw write, self-compact or operator input cannot slip between validation and enqueue; disabling send's optional lock cannot disable this guard |
| Safe clear, each harness | Real event-driven fixture paints a supported row; inspection → clear removes exactly that draft, confirms empty and leaves recoverable stash data; repeat with leading spaces |
| Safe submit, each harness | Inspection → submit emits exactly one encoded Enter and zero draft text bytes; one turn starts; no duplicate submission on swallowed Enter or lost response |
| Unsupported editors | Wrapped/multiline/hidden/non-ASCII/chip/selection/dialog/unknown-harness cases return unsupported or existing guard refusal with zero keys; phantom faint suggestions never become drafts |
| Authority | Attested operator succeeds only with the stronger identity dependencies; agent UID, unknown credentials, detached agent, target/parent/delegate, forged fields, PID reuse and stale parked lease fail with exit 13 before recovery writes or keys |
| MCP and skew | Inspect preserves count-only fields; mutation tools are absent/refused; direct socket denial holds; old server yields exit 78 with no raw-key fallback |
| Recovery and uncertainty | Failed stash write sends zero keys; partial clear, failed empty read-back, held input replay, lost Enter acknowledgement and crash/handoff retain recovery and cannot report false success or exit 10 after bytes; zero-byte cancellation removes all queued payloads; partial cancellation sets cancelled and discards the remainder before its single reply; late full writes and lost receipts return 8, never a safe retry |
| Bounded failure | A producer that stays alive but silent, dies, or emits an unexpected stage fails within §9.2's absolute budget, including queue/lease time; receipt Ok without acceptance returns 8; cancellation cannot leave an open-ended blocker; deliberately inject each regression once |
| Privacy and registries | Secret-shaped fixture contents never appear in new response/error/audit/log fields; tokens have at least 128 bits of entropy, pane-bound lookup and 60-second expiry; accepted submission drops only its recovery copy; tokens are not content hashes; code/exit drift locks and legacy send-envelope readers still pass |

#### 9.7 Owner decisions pending (recommendations, not acceptance)

1. **CLI shape:** accept the explicit `agent draft inspect|clear|submit` family
   and mandatory `--if-revision`; reject byte-count-only or implicit mutation.
2. **Authority:** accept attested operator-pane-only mutations initially,
   including the stricter refusal of unattested peers. Agent/self/parent
   delegation and external-terminal grants remain unavailable under #1586.
3. **MCP:** expose read-only inspect and keep clear/submit off MCP initially.
4. **Recovery/privacy:** clear means recoverable removal, not permanent deletion;
   secure recovery for submit too, with no draft text in new diagnostics.
5. **Harness support:** certify each harness separately for the narrow ASCII
   row; broader shapes require new proof under #1586.
6. **Observed-state guarantee:** accept the projection's limits; require the
   §1b receipt covering all received output, without claiming logical-buffer CAS.
7. **Budgets:** accept the 60-second inspection TTL and §9.2's 15-second caller,
   10-second write, 12-second lease/confirmation and 1-second reply-margin plan.
8. **Enter semantics:** one encoded Enter ticket, a timely full §5a receipt and
   separate acceptance evidence; never retry automatically or claim exactly-once
   harness execution.
9. **Uncertainty:** adopt §5a's zero-byte exit 10 and partial/late/unknown exit 8
   distinctions, with bounded cancellation and no open-ended recovery blocker.
10. **Capability rollout:** require the full §5a write contract and contract-bound
    methods throughout forwarding; no legacy fallback or bypass flag.
11. **Recovery retention:** drop only the submit recovery copy after receipt,
    acceptance and audit completion; retain uncertain copies until explicit
    restoration/deletion, with crash-durable bounded recovery admission under
    #1586 and no silent eviction or extended lease.

No implementation is authorized by recording these recommendations. Owner
acceptance must be recorded here before a production implementation PR; until
then all current manual-resolution guidance remains accurate.

## Consequences

- An agent can branch on an exit code without knowing the verb, and every
  non-zero code says whether retrying is safe.
- The composer-left-full failure is reported as itself.
- Scripts that parse today's JSON keep working; scripts that branch on any of
  the four moved codes (§2: 1 → 10, 3 → 6, 8 → 11, 4 → 13) need a one-line
  change, announced for a release with stderr notices.
- The CLI reference gains generated exit and error tables, which the docs
  currently lack.

## Alternatives considered

- **Keep type-only as the default and teach `--submit`.** The survey shows
  that teaching did not work (32 of 951). The safe default is the one that does
  what callers mean.
- **New envelope shape (`{"ok":…, "value":…}`).** Cleaner, and breaks every
  existing parser. Rejected; the envelope stays and gains fields.
- **Errors always on stdout.** Breaks human use and shell conventions. Only
  when machine output is requested.
- **Per-verb exit tables, documented better.** The collisions are the problem,
  not the documentation.
- **`jq` as the answer to selection.** Not installed everywhere and still
  crashes on a non-JSON error. `--field` composes with the error envelope.

## Risks

- **A default flip surprises a human using `agent send` to stage text.**
  `--no-submit` covers it, and the change ships with a changelog entry.
- **Guarded submit refuses more often than blind Enter.** That is the point,
  but agents will see more exit 10s at first. The message says what to wait
  for, and ADR-0028's `events wait` gives them the event to wait on.

## Test strategy

- Drift locks over the exit and error registries (a code may be added, not
  renamed or removed silently).
- Golden tests per verb family for `--field`, `--format` and the error envelope
  on stdout.
- A compatibility test that reads today's success envelopes with the paths the
  survey's pipes use (`result.agent.agent_status`, `result.agents[]`).
- A sandbox probe with fake composers (Claude, Codex, opencode fixtures from
  `tests/delegate.rs`) for each send outcome, including one whose Enter is
  swallowed.

## Owner decisions

Decided by the operator on 2026-10-11 (PR #1006): every item below was
accepted as recommended. The rejected alternative is kept for the record.

1. **`agent send` submits by default** (C4b), with `--no-submit` and a
   one-release deprecation of `--submit` as a no-op. Decided: yes; the
   survey shows teaching `--submit` failed (32 of 951 sends).
2. **The four exit-code moves in one release** (§2: 1 → 10, 3 → 6, 8 → 11,
   4 → 13), each with a one-release stderr notice. Decided: yes, together,
   so scripts are updated once. Rejected: keep 3 for the plain waits and
   accept that one signal has two codes depending on the verb.
3. **Errors on stdout with `--json` / `--field`**, a human line on stderr.
   Decided: yes; without machine output nothing changes.
4. **Refuse multi-row text without bracketed paste** (`unsafe_text`) instead
   of typing it line by line. Decided: yes; line-by-line typing submits the
   first line early in every harness.
