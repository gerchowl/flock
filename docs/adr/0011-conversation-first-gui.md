# ADR 0011 — A conversation-first GUI: surfaces, transports, and the write model

- Status: Proposed
- Date: 2026-08-11
- Issues: #150 (native client bridge, deferred by ADR-0001); #128/#129/#130
  (structured features it unblocks). New work: GUI client, use-case layer,
  agent-authored artifacts.
- Decision owner: operator; revised across three review rounds (architecture,
  security, data-model generality).
- Companion: **ADR-0012** owns the conversation read model and its index. This
  ADR owns surfaces, transports, authorization, and the write model.

## Context

We want a device-independent surface richer than a terminal: a standalone
desktop app, the same app from a phone over the tailnet, agent-authored result
pages, structured question forms, and searchable history.

Today there is no web *application* to extend. `flk web` is a terminal mirror
(`src/web/mod.rs:6`), so every "web feature" is a TUI feature seen through a
pipe. ADR-0001 §3 accepted that for v1 and named the exit: #150, *"what unblocks
structured features (#128/#129/#130)"*.

**This ADR treats the GUI as greenfield, not a port.** The TUI renders a
character grid; the GUI renders a conversation. "Parity" is the wrong success
criterion.

**A real RPC surface exists for actions.** `src/api/schema.rs` (2,883 lines)
carries **73** `Method` variants — agent lifecycle, messaging, workspaces,
worktrees, pane I/O — plus a push event stream (`api/event_hub.rs`) including
`PaneAgentStatusChanged` (`schema.rs:1159`). Actions are already agent-agnostic,
which is why the write model needs far less new design than the read model.

## Threat model

The control plane is **shell-equivalent**: any surface reaching it can call
`PaneSendText`/`PaneSendKeys`/`PaneSendInput` (`schema.rs:771,777,783`),
`AgentStart`, `WorktreeCreate`. Authorization drift is RCE.

In scope: an attacker on the operator's tailnet (user sharing, node sharing and
ACL federation put third-party devices there — reachability is not
authentication); a compromised process at the operator's UID (`flock.sock` is
`0o600`, `server/socket_paths.rs:12`, applied `api/server.rs:73`, so the OS-user
boundary holds but everything inside it is a peer, **including the ability to
spoof the identity header** — Decision 4); a prompt-injected agent producing
hostile artifacts or hostile `AskUserQuestion` payloads; the desktop app's
update channel.

Out of scope while Decision 4 holds: an attacker on the public internet.

Assumed: **one operator per `flock.sock`**. Relaxing this requires an ADR
extension, not a config change. ADR-0012 carries the transcript-content threats.

## Decision

### 1. Reads come from the conversation; writes go through the API

- **Read model** — a canonical conversation served through a transcript API.
  Its shape, sourcing, and index are **ADR-0012**.
- **Write model** — the existing JSON-RPC contract. Every action goes through
  it, and a feature lands in the API before any surface uses it.

TypeScript types are generated from `api/schema.rs` (`ts-rs` or
`schemars`→OpenAPI); no WASM — its only motivation would be reusing flock's VT
parser client-side, and xterm.js already does that. `Request` uses
`#[serde(flatten)]` over an adjacently-tagged `Method` enum (`schema.rs:5–14`),
which neither generator renders as a clean discriminated union, so a small
hand-written top-level wrapper is expected and ~a dozen types outside
`schema.rs` need derives.

**Explicit non-goals** — no API equivalent, not attempted, carried by the
terminal view (Decision 7): copy-mode/selection/scrollback
(`app/input/copy_mode.rs`), navigator and group collapse
(`app/actions.rs:305–1358`), mouse drag-resize (`app/input/mouse.rs`), the
keybinding/intent map (`protocol/wire.rs:116`), the in-TUI settings pane
(`app/input/settings.rs`). Config read/write and notification-ack are
*deferred*, not non-goals.

### 2. A use-case layer owns composite workflows; one applier owns their events

```text
src/worktree.rs, git helpers, …              primitives (already shared)
src/usecase/                                 composite workflows (new)
  ├─ app/actions.rs · app/api/*.rs · cli/*.rs   adapters
```

Primitives are already shared — `src/worktree.rs` holds `planned_action`,
`classify_kill_tier`, `explain_worktree_add_failure` (`:192`). Orchestration is
not: `resolve_worktree_source` exists only in `app/api/worktrees.rs:763` while
the TUI resolves its own way. Two paths, two error semantics; a GUI makes three.

A use-case takes typed input, depends on traits (`GitRunner`, `FileSystem`,
`PaneSpawner`), and returns `Result<Outcome>`.

**Use-cases do not emit events.** Payloads are built from App-owned identity —
`self.public_workspace_id(ws_idx)`, `self.state.workspaces[ws_idx].active_tab`
(`app/api.rs:866–879`) — as is `resolve_worktree_source`, which takes
`&mut App`. A use-case emitting its own events would need `&mut App`, making the
trait abstraction fiction.

**Stated plainly:** the applier is one callback invoked at N intermediate
points, not once at the end — a single end-of-workflow emission would go silent
for the workflow's duration then burst, breaking live subscribers and audit
ordering. `Outcome` is an enum over every use-case's mutations and the applier
is a large `match`. Every adapter must call it at the same N points. That buys
identical event streams across surfaces; it is not an elegant abstraction.

Reference implementation: `new_project` (mkdir → `git init` → initial commit,
honouring the unborn-HEAD guard at `worktree.rs:192` per #198/#243 →
`WorkspaceCreate` → pane → `AgentStart`). Extract on the second caller.

### 3. One app, three shells — the Tauri IPC is the auth layer

| shell | transport |
| --- | --- |
| Tauri desktop (Mac) | `flock.sock` direct via `invoke` |
| PWA (phone) | WSS → gateway |
| Tauri mobile (later) | WSS → gateway |

Socket perms enforce the OS-user boundary but **do not authenticate code inside
the app**: `peer_pid` resolves `pane_id_or_peer` (`app/api/panes.rs:617`) and is
never an authorization gate. The desktop app MUST expose exactly one command
(`rpc`) under a capability manifest forbidding `shell`, `fs`, `http`,
arbitrary-path `dialog::open`, and un-opted `updater`; set `default-src 'self'`
and refuse navigation outside the bundle; render artifacts out-of-process; gate
DevTools in release; ship signed, notarized, pinned-update-channel.

The TS client exposes one `RpcTransport` interface with two adapters. The
gateway (#150) serves only networked shells, and runs on the always-on host.

### 4. Tailnet-only, and identity is required

Guards stand and extend to the gateway: loopback bind (`web/mod.rs:117`) and
funnel refusal (`web/mod.rs:131`; *"funnel publishes to the PUBLIC internet;
this bridge is a full shell"* at `:138`).

Necessary, **not sufficient**: the identity allow-list is empty by default and
empty means unenforced (`web/mod.rs:592–597`); the absent-`Origin` bypass
(`:566–568`) lets any non-browser tailnet client open a WebSocket. Therefore the
gateway requires a non-empty `allowed_users` and refuses to start without one;
identity comes from `tailscale serve`'s `Tailscale-User-Login`; the gateway
propagates it into each `Request` as a `principal` alongside `peer_pid`
(`api/server.rs:600–651`) — it runs as one Unix user for all humans, so without
this per-user authorization is inexpressible, making it a #150 acceptance
criterion. `--allow-any-origin` (`web/mod.rs:779`) is refused at startup when an
identity list is configured, and the absent-Origin allowance is tightened on the
gateway path.

**The identity gate is a NETWORK gate.** It authenticates the far end of a
`tailscale serve` connection, not the local process that made the TCP connect —
anything at the operator's UID can connect to the loopback port and set a
spoofed header. Hardening needs peer-authentication of the loopback socket or an
unforgeable injected header; deferred, named.

Sharing the tailnet is equivalent to sharing a shell. Node registration is
unchanged: clients never gossip (ADR-0001 §1), `[[peers]]` stays static TOML
merged per ADR-0002, with no RPC (`FleetPause/Resume/Status` only). Adding peer
registration later is a privileged write — a peer entry creates an SSH edge
(ADR-0009).

### 5. Agent-authored artifacts are untrusted content on a distinct hostname

A **distinct hostname**, not merely a port: browsers treat ports as different
origins but the same *site*, so cookies are not port-scoped and
IndexedDB/BroadcastChannel do not partition. Use a second MagicDNS name or node
— a tailnet node, **not** a `[[peers]]` entry. Without one, serve
`Content-Disposition: attachment` and never render inline.

```
Content-Security-Policy: default-src 'none'; img-src data:; style-src 'unsafe-inline';
  script-src 'none'; object-src 'none'; connect-src 'none'; form-action 'none';
  base-uri 'none'; frame-ancestors 'none'; sandbox
X-Content-Type-Options: nosniff
```

Any relaxation (notably `script-src 'self'`) is a separate accepted ADR. No CSP,
`X-Frame-Options`, or `Referrer-Policy` exists in `web/mod.rs` today
(`:209–218` sets only Content-Type). The control plane additionally sets
`frame-ancestors 'none'`, `X-Frame-Options: DENY`, and
`Cross-Origin-Opener-Policy: same-origin`.

Artifacts render inside `<iframe sandbox>` with visible untrusted chrome — never
full-window, never `window.open`; pixel-level phishing of our own approval UI is
otherwise undefendable. Addressing is pane id + `agent_session` ref (id **or**
path). URLs are UUIDv4-unguessable **and** identity-gated — not capability URLs.
Retention defaults to 14 days with an operator purge command. The direct-socket
desktop shell has no HTTP origin; its isolation mechanism is settled in
Sequencing step 5.

### 6. Typed asks come from hooks; the answer path is atomic

Add `PreToolUse` with an `AskUserQuestion` matcher to `CLAUDE_HOOK_ENTRIES`
(`integration/mod.rs:36`). Its `tool_input` carries questions, options,
descriptions, and previews as JSON — a real form instead of matching
`"do you want to proceed?"` against the screen
(`detect/agents/claude_code.rs`), and genuine state authority where today no
Claude hook calls `PaneReportAgent` at all.

**This mechanism is Claude-only.** Agents without a typed-asks channel keep the
terminal prompt; a general contract is out of scope here.

The API MUST expose an atomic operation that (1) **verifies the pane is still
blocked on the same question id** — else the operator approves "option 2", the
agent advances to "how many files to delete? [1]", and `2\n` answers the wrong
prompt; (2) **accepts an option index, never text**, mapping index → keystrokes
server-side, since labels originate with an agent processing possibly-injected
content and a label containing `\r$(rm -rf ~)\r` must never become keystrokes;
(3) **emits only a fixed key subset** with no ANSI or control passthrough. The
client never composes keystrokes. Prerequisite to the GUI form, not a follow-up.

### 7. The terminal is a first-class per-agent view

Spawned per agent via `ClientLaunchMode::TerminalAttach`
(`protocol/wire.rs:129`) — a direct attach, not the whole-TUI `terminal-ansi`
mirror. Permanent and load-bearing: it carries every Decision 1 non-goal, and
per ADR-0012 it is the conversation view for every agent without a readable
transcript.

**The authorization delta must close before the gateway proxies it.** Today the
gate on attaching to a live PTY is `0o600` on `flock-client.sock`;
`AttachTerminal` (`wire.rs:396`) takes a caller-supplied `terminal_id` and a
`takeover` flag that evicts the operator's own client. The desktop shell is fine
(local FDs, OS-user boundary). The gateway MUST NOT proxy `flock-client.sock` to
networked shells without re-verifying identity on every `AttachTerminal`,
audit-logging attach and takeover with identity and `terminal_id`, and refusing
`takeover: true` absent an explicit capability.

Fidelity gap: flock emits Kitty graphics (field at `wire.rs:504`, 32 MB cap
`MAX_GRAPHICS_FRAME_SIZE` at `wire.rs:93`) and xterm.js implements only the
Kitty MVP (transmit and transmit+display), so image panes look correct natively
and partial in the browser. Accepted.

**A shared palette is a new artifact.** `terminal_theme.rs` tracks only default
foreground/background (`DefaultColorKind`) because the TUI inherits the host
terminal's colours. A GUI has none, so a 16-colour ANSI palette must be defined
for the first time and fed to both xterm.js's `ITheme` and the app tokens,
seeded from `ThemeConfig.accent` (`config/model.rs:428,965`).

### 8. The TUI is deprecated as a surface, retained as the reference client

Until the GUI is useful on its own terms — not until parity, which Decision 1
disclaims. It is a paint-only client of a headless server (`server/headless.rs`,
7,449 lines); removing it removes no core. Keeping it through the transition
keeps the API honest: a contract with one consumer decays into "whatever that
consumer needed."

## Sequencing

1. **1a. Generated TS types** — blocks step 3. **1b. #150 gateway** with
   identity propagation — blocks step 4.
2. **ADR-0012 step 1** (canonical model + Claude adapter + index). Independent
   of 1a; server-side.
3. **GUI shell** (desktop, direct socket): conversation view + per-agent attach.
4. **PWA over `tailscale serve`**, in-app alerts from `PaneAgentStatusChanged`,
   desktop notifications via Tauri. **Prerequisite spike:** WebKit 279904 — the
   on-screen keyboard can fail to appear in standalone iOS PWAs, disqualifying
   for a terminal view. **Config prerequisite:** the attached server defines the
   fleet view; a phone sees the laptop only if that host lists it in
   `[[peers]]`, at the ≤15s pull cadence.
5. **Artifacts**, hostname isolation and the desktop mechanism settled first.
6. **Typed asks** (Decision 6) and the atomic answer operation.
7. **Use-case extraction**, driven by the second caller appearing.

## Alternatives considered

**Extend the xterm.js mirror.** Rejected: every feature stays a TUI feature, and
mobile inherits a character grid it cannot use.

**GUI as a TUI port aiming at parity.** Rejected: imports interaction models a
conversation UI does not want, and makes deliberate scope look like debt.

**Rust + WASM in the browser.** Rejected: no capability gained, a build system
added.

**Port the TUI keybinding stack.** Not possible — `raw_input.rs`,
`pane/kitty_keyboard.rs`, and `ClientKeybindings::Local{keys_toml}` operate on
bytes and terminal protocols; a browser has `KeyboardEvent` and a phone has no
keys. Mobile gets a command surface, not a rendered keyboard.

**Use-cases emit their own events.** Rejected as unimplementable — Decision 2.

**Third-party push (Telegram, ntfy).** Rejected per operator preference.

## Consequences

- **The API becomes the bottleneck for writes by design.** The non-goals table
  is what keeps that bill bounded.
- **Audit logging covers writes and attaches** — `PaneSendText`, `PaneSendKeys`,
  `PaneSendInput`, `AgentStart`, `AgentFork`, `WorktreeCreate`, `WorktreeKill`,
  `PaneSplit`, plus client-socket attaches and takeovers with identity and
  `terminal_id`. Append-only. Today `api_request_started` (`api/server.rs:167`,
  `logging.rs:229`) logs `request_id`, `method`, `changes_ui` only. ADR-0012
  adds the read-side audit.
- **Artifacts may contain secrets.** They inherit the identity gate;
  unguessable URLs are not the control.
- **Locked-phone alerts are not free.** In-app and desktop notifications are
  free from `PaneAgentStatusChanged`. Background delivery needs APNs on iOS
  ($99/yr, self-hosted sender) or an Android foreground service — **stock
  Android only**; Xiaomi/Huawei/Oppo battery managers kill foreground services
  below the API surface. Silent APNs pushes are best-effort; PushKit VoIP has
  been CallKit-only since iOS 13. Foreground-only is a legitimate stopping point.
- **iOS PWA storage is evicted after ~7 days without interaction** (script-
  writable storage cap since iOS 13.4).
- **Cross-host state stays ≤15s stale** (ADR-0001 pull gossip, unchanged).
- **Two surfaces during the transition** means duplicate maintenance.
- **Rate limiting** for shell-equivalent methods is unaddressed; deferred, named.
- **adr-matrix**: Proposed trips no gate. On flip to Accepted, add a
  FEATURE-MATRIX.md row citing ADR-0011.
