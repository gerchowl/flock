---
name: flock
description: "Control flock from inside it. Manage workspaces and tabs, split panes, spawn agents, read output, and wait for state changes — all via CLI commands that talk to the running flock instance over a local unix socket. Use when running inside flock (FLOCK_ENV=1)."
---

# flock — agent skill

before using this skill, check that `FLOCK_ENV=1`. if it is not set to `1`, say you are not running inside a flock-managed pane and stop. do not inspect or control the focused flock pane from outside flock.

you are running inside flock, a terminal-native agent multiplexer. flock gives you workspaces, tabs, and panes — each pane is a real terminal with its own shell, agent, server, or log stream — and you can control all of it from the cli.

this means you can:

- see what other panes and agents are doing
- create tabs for separate subcontexts inside one workspace
- split panes and run commands in them
- start servers, watch logs, and run tests in sibling panes
- wait for specific output before continuing
- wait for another agent to finish
- spawn more agent instances

the `flk` binary is available in your PATH. its workspace, tab, pane, and wait commands talk to the running flock instance over a local unix socket.

if you need the raw protocol or full api reference, read the [socket api docs](https://flock.dev/docs/socket-api/).

## concepts

**workspaces** are project contexts. each workspace has one or more tabs. unless manually renamed, a workspace's label follows the first tab's root pane — usually the repo name, otherwise the root pane's current folder name.

**tabs** are subcontexts inside a workspace. each tab has one or more panes.

**panes** are terminal splits inside a tab. each pane runs its own process — a shell, an agent, a server, anything.

**agent status** is detected automatically by flock. the api exposes one public field for it:

- `agent_status` — `idle`, `working`, `blocked`, `done`, `unknown`, `hibernated`

`done` means the agent finished, but you have not looked at that finished pane yet.

`idle` is "ready for input", and it means `done` too — an agent nobody is
watching goes quiet as `done`. `hibernated` means the agent process is gone and
a resume plan is stashed on the pane.

plain shells still exist as panes, but flock's sidebar agent section intentionally focuses on detected agents rather than listing every shell.

**ids** — workspace ids look like `1`, `2`. tab ids look like `1:1`, `1:2`, `2:1`. pane ids look like `1-1`, `1-2`, `2-1`. these are compact public ids for the current live session.

important: ids can compact when tabs, panes, or workspaces are closed. do not treat them as durable ids. re-read ids from `workspace list`, `tab list`, `pane list`, or create/split responses when you need a current id. do not guess that an older `1-3` is still the same pane later.

## discover yourself

see what panes exist and which one is focused:

```bash
flk pane list
```

the focused pane is yours. other panes are your neighbors.

list workspaces:

```bash
flk workspace list
```

## tab management

list tabs in the current workspace:

```bash
flk tab list --workspace 1
```

create a new tab:

```bash
flk tab create --workspace 1
```

without `--label`, the new tab keeps the default numbered tab name.

create and name it in one step:

```bash
flk tab create --workspace 1 --label "logs"
```

rename it:

```bash
flk tab rename 1:2 "logs"
```

focus it:

```bash
flk tab focus 1:2
```

close it:

```bash
flk tab close 1:2
```

## read another pane

see what is on another pane's screen:

```bash
flk pane read 1-1 --source recent --lines 50
```

- `--source visible` = current viewport
- `--source recent` = recent scrollback as rendered in the pane
- `--source recent-unwrapped` = recent terminal text with soft wraps joined back together

## split a pane and run a command

split your pane to the right and keep focus on your current pane:

```bash
flk pane split 1-2 --direction right --no-focus
```

that prints json with the new pane nested at `result.pane.pane_id`. parse that value, then run a command in that pane:

```bash
NEW_PANE=$(flk pane split 1-2 --direction right --no-focus | python3 -c 'import sys,json; print(json.load(sys.stdin)["result"]["pane"]["pane_id"])')
flk pane run "$NEW_PANE" "npm run dev"
```

split downward instead:

```bash
flk pane split 1-2 --direction down --no-focus
```

## wait for output

block until specific text appears in a pane. useful for waiting on servers, builds, and tests.

for `--source recent`, matching uses unwrapped recent terminal text, so pane width and soft wrapping do not break matches. `pane read --source recent` still shows the pane as rendered. if you want to inspect the same transcript that the waiter matches, use `pane read --source recent-unwrapped`.

```bash
flk wait output 1-3 --match "ready on port 3000" --timeout 30000
```

with regex:

```bash
flk wait output 1-3 --match "server.*ready" --regex --timeout 30000
```

if it times out, exit code is `1`.

## wait for an agent status

block until another agent reaches a specific status:

```bash
flk wait agent-status 1-1 --status idle --timeout 60000
```

`--status idle` means ready for input and matches `done` as well, so it is the
one to use for an agent nobody is watching. `--status done` means exactly `done`.
`flk agent wait <target> --status <status>` takes the same seven statuses with
the same meanings.

## wait for an agent to go quiet

`--status settled` is the one for coordinating with an agent you prompted.
`settled` is **observed quiescence**: after a turn cursor, the agent entered
`working` and then held `idle`/`done` with no state transition at all for the
settle window (5000 ms by default). it is not proof the turn produced a result —
for the turn's output use `flk agent result` (once #575 lands).

capture the cursor **before** you prompt. without it any quiet counts, so an
agent that is already idle settles after one settle window:

```bash
c=$(flk agent get worker | jq -r .result.agent.turn_cursor)
flk agent send worker "review the auth module"   # submits; no Enter needed
flk agent wait worker --status settled --after "$c" --timeout 3600000
```

`--settle MS` sets the window (default 5000; `--settle 0` settles on the sample after the first qualifying one) and `--after CURSOR` comes from `flk agent get`,
`flk agent list` or `flk pane get` (all the same string). `flk wait agent-status
1-1 --status settled` is the same signal if that is the verb you already have.

exit codes: `0` settled · `6` blocked (a human has to look; `3` before #1025) ·
`4` pane gone (the line says whether it was closed, hibernated or restarted) ·
`124` timeout · `2` a usage error or a refused cursor. on `0`, `6` and `4` it
prints one json
object:

```bash
flk agent wait worker --status settled --after "$c"   # {"status":"settled","pane_id":"1-2",...}
```

two limits worth knowing: flock samples a native agent's screen every 300-500 ms,
so a turn whose working phase is shorter than one sample is never seen working,
and a cursor taken before one waits for the *next* turn. and a cursor never
satisfies a wait on another terminal or another agent that was restarted in the
same pane — those are refused or reported as gone, never as settled.

## wait for the answer to a message you sent

ask another agent something and block until it answers:

```bash
flk msg send 1-1 "which branch should I base on?" --intent needs-reply --await --timeout 600000
# or, holding the id yourself:
id=$(flk msg send 1-1 "which branch?" --intent needs-reply | jq -r .result.correlation_id)
flk wait reply "$id" --timeout 600000
```

stdout is the reply body alone. Exit `0` replied, `3` the recipient is muted (its deferral is printed) or the message was dropped unread, `124` timed out (`--json` shows whether it was at least read). Run it as a background task to be woken by the answer instead of polling `flk msg status`. It works from outside any pane too (an ssh shell): the reply is held for you under the correlation id.

## send a prompt to an agent

`agent send` types the text **and submits it**. do not follow it with
`pane send-keys … Enter`: that is a second Enter, and it lands on whatever the
agent shows next.

```bash
flk agent send worker "review the auth module"
```

it needs an idle agent with an empty composer, presses Enter 120 ms after the
paste, retries only that Enter once, and confirms that a turn started. a long
paste that the agent folds into a chip (`[Pasted text #1]`,
`[Pasted Content 1099 chars]`) counts as the sent text. it prints json; branch
on the exit code, and read `result.outcome` and `result.reason` for detail:

- `0` — `accepted` or `observed_accepted`: the turn started.
- `8` — `unconfirmed`: flock could not see the turn start. with reason
  `typed_not_submitted` the text is still sitting in the composer; any other
  reason leaves the composer state unknown. read the pane
  (`flk pane read <pane> --source recent --lines 20`) before you send again,
  or you will submit it twice.
- `9` — `abandoned`: something changed before the Enter (operator input, a
  dialog). nothing was submitted and the composer is untouched; send again.
- refused before typing anything (`not_settled`, `input_not_empty`,
  `operator_active`, `injection_pending`, …): nothing changed. the error json
  on stderr names the code. this exits `1` today and `10` once #1025 lands.
  wait for the agent (`flk agent wait worker --status idle`) and send again.

`--no-submit` types only, without Enter, and reports `delivered` or
`unconfirmed` (exit 8). to message another agent rather than prompt it, use
`flk msg send` (see above): it lands in the agent's inbox and wakes it.

## type into a terminal or press keys

send text without pressing Enter, for shells and other non-agent terminals:

```bash
flk pane send-text 1-1 "hello from claude"
```

press real keys — answer a dialog, cancel, move a selection:

```bash
flk pane send-keys 1-1 Esc
flk pane send-keys 1-1 Down Enter     # pick the second option of a menu
```

do not use `send-keys … Enter` to submit a prompt to an agent; use
`agent send`, which confirms the submit.

`pane run` types a shell command and then presses a real `Enter`:

```bash
flk pane run 1-1 "echo hello"
```

it confirms nothing, so it is for shells, not agents. the text and the Enter
are deliberately separate writes with a short pause between them. a
program reading raw stdin sees read boundaries, not keystrokes, and every agent
tui uses those boundaries to tell typing from pasting — text and a carriage
return arriving in one read look like a pasted block containing a newline, so
the newline gets inserted and nothing is submitted. the pause is a heuristic,
not a guarantee: a tui that has not started reading stdin yet misses the text
as well as the enter. wait for the pane to be ready first (see below).

## restart your own process and carry on

```sh
flk agent restart self --reason "reload MCP configuration" --continue-with "finish the assigned task"
```

The call schedules the restart and returns immediately. Finish your turn
normally. Flock waits for settled idle, stops your process tree through the
runtime, then resumes the same native session with the original launch flags,
profile environment, cwd, name and agent identity. It verifies a fresh report of
the same session and an empty idle prompt before queuing the continuation through
your inbox wake path. Read that inbox when prompted.

Omit `--continue-with` to receive “You restarted yourself for: <reason>. Continue
your task.” Use `--continue-with @FILE` for a file containing one line of plain
instructions, at most 16 KiB. A duplicate pending restart is refused.

MCP restart requests are self-only. A configured hard limit can force a restart
mid-turn; grace expiry still waits for operator quiet and identifies completed
turns that could not reach settled idle because of background activity. The
continuation warns about potentially incomplete tools, unconfirmed transcript
flushes and stopped background tasks/processes. Verify those before continuing.
Unknown startup dialogs need the operator. Explicit requests at the configured
cap are refused with `restart_rate_limited` and a retry-after, leaving you running.
Only forced hard-limit loops stop without resuming; their reports say
`stop_only: true`. An explicit `flk agent resume <target>` releases that stop.
Failures retain a retry plan and release the restart lock. Server handoff cancels
pending/verifying requests and reports it in your inbox and pane notification;
request a new restart after handoff if it is still needed.

## compact your own context and carry on

use this when your context is filling up **and you already know what the next
stretch of work is**. it is the difference between finishing unattended and
handing a human a chore.

```bash
flk pane arm-self-compact "pick up at step 3: the refactor is done, \
run just check, then open the PR and watch CI"
```

what happens, in order:

1. the call **stores** that prompt and returns. nothing is typed. you are
   mid-turn right now, and your harness cannot compact inside a turn.
2. you finish the turn normally.
3. once your pane is idle and settled and nobody has touched the keyboard,
   flock types `/compact` and submits it.
4. when your harness reports the compaction back, flock types your handoff
   prompt in and submits it — and that is your next turn.

to steer what the compaction's summary keeps, pass instructions too. flock
types them after the command, as `/compact <instructions>`:

```bash
flk pane arm-self-compact \
  --instructions "keep GOAL.md, the STATUS.md next step, open PRs, standing rules" \
  "pick up at the STATUS.md next step"
```

`--instructions-file <path>` reads them from a file instead. one line, plain
text, at most 500 bytes: claude code collapses a longer paste, and a collapsed
`/compact` may not run. name a file if there is more. claude code only, like
the rest of this.

write the prompt as instructions to your own next self. assume nothing
survives but the compaction: name what to check first, what not to redo, and
where you left off. if the detail is long, put it in a file and name the file.

important:

- you are arming, not compacting. the call returning `armed` means nothing has
  happened yet.
- flock never types over a human. if someone is at the keyboard when your turn
  ends, it waits.
- if a compaction is already armed, yours is **refused**, not merged. the
  prompt already stored is untouched. to replace it:
  `flk pane arm-self-compact --abort`.
- claude code only. other harnesses are refused by name, because `/compact` is
  a claude code affordance and typing it anywhere else would just leave a
  stray line in your prompt box.
- if the harness never reports the compaction back, the arming is dropped
  rather than left to fire later. nothing was resumed.

over mcp the same thing is the `flock_self_compact` tool, if your harness has
flock's mcp server.

## workspace management

create a new workspace:

```bash
flk workspace create --cwd /path/to/project
```

without `--label`, the new workspace keeps the default cwd-based name.

create and name one in one step:

```bash
flk workspace create --cwd /path/to/project --label "api server"
```

create one without focusing it:

```bash
flk workspace create --no-focus
```

focus a workspace:

```bash
flk workspace focus 2
```

rename:

```bash
flk workspace rename 1 "api server"
```

close:

```bash
flk workspace close 2
```

## close a pane

```bash
flk pane close 1-3
```

## recipes

### run a server and wait until it is ready

```bash
NEW_PANE=$(flk pane split 1-2 --direction right --no-focus | python3 -c 'import sys,json; print(json.load(sys.stdin)["result"]["pane"]["pane_id"])')
flk pane run "$NEW_PANE" "npm run dev"
flk wait output "$NEW_PANE" --match "ready" --timeout 30000
flk pane read "$NEW_PANE" --source recent --lines 20
```

### run tests in a separate pane and inspect the result

```bash
flk pane split 1-2 --direction down --no-focus
flk pane run 1-3 "cargo test"
flk wait output 1-3 --match "test result" --timeout 60000
flk pane read 1-3 --source recent --lines 30
```

### get the result of a delegated task

```bash
flk agent result reviewer            # the reply its newest turn ended on
flk agent result reviewer --offset 4000   # the next page of a long report
```

works for claude and opencode agents. `status` is `done` / `blocked` / `verdict` when the reply's last line is `DONE: …` / `BLOCKED: …` / `VERDICT: …` (with the rest in `status_text`), so ask delegated agents to end that way. `finished: false` means it is still working and the text is its previous reply.

### check what another agent is working on

```bash
flk pane list
flk pane read 1-1 --source recent --lines 80
```

### watch another pane robustly

use this pattern when you need to coordinate with a sibling pane:

```bash
# inspect what is already there
flk pane read 1-3 --source recent --lines 40

# wait only for the next output you expect
flk wait output 1-3 --match "ready" --timeout 30000

# if you need to inspect the same transcript the waiter matched,
# read the unwrapped recent text directly
flk pane read 1-3 --source recent-unwrapped --lines 40
```

### spawn a new agent and give it a task

```bash
# prints the agent as json once it is up; read result.agent.pane_id from it
flk agent start reviewer --cwd /path/to/project --wait-ready -- claude
flk agent send reviewer "review the test coverage in src/api/"
```

`--wait-ready` returns once the pane reports an agent status other than
`unknown`, and prints the agent — so `agent_status` tells you whether it is at
its prompt (`idle` / `done`) or already stuck on a first-run dialog
(`blocked`). matching on prompt text instead is what makes this fragile: an
agent tui paints nothing recognisable for its first 30-60 seconds, so a match
that has not arrived is indistinguishable from a crash.

### coordinate with another agent

```bash
flk agent wait worker --status settled --after "$c" --timeout 120000
flk agent result worker          # once #575 lands
```

## notes

- `workspace list`, `workspace create`, `tab list`, `tab create`, `tab get`, `tab focus`, `tab rename`, `tab close`, `pane list`, `pane get`, `pane split`, `wait output`, and `wait agent-status` print json on success.
- `pane read` prints text, not json.
- `pane read --format ansi` or `pane read --ansi` returns a rendered ANSI snapshot for TUI feedback loops.
- `pane read --source recent-unwrapped` is useful when you want to inspect the same unwrapped transcript that `wait output --source recent` matches against.
- `agent send` prints JSON with the submit outcome; unconfirmed exits 8 and abandoned exits 9.
- `pane send-text` and `agent send --no-submit` print JSON with the delivery outcome and evidence level; unconfirmed delivery exits 8.
- `pane send-keys` and `pane run` print nothing on success.
- parse ids from `workspace create`, `tab create`, and `pane split` responses when you need new ids. `workspace create` returns `result.workspace`, `result.tab`, and `result.root_pane`. `tab create` returns `result.tab` and `result.root_pane`. for `pane split`, the new pane id is at `result.pane.pane_id`.
- use `pane read` for current output that already exists. use `wait output` for future output you expect next.
- `--no-focus` on split, tab create, and workspace create keeps your current terminal context focused.
- without `--label`, workspace create keeps cwd-based naming and tab create keeps numbered naming.
- `--label` on tab create and workspace create applies the custom name immediately.
- if you are running inside flock, the `FLOCK_ENV` environment variable is set to `1`.
