# ADR 0019 — Agent mail may also arrive as a Claude Code channel push, as a first knock over the pull that stays the source of truth

- Status: Proposed
- Date: 2026-09-30
- Issues: #438 (the spike this records). Related: #412 (idle wake), #413
  (Stop-hook nudge), #415 (the nudge's race), #416/#417 (sender attestation).
- Amends: ADR-0018 §2 (how an idle agent is reached) and, for sessions that
  opt in, when a busy agent hears about mail. ADR-0008's split — content is
  pulled through the tool, the inbox is the source of truth — is unchanged.
- Decision owner: operator.

## Context

Delivery today is a pull with two knocks. The Stop hook tells an agent at its
turn boundary that N messages are waiting (#413), and an idle pane is woken by
flock typing a constant sentence into its PTY (ADR-0018 §2, #412). The agent
then calls `flock_msg_read`. Every message therefore costs an extra model round
trip, a busy agent hears nothing until its whole turn ends, and the idle wake
refuses a pane whose prompt holds an operator's draft.

Claude Code **channels** (research preview) let an MCP server push an event
into a running session: the server declares
`capabilities.experimental['claude/channel']` and emits
`notifications/claude/channel { content, meta }`, which the model sees as a
`<channel source="flock" …>` tag. `flk mcp serve` is already a per-session
stdio MCP server running inside the recipient's pane, so it is the natural
place to push from. The spike built that behind `[msg] channel_push`
(default off) and measured it against the two existing knocks.

## What was measured

All on sage, Claude Code 2.1.281, Opus 5.5, in an isolated flk instance (a
sandbox `XDG_CONFIG_HOME`, never the operator's server). T0 is stamped in
the sender's pane immediately before `flk msg send`. "working" is flk's own
screen detector seeing the recipient start a turn, so it includes detector
latency. Samples are small. Read these as orders of magnitude, not as
benchmarks.

| Case | n | push emitted | turn starts | `flock_msg_read` | reply sent |
| --- | --- | --- | --- | --- | --- |
| Idle, attested sender, body pushed | 5 | 25 ms (18–133) | 182 ms (60–327) | not needed | **2.03 s** (1.95–4.93) |
| Idle, unattested sender, doorbell | 3 | 34 ms (28–35) | 324 ms (193–362) | 1.73 s (1.73–1.89) | — |
| Idle, flag off: typed idle wake (baseline) | 3 | — | 324 ms (281–652) | 1.94 s (1.71–4.02) | **3.23 s** (3.15–5.99) |
| Busy: 25 s tool call, mail arrives 9 s in, push | 1 | 118 ms | (already working) | not needed | **21.2 s** — right after the tool returned |
| Busy, flag off: Stop-hook nudge (baseline) | 1 | — | (already working) | 23.2 s | **24.6 s** — after the turn ended |
| Flag on, session did NOT register the channel | 3 | 25 ms, dropped | 2.44 s (2.44–2.53) via idle wake | 4.56 s | — |
| After `flk server live-handoff` | 1 | 139 ms | — | not needed | 2.19 s |

Each row is median (min–max). Other findings:

1. **An idle session starts a turn on a push.** The pane shows
   `← flock: <first line of the body>` (or the doorbell sentence), and the
   model replies through `flock_msg_reply` without reading first.
2. **A busy session gets the push at its next tool boundary, not at the end
   of its turn.** Claude Code queues the event while a tool runs and hands it
   to the model with that tool's result, in the same turn. It never
   interrupts a running tool. It does not wait for the turn to end either, so
   under a long turn the Stop hook arrives minutes later than a push. The
   issue read the docs' "delivered together on the next turn" as the turn
   boundary, and this measurement corrects that.
3. **A push reaches a pane with an operator's draft in its prompt.** The turn
   starts and the draft stays in the input box. The idle wake refuses this
   pane (`prompt_not_empty`), so today it waits for the Stop hook.
4. **The server cannot tell whether it was registered.** The `initialize`
   request, `notifications/initialized` and the first requests after it are
   byte-identical with and without `--dangerously-load-development-channels`
   (client capabilities: `roots`, `elicitation`, and nothing channel-shaped).
   An unregistered session drops the push silently, as the docs say.
5. **Push and idle wake race without a grace window.** Both fire on enqueue.
   In the first probe the idle wake typed its sentence, the push's turn
   started, and the idle wake then withheld its Enter as `not_idle`, which
   left the sentence sitting in the prompt. The prototype holds the idle wake
   off for `channel_push_idle_wake_grace_ms` (2000 ms). After that, all later
   trials were clean.
6. **The startup gate is a dialog on every launch.** With
   `--dangerously-load-development-channels server:flock`, Claude Code shows a
   full-screen "WARNING: Loading development channels" with
   `❯ 1. I am using this for local development` preselected. One Enter passes
   it. It is not remembered: a second launch in an already-trusted folder
   shows it again (the folder-trust dialog, by contrast, is remembered).
   While it is up, flk's detector reports the pane `unknown` with no agent,
   so `flk agent start --wait-ready` would wait on it.
7. **No settings route on this account.** A session-only plugin
   (`--plugin-dir`) passed to `--channels plugin:flock-channel@inline`
   registers nothing: the startup notice says "plugin not installed" and "not
   on the approved channels allowlist". `allowedChannelPlugins` given through
   `--settings` is ignored. The docs confirm it is a managed setting for Team
   and Enterprise orgs, and that "Pro and Max users without an organization
   skip these checks entirely". For a custom server on a Max account, only the
   development flag works.

## Decision (proposed)

### 1. The push is an additional first knock, never a replacement

With `[msg] channel_push = true`, `flk mcp serve` declares
`claude/channel` and emits one `notifications/claude/channel` per arriving
message. The inbox stays the source of truth. A push is never an
acknowledgement, because Claude Code acknowledges nothing and drops the event
silently for a session that did not register the channel (finding 4). So
nothing about a push marks a message delivered. `flock_msg_read` does, and so
does `flock_msg_reply` naming a still-queued message in the replier's **own**
inbox (the replier is resolved from process ancestry). The reply is the only
acknowledgement a pushed body gets, so it counts only once it has actually
gone out. A reply queued locally or relayed over a peer's ssh settles the
original at once. A reply handed up to the hub settles it only when the hub
answers that it delivered the reply. A refusal, a timeout or any failed reply
leaves the original unread, and the wakes keep knocking for it.

The Stop-hook nudge and the idle wake keep running unchanged underneath.
Because the server cannot detect registration (finding 4), there is no
per-session choice between push and fallback. Instead the three knocks are
ordered:

1. the push, on enqueue;
2. the idle wake, no earlier than `channel_push_idle_wake_grace_ms` after the
   newest unannounced message. A push that started a turn has left the agent
   non-idle by then, so nothing is typed (finding 5);
3. the Stop-hook nudge, at the turn boundary, for anything still unread.

A session that did not register the channel therefore still gets its idle
wake, `grace` later (finding 4's row: 2.44 s instead of 0.32 s).

### 2. The push asks the wake question

Before each push the server asks `msg.wake`, the one decision the other two
knocks already use. A muted recipient, a paused fleet and an `fyi`-only
inbox are pushed nothing, and ADR-0018 §1's reply rule applies unchanged. An
`fyi` still never costs a turn.

### 3. What rides the push

- **Body verbatim** only when the receiving server attested the sender from
  process ancestry (a local pane) **and** the body is at most
  `channel_push_body_max_bytes` (4096). It arrives inside a `<channel
  source="flock" kind="message">` tag, labelled as another agent's words by
  both the tag and the server's `instructions`. It is not a user turn, and it
  is never typed.
- **Otherwise a count-only doorbell**: ADR-0018 §2's constant, verbatim.
  Every relayed (cross-host) message is unattested on the receiving server,
  so cross-host mail always gets a doorbell and never a pushed body.
- **Meta**: `kind`, `from_agent`, `from_host`, `intent`, `correlation_id`,
  `replyable`, `unread`. The keys are identifiers, because Claude Code drops
  any other key silently. A value that is not id-shaped is dropped rather than
  escaped, so a sender-minted correlation id cannot smuggle prose into an
  attribute.
  On a doorbell for a relayed message, `from_agent` and `from_host` are the
  relay's **claim**, not an attestation. The ingress check guarantees they are
  id-shaped, which keeps the risk low, but a reader must not take them as
  proof of who sent the message. Only a pushed body implies an attested
  sender.
- **No permission relay.** flock does not declare
  `claude/channel/permission`. Doing so would let whoever can reach the
  channel approve tool calls.

### 4. The server's live flag governs, not the session's snapshot

`flk mcp serve` reads `channel_push` once, at startup, because it must
declare the capability in its `initialize` answer. Claude Code fixes the
capability for the session. The server, meanwhile, reads the flag live for
the idle wake's grace. So that the two cannot drift apart, every `msg.wake`
answer carries the server's current `channel_push`, and the session pushes
only while it is true. Turning the flag off with a config reload therefore
stops every push immediately, on the same reload that removes the grace.
Turning it on reaches only sessions started afterwards.

### 5. The feed is event-driven

`flk mcp serve` opens one `events.subscribe` with a new `msg.queued { pane }`
subscription for its own pane, resolved the way `msg.read` resolves it (the
`msg.wake` response now names the pane). On the server, a stream made only of
hub-driven subscriptions sleeps on a condvar the event hub notifies on every
push, so delivery costs a notify rather than a poll tick. The 100 ms tick
remains only as the bound for noticing a hung-up client. Nothing is spawned
per message. The feed re-attaches `channel_push_reconnect_secs` (at least one second)
after the socket goes away, and it survived a live handoff (last table row).
A pushing `flk mcp serve` writes its own `flock-mcp-<pid>.log`, so a feed that keeps
failing is visible. With the flag off it still logs nothing.

## Consequences

**Gained, for a session that opted in.**

- An idle recipient answers about 1.2 s sooner, because it skips the
  `flock_msg_read` round trip.
- A busy recipient hears at its next tool boundary instead of at the end of
  its turn. The gain is the rest of the turn, which is unbounded under a long
  turn.
- A pane with an operator's draft in its prompt becomes reachable without
  flock typing anything.
- Unlike the idle wake, the push does not depend on screen detection being
  fresh, on the Stop hook being declared, or on the prompt box being empty.

**Given up, or not solved.**

- **The launch gate.** On a Max account a custom channel needs
  `--dangerously-load-development-channels`, and its dialog needs a keypress
  on every launch (findings 6 and 7). Getting `flk agent start` past it
  unattended means a new detector for that dialog plus one flock-authored
  Enter, only when flock itself put the flag on the command line. That is a
  narrow cousin of the idle wake's keystroke and would need its own review.
  The alternatives are outside flock: an Anthropic allowlist listing, or a
  Team/Enterprise org's managed `allowedChannelPlugins` naming an installed
  marketplace plugin. Neither was testable here.
- **Research preview.** The flag, the capability key and the notification
  contract may change. The docs also warn that MCP protocol negotiation
  `auto` against revision 2026-07-28 stops channel delivery. Everything here
  must stay optional and must degrade to the pull.
- **Mid-turn delivery is new.** ADR-0018 never delivered inside a turn. The
  push does, at a tool boundary, the same place a message the operator types
  mid-turn lands. It never interrupts a running tool.
- **The push does not honour the operator-quiet window.** It types nothing,
  so it cannot corrupt a draft (finding 3), but it can start a turn under an
  operator who is composing.
- **Two seconds more for sessions that did not opt in,** once a host sets
  the flag, because the grace delays their idle wake. The grace is tunable,
  and it is zero cost for a session that registered.
- **The body reaches the model before any tool call.** This widens what
  ADR-0008 let into a session: a sender's words, labelled and tagged as
  such, rather than a count. It is limited to senders this server attested
  and to bodies under the cap. The typed idle wake still carries only the
  constant. Pushing a relayed sender's body would need an attestation that
  survives the relay, and that is out of scope here.

## Alternatives considered

- **Replace the Stop hook and the idle wake with the push.** Rejected. The
  server cannot tell whether the session registered the channel (finding 4),
  so a replacement would turn every unregistered session into silence. That
  is the failure ADR-0018 was written to end.
- **Pick push or fallback per session from the handshake.** Impossible. There
  is no signal (finding 4).
- **Learn registration after the fact.** A pushed session that starts a turn
  has shown it registered, and the grace could then be skipped for it. This
  adds state for a 2 s saving that only unregistered sessions pay, so it is
  deferred.
- **Push the body for every sender.** Rejected. Relayed identity is a claim,
  and the channels docs name an ungated channel a prompt-injection vector.

## Recommendation

**Go, as an opt-in, per host, additive layer.** The prototype keeps every
existing guarantee, and the latency wins are real, largest for busy agents.
**No-go on making it the default, or on removing either existing knock**,
until the launch gate has an unattended answer that the operator accepts and
channels leave research preview.
