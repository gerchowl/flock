# ADR 0032 — An agent's starting context is compiled in five layers, most stable first, and the stable layers ride the harness's cached system prompt

- Status: Proposed
- Date: 2026-10-11
- Issues: #1042 (the spike this records). Composes with #573 / ADR-0030 (goal
  supervision, G6 re-inject), ADR-0031 (goal state storage, PR #1068),
  ADR-0027 §4 (delegate table after compaction, #962), ADR-0028 (events,
  `events.read`), #1043 (forked AX reviews), #1044 (goal heartbeat
  fallback), #1062 (slash command after self-compaction).
- Decision owner: operator.
- Implemented: none. The prototype and the measurement scripts are under
  `scripts/research/context_compile/`; the slices (§9) are not filed yet.

## Context

When an architect resumes after a compaction, or a builder starts, its
context is assembled by hand and by the agent itself, one tool call at a time.
Nothing decides what is loaded, in which order, or which part of it is
byte-identical to what the provider already has cached.

### What is loaded today

Inventoried from the flock fleet's own architect session (one Claude Code
transcript, eight compactions between 2026-10-08 and 2026-10-11) and from 16
builder spawns on 2026-10-11 (Claude Code 2.1.x, Opus 5.5, one host).

**Architect resume.** The harness injects the compaction summary (9.6k–12.9k
tokens after compaction, from 210k–637k before) plus its own attachments. Then
the architect re-orients from files, in whatever order the continuation or its
own judgement picks:

| Source | Size | How it is read |
| --- | --- | --- |
| `.fleet/GOAL.md` (frozen goal, rules, three owner amendments) | 7.7 KB | `cat`, sometimes `head -60` |
| `.fleet/STATUS.md` (append-only log, 817 lines, five `SNAPSHOT` markers) | 99 KB | `tail -12` … `tail -40`; decisions above the tail are not read |
| `.fleet/HANDOFF.md` | 5.9 KB | named by the skill, last rewritten 2026-10-08, stale |
| `.fleet/CONTINUE.md` (hand-written continuation) | 1.1 KB | pasted as the post-compaction turn |
| `.fleet/supervisor.log` | 2.75 MB | `tail -5` … `tail -25` |
| `tasks.json` (364 tasks) and `agents.json` (120 entries) | 88 KB, 58 KB | ad-hoc `python3 -c` renders |
| `batches/plan`, `batches/*.closed` | small | `cat`, `ls` |
| builder panes | — | `flk pane read` (672 calls over the session) |
| PR state | — | `gh pr view` / `gh pr checks` |
| the `architect` skill | 17.6 KB | loaded once in the whole session, through the Skill tool |

**Builder spawn.** `spawn-claude.sh` starts Claude in a fresh worktree and
types "Read `<brief>` and execute it exactly." The builder then reads the
brief (median 2.3 KB), runs `cat AGENTS.md | head -150`, `gh issue view <n>
--comments`, the issues and ADR files the brief names, and only then the code.

Two facts about that sequence matter for caching:

- **Claude Code already loads `AGENTS.md`** as project instructions (measured
  below: +8,051 tokens of cache creation per session for this repo's file). The
  brief's "Read `AGENTS.md` first" makes every builder load it a second time.
- **The briefs share their rules, but not their bytes.** 120 briefs carry a
  `## Rules` section. They are 110 distinct strings, because each embeds its
  issue number and commit type. With those templated out they collapse to 18,
  and one variant covers 83 of them.

### Cache usage today

Measured with `scripts/research/context_compile/cache_usage.py` (read-only,
reads `usage` from the transcripts). "Units" are input-token equivalents at the
provider's ratios for the TTL the transcripts show (every creation was
`ephemeral_1h`): cache read × 0.1, cache creation × 2, uncached input × 1.
Medians:

| | n | First call: read / created | First 6 calls: read / created | Units (6 calls) | Hit ratio (6 calls) | First 12 calls: read / created / hit ratio |
| --- | --- | --- | --- | --- | --- | --- |
| Architect resume | 8 | 27.0k / 21.5k | 287k / 28.3k | 85.3k | 0.91 | 649k / 34.9k / 0.95 |
| Builder spawn | 16 | 26.3k / 18.6k | 284k / 31.4k | 89.0k | 0.88 | — |

The hit ratio is already high, and it is the wrong number to optimise. Every
call re-reads the whole conversation, so reads dominate the ratio. The cost is
elsewhere:

- **Rehydration turns.** In the eight real resumes, the first action (a
  write, a merge, a dispatch, a rerun) came at call 1–16 after the boundary,
  median 4.5. The calls before it read state, and each of them re-reads the
  full prefix (55k–64k tokens per call after the first).
- **Per-session creation.** Everything after the first byte that differs
  between sessions is created anew. In Claude Code that first byte comes early:
  the working directory sits in the system prompt, so two worktrees share only
  the tool definitions and the static prompt (16k–27k tokens; the shared
  prefix dropped from 27.2k to 16.0k between two spawns on 2026-10-11, with no
  change on our side).
- **Duplicates.** `AGENTS.md` is created twice per builder (harness copy plus
  the brief's read), and the referenced issues and ADRs are created once per
  builder although several builders in a wave read the same ones.

### Harness behaviour, measured

Throwaway `claude -p` runs on Haiku 5.5 (Claude Code 2.1.296) through
`scripts/research/context_compile/probe_claude.sh`, each in a sandbox
directory, with user settings, hooks, MCP servers and skills disabled
(`--setting-sources project --strict-mcp-config --disable-slash-commands
--no-session-persistence`), never touching the live flk server or a running
session. One run per cell; read the numbers as magnitudes:

| Probe | Cache read | Cache created |
| --- | --- | --- |
| Same prompt, same directory, twice (second run) | 28,561 | 0 |
| Same prompt, a second directory with identical content | 18,819 | 9,742 |
| Empty directory, `--exclude-dynamic-system-prompt-sections` | 17,822 | 2,402 |
| Directory holding only this repo's `AGENTS.md`, same flag | 17,822 | 10,453 |
| `--append-system-prompt-file` (11.7k tokens) in a second worktree, **without** the flag | 16,234 | 23,761 |
| The same, **with** `--exclude-dynamic-system-prompt-sections` | 27,961 | 11,845 |

So:

1. Text appended to the system prompt is shared across worktrees **only** when
   the per-machine sections (cwd, env, git status, memory paths) are moved out
   of it with `--exclude-dynamic-system-prompt-sections`. Then it sits right
   after the static prompt and before anything per-session.
2. The harness-loaded `AGENTS.md` block names its absolute path, so it is never
   shared across worktrees, flag or not. It costs about 8k created tokens per
   spawn of this repo, and nothing flk passes can change that.

## Decision

### 1. Five layers, in a fixed order, most stable first

`flk context compile` emits one artifact made of these layers, always in this
order. A layer may be empty; it is never reordered.

| # | Layer | Changes when | Authored or derived | Source of truth |
| --- | --- | --- | --- | --- |
| 1 | **Role**: the role skill (architect, builder, reviewer) and the working principles | a release, or an edit to the skill | authored | builder and reviewer packs ship in flk's release; the architect skill is read from its path and pinned by hash |
| 2 | **Repo**: vision, ADR index, known traps; conventions the harness does not already load | a merge to `dev` | authored in the repo, compiled from `origin/dev` | `docs/adr/README.md`, a repo traps file (§8, owner question 3); `AGENTS.md` only when the harness does not load it itself |
| 3 | **Goal and task**: the frozen goal, owner decisions and the wave structure (which work package holds what, not its progress) for an architect; the issue, the brief's task text and the issues and ADR sections it names for a builder | a decision, or a new task | authored | ADR-0030 goal record (G1, stored per ADR-0031), ADR-0023 decision records; `gh` for issue text |
| 4 | **Snapshot**: wave progress (done counts, open tasks), in-flight delegates and their phase, open PRs and CI, landing queue, red/green, the last N events | any event, including a finished task | derived | the goal's subtask states, `delegate.list` (ADR-0027), `events.read` (ADR-0028), the PR poller's `pr.*` events |
| 5 | **Next action**: one line | any event | derived (§6) | layers 3–4 |

Nothing in layers 4–5 is authored. The hand-appended STATUS.md lines become
unnecessary: the snapshot is recompiled from server state whenever it is
needed, and the history is the event log (ADR-0005), which already outlives
the session. A human-readable archive is a render of that log, not a file an
agent appends to.

### 2. Byte-stability rules

Layers 1–3 must reproduce **byte for byte** when their inputs have not
changed. The compile enforces it:

- no timestamps, durations, ages, counters, PIDs, pane ids or sequence
  numbers in layers 1–3. "3/12 done" is a count of state and allowed;
  "updated 4 min ago" is not;
- every list is sorted by a stable key (task id, issue number, ADR number),
  never by recency or map iteration order;
- inputs are read at a pinned version: layer 2 from `origin/dev` blobs, not the
  worktree; layer 3 from the goal record's version, not "now";
- whitespace and line endings are normalised once, and every layer ends with
  exactly one newline;
- the manifest records the sha256 of each layer and of each cumulative prefix,
  so a consumer (and a test) can tell which layer moved.

The prototype holds this: two compiles of the architect context minutes
apart, while the live fleet appended events and finished tasks, produced
identical layer 1–3 hashes and different layer 4 hashes. That holds only
because wave progress (done counts, open tasks) is in layer 4. The first
version of the prototype kept it in layer 3, where every finished task would
have changed the hash. Two builders for different issues (#1021, #1018)
produced the same layer 1–2 prefix hash.

### 3. Where each layer goes, per harness, and where the cache breaks

A Claude Code session cannot place `cache_control` markers itself. The
breakpoints are therefore placement decisions, verified by measurement:

| Harness | Layers 1–2 | Layers 3–5 at spawn | Layers 3–5 after a compaction or resume |
| --- | --- | --- | --- |
| Claude Code, builder or reviewer | `--append-system-prompt-file <layers 1–2>` together with `--exclude-dynamic-system-prompt-sections` | the first user message (what `delegate start` submits) | SessionStart hook output (`additionalContext`) |
| Claude Code, architect | layer 1 in the appended system prompt; layer 2 with layers 3–5, because an architect lives for days and layer 2 moves with every merge | first user message | SessionStart hook output |
| Codex, opencode | to verify in slice X8 (Codex already reads `AGENTS.md` natively; developer instructions and the opencode plugin are the candidates) | first message | the harness's session-start equivalent, else the agent runs `flk context compile` itself |

- **Breakpoint A** is the end of the system prompt. Everything before it is
  shared by every session of the same role in the same repo, across worktrees.
- **Breakpoint B** is the start of layer 4 inside a compiled message. A
  consumer that controls `cache_control` (an API client, a future harness
  adapter) puts its marker there; the compiled markdown carries a
  `<!-- flk:cache-breakpoint layer=N sha=... -->` line after every layer for
  that purpose.
- **Don't duplicate what the harness loads.** Layer 2 omits `AGENTS.md` for
  Claude Code and Codex. A duplicate inside the cached system prompt is cheap
  to read (measured: same creation, +10k read) but it still takes context
  window, and it gains nothing. (Measured once, in the first probe round of
  this spike; not repeated.)
- **The role survives compaction.** The system prompt is not summarised, so a
  role skill in layer 1 is present verbatim after every compaction. That covers
  the reason behind #1062 ("reload the skill fresh") without a slash command,
  for any architect started this way.
- **The system-prompt file is immutable.** Claude Code records the system
  prompt on the first request and reuses it, but only "until the conversation
  is compacted" (`--system-prompt-snapshot`, default on): compaction renders
  it again from the launch arguments. Rewriting the appended file in place
  would therefore change a running agent's role at its next compaction and pay
  a cold write of the whole system prompt there. So flk writes layer files
  content-addressed (`<state dir>/context/<sha256>.md`), the launch names that
  path, and nothing rewrites it. A new layer 1 reaches a running agent only
  through a deliberate restart or a new session.
- `agent_restart` already preserves `--append-system-prompt[-file]` across a
  restart (`src/agent_restart.rs`). It does **not** know
  `--exclude-dynamic-system-prompt-sections` and refuses to restart an agent
  launched with it ("cannot safely preserve launch option …; unknown option
  arity"). That is #1070, and slice X2 depends on it.

### 4. The command contract

```text
flk context compile --role architect|builder|reviewer
                    [--agent <target>|self]     # whose goal, edges and events (default: self)
                    [--issue <n>] [--brief <path>]   # builder / reviewer task
                    [--pr <n>]                  # reviewer task
                    [--layers 1-5]              # a range, e.g. 4-5 for a cheap re-orientation
                    [--events <n>]              # layer 4 tail, default 20
                    [--format md|json] [--out <dir>]
```

- **Output, `md`:** the layers in order, each followed by its breakpoint
  line. **`json`:** `{ role, layers: [{ n, text, bytes, tokens_est, sha,
  prefix_sha, sources: [{ kind, ref, version }] }], next_action }`. With
  `--out`, one file per layer plus `compiled.md` and `manifest.json`, so a
  spawner can pass `layer1.md` + `layer2.md` as the system-prompt file.
- **Sources** are named in the manifest with the version read (`origin/dev`
  blob ids, goal record version, last event `seq`), so a compiled artifact is
  reproducible and auditable.
- **Reads only.** Compile never writes state. It makes no `gh` call on the
  server's hot path: issue text comes from `gh` on the client side for layer 3,
  and layer 4 comes from cached server state (the PR poller), as ADR-0027 §2
  requires of `delegate.list`.
- **Bounded.** Layer 4 has a row cap (`[context] snapshot_rows`, default 12
  per section, overflow as `+k more: flk delegate list --mine`), the same rule
  as the digest (ADR-0028 §6). The compiled architect context in the prototype
  is about 18k tokens; the open-task listing in layer 3 is the largest part and
  is capped the same way.
- **Exit codes** come from ADR-0029's registry: 0 compiled, 4 unknown agent or
  issue, 12 an unknown `--layers` value, 78 against a server without the
  method. A missing source (no goal armed, `gh` offline) yields an empty layer
  and a `missing` entry in the manifest, never a failure: a resume with less
  context beats no resume.
- **Spawning with it:** `flk delegate start … --context compiled` (and `flk
  agent start --context <role>`) compiles, passes layers 1–2 as the system
  prompt file with the flag above, and submits layers 3–5 as the brief.
- **Resuming with it:** the SessionStart hook (ADR-0027 §4, ADR-0030 §6)
  returns `flk context compile --layers 2-5` (architect) or `--layers 3-5`
  (others) as `additionalContext`, instead of composing goal, checklist, status
  and delegate table separately.

### 5. Builder briefs are compiled the same way

A brief becomes layer 3 only: the task, acceptance criteria, the size budget
and what is out of scope. The rules every builder shares move to layer 1 (one
text per role and repo, not one per brief), and the reading list is inlined:
the issue with its comments, the issues and ADR sections the brief names. N
concurrent spawns in one repo then share layers 1–2 through breakpoint A, and
each creates only its own task.

### 6. The next action is derived, in a fixed order

Layer 5 is the first rule that applies:

1. an open question to the owner (ADR-0028 §8a) → "answer or wait on `<question>`";
2. a delegate in `needs_human` or `blocked` → handle it;
3. a delegate `finished` with a PR and no review → "review PR #n";
4. an approved PR not in the landing queue, and `dev` green → "land #n";
5. a red landing batch or red `dev` → "split / rerun `<batch>`" (spawn freeze);
6. a start intent whose gate opened (ADR-0027 §6) and a free slot under the cap → "start `<name>`";
7. otherwise → "park on your delegates" (ADR-0028 §5), never "poll".

The rule that fired is named in the manifest, so the line is checkable.

### 7. How this fits the neighbouring work

- **#573 / ADR-0030 G6.** The goal re-inject *is* a compile with
  `--layers 2-5`: layer 3 is the goal record and checklist, layer 4 carries the
  delegate table (ADR-0027 §4). G6 and #962 then emit one ordered artifact
  instead of four separately bounded parts. The pre-compaction `GoalStatusSet`
  handshake stays: it is an authored input to layer 3.
- **goal-s1 re-inject and `flock_self_compact`.** The continuation shrinks to
  "context recompiled below; act on the next action", because layers 3–5
  arrive as hook output and layer 1 never left the system prompt. The
  multi-line continuation file (ADR-0030 §6) remains for anything else.
- **#1062.** For an architect started with `--context architect`, reloading
  the skill after a compaction is unnecessary (§3). The opt-in slash command
  stays useful for sessions started without it.
- **#1044.** The heartbeat wake for a long-idle goal agent names `flk context
  compile --layers 4-5`, the cheapest re-orientation, rather than "read
  STATUS.md".
- **#1043.** A forked reviewer inherits the architect's whole conversation
  (210k–637k tokens before each of the eight measured compactions), all read
  from cache on the first call when the fork follows the parent's turn. A
  fresh reviewer spawned with `--context compiled --role reviewer` instead
  reads layers 1–2 from cache and creates only the PR task, so every later
  turn re-reads a prefix of tens of thousands of tokens, not hundreds of
  thousands. #1043 picks between them with its fork measurement; this ADR
  supplies the second option.
- **The architect skill's HANDOFF.md.** Its sections map onto the layers: Goal
  → 3, Environment → 2, In flight / Landing queue → 4, Decisions → 3, Next
  action → 5. Friction and watch items stay authored (layer 3 decisions or
  issues). HANDOFF stops being a file the architect must rewrite before every
  compaction.

### 8. Not decided here (owner questions)

1. **Where layer 1 lives.** Recommended: flk bundles the builder and reviewer
   packs, versioned with the release; the architect skill stays in the skills
   repo and flk reads it by path and pins it by hash in the manifest.
2. **`AGENTS.md` in layer 2.** Recommended: omit it for harnesses that load it
   (Claude Code, Codex) and include it for those that do not.
3. **Known traps.** Recommended: an authored repo file (for example
   `docs/agents/traps.md`), reviewed through PRs like any doc, compiled into
   layer 2. Today they live in briefs, the skill and private memory, and drift.

## Measurements: today vs compiled

**What these numbers are.** Sandbox replays, not production runs. Each cell is
a throwaway `claude -p` run (`probe_claude.sh`, driven by `replay.sh`), three runs per cell on Haiku
5.5 and one Opus 5.5 spot check, each run in a fresh copy of its directory, so
that no run inherits another's per-session cache. The real-transcript data in
Context is the baseline only. No real resume or spawn ran the compiled form.
The compiled side uses exactly the §3 placement:

- **Builder, today:** what `spawn-claude.sh` does: nothing appended, first
  message "Read `BRIEF.md` and execute it exactly", restricted to the reading
  steps and a five-line plan. Briefs #1021 and #1018, in two worktrees.
- **Builder, compiled:** layers 1–2 appended with
  `--exclude-dynamic-system-prompt-sections`, layers 3–5 as the first message
  (issue, comments, named issues and ADR files inlined). Layers 1–2 carry new
  bytes in each round, so the first builder of a round is cold and the second
  builder is warm.
- **Architect, today:** the last real compaction summary (made unique per run)
  plus a CONTINUE.md-style continuation over a read-only copy of `.fleet/`.
  Tools: file reads only, so no `flk pane read` or `gh` calls. That makes
  today's side cheaper than a real resume.
- **Architect, compiled:** the same summary, then layers 2–5 in the message;
  layer 1 alone in the appended system prompt. It is measured twice. **Warm:**
  layer 1 was already read within the 1 h TTL, which is what a real resume
  sees: the session's own calls up to and through the compaction keep its
  system prompt alive. All eight real resumes read their full system prompt
  (27.0k–27.3k) from cache on their first call. **Cold:** layer 1 was never
  seen. That is a resume after more than an hour with no call, or after
  layer 1 changed.

Units as in Context (read × 0.1, creation × 2, uncached input). Haiku: median
(min–max) of 3. Opus: n = 1.

| Scenario | Today | Compiled | Change (median) |
| --- | --- | --- | --- |
| Builder #1021, first of a round (cold layers 1–2), Haiku | 6 turns (6–8), 13.8 s, 64.4k units (42.0k–66.3k) | 1 turn, 4.5 s, 51.1k units (51.1k–51.1k) | −21 % |
| Builder #1018, second of a round (warm layers 1–2), Haiku | 5 turns (5–6), 11.8 s, 61.0k units (55.8k–61.0k) | 1 turn, 5.1 s, 45.1k units (45.1k–83.4k) | −26 % |
| Architect resume → next action, Haiku | 11 turns (9–14), 20.0 s, 84.1k units (54.7k–99.9k) | warm: 1 turn, 8.0 s, 42.3k units; cold: 1 turn, 8.1 s, 58.1k units | warm −50 %, cold −31 % |
| Builder #1021 / #1018, Opus | 9 / 10 turns, 62.9k / 57.6k units, $0.264 / $0.243 | 1 turn, 49.5k / 45.0k units, $0.213 / $0.194 | −21 % / −22 % units, −20 % $ |
| Architect resume, Opus | 13 turns, 67.8k units, $0.269 | warm: 42.2k units, $0.187; cold: 56.5k units, $0.245 | warm −38 %, cold −17 % units |

What the numbers do and do not show:

- **Most of the saving is turns, not cross-worktree sharing.** With
  `AGENTS.md` out of layer 2 (§3), layers 1–2 are about 3.9k tokens: the warm
  builder read 20.1k from cache against 16.2k for the cold one. Each compiled
  builder still creates about 21k–25k tokens: roughly 8k is the harness's own
  `AGENTS.md` block, roughly 7k–7.5k the inlined task (#1001's comments and
  ADR-0029 are long), and the rest the per-session prompt.
- **The architect's warm/cold gap is layer 1:** 8.3k tokens read instead of
  created, about 16k units per resume.
- **Noise.** One compiled warm builder run read nothing from cache (83.4k
  units). The provider does not guarantee a hit, and the median hides that.
  Today's builder runs varied with how much each chose to read (42.0k–66.3k).
- **Opus.** The single Opus spot check points the same way, with somewhat
  smaller percentages than Haiku. Two Opus figures are a direction, not a
  ratio estimate. In dollars the warm architect resume saved 30 %, the cold
  one 9 %.
- **Answer quality.** After the prototype's layer 4 was fixed (in-flight =
  owner of an open task, not reaped, worktree present; landing queue minus
  each batch's `.deferred` members), no compiled architect answer (8 runs)
  named an agent outside the ten that STATUS.md has in flight. They named 5
  to 10 of the ten, because the answers summarise, and every one gave the
  b109/b110 landing state. The
  first version had derived in-flight from task owners alone and listed 42
  agents, because owners are never cleared. Hand-kept files are not a sound
  source for layer 4; `delegate.list` and the event log are. The "today" runs
  read a `.fleet/` copy taken a few minutes after the compile, so one of them
  reported b109 merged where the compiled snapshot still had it open. Both
  answers were right for their inputs.
- **Superseded figures.** The first version of this ADR reported −46 %, −28 %
  to −32 % and −39 % to −69 % from single Haiku runs. Those runs had
  `AGENTS.md` inside the shared layers, used one directory per role (so later
  runs inherited per-session cache), credited a resume-to-resume cache hit
  that §3's placement cannot produce, and compared the builder's first call
  against a layout no builder uses today. The table above replaces them.

## Consequences

- A resume or a spawn starts with one deterministic view, in one turn, instead
  of an agent-chosen sequence of reads (median 4.5 calls before the first
  action in the real resumes, 5–14 turns in the replays).
- Every builder of a repo shares one cached system-prompt tail (small, about
  4k tokens, once `AGENTS.md` is left to the harness); each spawn creates only
  its task and the harness's own per-session block.
- STATUS.md, HANDOFF.md and CONTINUE.md stop being load-bearing. The decisions
  they buried become authored records (goal, ADR-0023 decisions) in layer 3.
- Spawning gains two harness flags for Claude Code. The restart path keeps
  one of them today and must learn the other (#1070).
- An architect's role text is fixed per session, because the system-prompt
  file is content-addressed and never rewritten. A skill update reaches it on
  a restart or a new session, not at a compaction.

## Alternatives considered

- **Keep the agent reading files, just in a better order.** Each read is a
  round trip that re-reads the whole prefix, and the agent still chooses what
  to skip; that is how decisions above the STATUS tail went unread.
- **One compiled message, no system-prompt placement.** Byte-identical layers
  inside the first user message are never shared across worktrees, because the
  per-session block comes first (measured: 16.2k read either way).
- **Put layers 1–3 in the system prompt for the architect too.** The architect
  outlives many merges and decisions; a system prompt fixed at session start
  would carry a stale layer 2 and 3 for days.
- **Teach compaction to keep the files.** The `/compact` instructions already
  say "keep GOAL and STATUS"; the summary still paraphrases them, and the
  architect re-reads them anyway.
- **Rely on a high hit ratio.** It is already 0.88–0.95 and says nothing about
  the rehydration turns or the per-worktree creation, which is where the cost
  is.

## Risks

- **Harness flags can change.** `--exclude-dynamic-system-prompt-sections` and
  the append position are Claude Code behaviour, not a contract. The probe
  (slice X9) re-measures them on every harness update, and the spawn falls
  back to message placement when the flag is missing.
- **A stale snapshot is worse than none.** Layer 4 must come from server
  state only, with the event `seq` it was read at, and the next-action rule
  that fired.
- **Over-inlining.** Inlining every referenced issue can grow layer 3 past what
  the builder needs (issue #1001's comments are long). Layer 3 gets a byte
  budget with section-level truncation that names what it cut.
- **Replays, small samples.** Three Haiku runs per cell, one Opus run per
  cell, one architect session's real transcripts. Nothing has measured a real
  compiled resume or spawn; slice X9 does that. The direction is consistent
  across cells and both models, but the percentages are not tight bounds.

## Test strategy

- **Byte stability:** compile twice with changed layer 4 inputs (new events,
  a new PR) and assert identical layer 1–3 hashes; change one ADR row and
  assert only layer 2 and later prefixes move.
- **Placement:** the sandbox probe (`claude -p`, Haiku, two worktrees, the
  flags above) asserts the second spawn's cache read covers layers 1–2.
- **Next action:** one table case per rule in §6, first match wins.
- **Re-inject:** an unarmed auto-compaction of a sandbox architect yields
  layers 2–5 as hook output and types nothing (shared with ADR-0030 G6).

## 9. Slices

Each is sized at roughly 400 diff lines or less. Not filed; the architect
files them after the owner accepts.

| # | Slice | Depends on | Acceptance |
| --- | --- | --- | --- |
| X1 | `flk context compile` skeleton: layers 1–2, `md`/`json`/`--out`, manifest with per-layer and prefix hashes, byte-stability rules, builder and reviewer packs bundled | — | Two compiles with unchanged inputs are byte-identical; an ADR index change moves only the layer 2 hash |
| X2 | Builder compile (layer 3 inlines the issue, comments, named issues and ADR sections, under a byte budget) and `flk delegate start --context compiled` passing content-addressed layers 1–2 via `--append-system-prompt-file` with `--exclude-dynamic-system-prompt-sections` | X1, #1070 (restart keeps the flag) | Sandbox probe: a second builder in another worktree reads layers 1–2 from cache on its first call; `flk agent restart` of that builder succeeds and keeps both flags |
| X3 | Architect layer 3 from the goal record, decisions and subtasks | X1, ADR-0030 G1 | A goal amendment moves only the layer 3 hash; no timestamps in the layer |
| X4 | Layer 4 from the goal's subtask states, `delegate.list`, `events.read` and the PR poller, row caps, read `seq` in the manifest | X1, #960 (ADR-0027 slice 2), #1028 (ADR-0028 E4a), #1020 (ADR-0028 E7) | A fixture fleet renders the documented sections; no `gh` call on the server path |
| X5 | Layer 5 rules (§6) and the fired rule in the manifest | X4, ADR-0028 E1a–E1c (#1019, #1022, #1023: `phase` and `needs_human`), ADR-0028 E9a (#1038: open questions) | One table test per rule |
| X6 | SessionStart re-inject renders `--layers 2-5` / `3-5` as `additionalContext`, replacing the separate goal, checklist, status and delegate parts | X3, X4, #962, ADR-0030 G6 | An unarmed auto-compaction of a sandbox architect yields the compiled layers and types nothing |
| X7 | Reviewer role: `--role reviewer --pr <n>`, the fresh-spawn option for #1043 | X2, #1043 | A reviewer spawned for a PR starts from cached layers 1–2 and the PR task |
| X8 | Codex and opencode placement verified and wired, or a documented fallback to message placement | X1 | Probe per harness reports the shared prefix on a second spawn |
| X9 | Promote the measurement scripts to `tests/probes/context_cache.py` (before/after runbook, harness-version check) | X2 | Runs against two flk builds and prints the table in this ADR |
| X10 | Retire the fleet's own STATUS.md appends, HANDOFF rewrite and CONTINUE.md; update the architect skill §7 to the compiled resume | X6, X11 | A full resume of the fleet's architect needs no `.fleet` file reads |
| X11 | Wakes and continuations point at the compile: the #1044 heartbeat names `flk context compile --layers 4-5`; the `flock_self_compact` continuation shrinks to "act on the next action"; #1062's slash-command continuation documented as the path for sessions started without `--context` | X5, X6, #1044, #1062 | A heartbeat wake and a self-compaction of a sandbox architect each carry the compile hint, and the architect's next turn reads no `.fleet` file |
