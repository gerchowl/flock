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
| 75 | the client gave up reconnecting to its server (`RECONNECT_GAVE_UP_EXIT_CODE`, #436; EX_TEMPFAIL) | yes, once the server is back | the attached client and launcher |
| 78 | client/server protocol gap (#868) | after a handoff | all |
| 124 | timeout | yes | waits |

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

