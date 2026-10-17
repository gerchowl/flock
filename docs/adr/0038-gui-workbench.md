# ADR-0038: The GUI is a tiled workbench over one wire: a client that holds only presentation state, artifacts in opaque-origin sandboxes with feedback through questions, and goals and work as records on the same log

- Status: Proposed; the §11 owner decisions were taken on 2026-10-16 (recorded on #1591)
- Date: 2026-10-15, amended 2026-10-16 (#1606; then #1641: the client side of ADR-0036 §5a's transport recovery)
- Issues: [#1307](https://github.com/gerchowl/flock/issues/1307) (ADR-0036 slices), [#1310](https://github.com/gerchowl/flock/issues/1310), [#1323](https://github.com/gerchowl/flock/issues/1323) (superseded by the vigil decision), [#1614](https://github.com/gerchowl/flock/issues/1614), [#1380](https://github.com/gerchowl/flock/issues/1380), [#1401](https://github.com/gerchowl/flock/issues/1401), [#1402](https://github.com/gerchowl/flock/issues/1402), [#1546](https://github.com/gerchowl/flock/issues/1546) (settled by ADR-0036 §5a), [#1641](https://github.com/gerchowl/flock/issues/1641), [#1076](https://github.com/gerchowl/flock/issues/1076)
- Builds on: ADR-0036 §5–§8, §10a, §12 (wire contract, generated types, reference client, unknown events, collapsible turns, reserved extension points); ADR-0011 (conversation-first GUI, Accepted, not built); ADR-0012 (read model); ADR-0026 (mesh); ADR-0027 (delegate edges); ADR-0030 and ADR-0031 (goals); ADR-0034 (question routing); ADR-0035 (repo owner)
- Wireframes: [`0038-gui/wireframes.html`](0038-gui/wireframes.html) (static, self-contained; open it in a browser)
- Implemented: nothing.
- Prior art: vig-os/revkit (review surface: anchored comments, agent question UIs; see "Prior art"); vig-os/vigil (telemetry, see §4a)

## Summary

ADR-0036 fixed how a structured agent's stream reaches any frontend: one WebSocket wire with cursors, idempotent commands, blobs by hash and generated TypeScript types. It said little about the frontend itself: what it shows besides one agent's transcript, how several things share a screen, how agent-made artifacts are hosted and answered, and where goals, tasks and projects come from. ADR-0011 answered some of these questions in August, before ADR-0036 existed, with a different transport (Tauri IPC to the socket, a tailnet gateway) and a distinct hostname for artifacts.

This record proposes:

1. **An information architecture.** Nodes, projects, agents, goals, work, questions and artifacts are entities with ids and typed relations. The sidebar is a set of trees over them and the inbox is their attention queue (§1).
2. **A tile manager.** Every subject (a transcript, an artifact, a queue, a goal tree, a node) opens in a tile. Tiles split, stack, collapse to a rail and expand into a neighbour. Layout is client state and never reaches an agent (§2).
3. **A client state machine with no session logic.** It covers connection, cursors, a normalised entity store and a command outbox. Everything the client shows is either server state or a presentational derivation of it (§3).
4. **A render flow.** Wire events go to reducers, then to per-tile view models, then to components. Streaming text, tool cards and blobs are handled lazily (§4).
5. **Contracts.** The new streams and Rust types (`fleet`, `inbox`, `artifacts`, `work`) are generated to TypeScript under ADR-0036 §6 and versioned under `WIRE_JSON_VERSION` (§5).
6. **One client bundle in two shells.** The `flk` server serves it to any browser, and a Tauri v2 app carries it on desktop and mobile. Both shells use ADR-0036 §5's WebSocket and client tokens. That supersedes ADR-0011 §3's direct-socket desktop transport (§6, owner decision).
7. **Artifacts.**
   - Agents publish them through MCP tools.
   - They are stored as versioned blobs and rendered in an `<iframe sandbox>` with an opaque origin, which supersedes ADR-0011 §5's second hostname (owner decision).
   - A viewer answers them with anchored comments or answers.
   - An agent's request for feedback is a question (ADR-0036 §2a) that opens the artifact into a second tile (§7).
8. **Goals and work as records.**
   - First, a read-only projection of goal records, delegate edges, owner intake and the issues and PRs they name.
   - Second, a `work` record kind folded from the durable event log like ADR-0031's goals.
   - No embedded versioned graph engine for now (§8, owner decision).
9. **Slices.** Some can start now, before the -p MVP's runtime lands. The others wait for named slices of that chain (§10).

## Context

**What exists.**
- **Server.** The TUI is a paint-only client of the headless server (ADR-0011 §8).
  - Its sidebar has a *spaces* section, which is projects: repo groups of workspaces and worktrees.
  - It has a *servers* section, which is mesh nodes with health counters (#1076).
  - It has a local or all scope toggle, tabs and pane splits, a notification panel, and a separate phone layout (`src/ui/mobile.rs`).
- **Web.** The `web` feature serves the terminal bridge (ADR-0001). It has no structured client.
- **ADR-0036.** It specifies the structured client's wire (§5), types (§6) and reference client in Solid (§7). It also specifies unknown-event rendering (§8), collapsible turns as client state (§10a), and three reserved hooks for agent-authored blocks (§12).
  - The slices S7 (wire endpoint), S8 (generated types), S9a/S9b (reference client) and S18 (collapsible turns) are not started.
  - S7 depends on S3, the structured runtime, which is in progress: #1518, then #1439, then #1426.

**What is missing.**
- **One transcript is not a workbench.** An operator running a fleet watches several agents, answers questions from many, and needs to see nodes, goals and the work graph. Nothing says how these share a screen, how they collapse when not in focus, or how a phone shows them.
- **Artifacts have no host.** #1310 explores agents writing HTML fragments. ADR-0011 §5 says artifacts are untrusted and puts them on a distinct hostname. Nothing defines how an agent publishes one, how it is versioned, how a viewer answers it, or how that answer gets back to the agent.
- **Goals, tasks and projects are not on the wire.**
  - Goal records exist as server state (ADR-0030, folded per ADR-0031).
  - Delegate edges exist (ADR-0027), and owner intake is designed (ADR-0035).
  - "Work packages" and "tasks" exist only in an architect's own files. ADR-0036 §2a and S6d refer to "the open task and work-package graph" without a record that holds it.
- **The client's connection behaviour is unspecified beyond "reconnect with the last seq".** #1546 asked for liveness, reconnect policy and in-flight command rules.
- **Two transports were decided twice.** ADR-0011 §3: Tauri IPC to the socket on desktop, a WSS gateway for phones. ADR-0036 §5: one `/v1/ws` with scoped client tokens for every frontend. Nothing reconciles the two.

## Decision

### 1. Information architecture

Every thing the GUI shows is an **entity** with a stable id and a kind. Relations are typed edges that the server owns. The client never invents a relation.

| Entity | Id | Source of truth | Relations |
|---|---|---|---|
| Node | `node_id` (ADR-0026) | mesh state and gossip | hosts → project, agent |
| Project | repo key, as today's sidebar *space* | workspace config and worktrees | contains → workspace/worktree; owned_by → owner agent (ADR-0035) |
| Agent | `AgentId` (ADR-0008) | agent registry; ADR-0036 runtime tag `pty` or `structured` | runs_on → node; works_in → worktree; delegated_by → agent (ADR-0027 edge); pursues → goal |
| Goal | goal id (ADR-0030) | goal record folded from the log (ADR-0031) | held_by → agent; contains → work |
| Work | `work_id` (new, §8) | phase 1: projection; phase 2: `work` records | contains → work; blocks → work; assigned_to → agent; implements → issue; delivered_by → PR |
| Question | question id (ADR-0036 §2a, S6b) | question records | asked_by → agent; about → artifact or work (optional); waits_on |
| Permission prompt | control request id (ADR-0036 §6/S6) | session stream | raised_by → agent |
| Artifact | `artifact_id` + version (new, §7) | artifact records; bytes are blobs by sha256 | published_by → agent; about → work (optional); answered_by → comment, question |
| Notification | ADR-0016 record | notification log | refers_to → any entity |

The navigation follows from this:

- **The sidebar is three trees, each collapsible.** All three filter on the scope toggle (this node or all nodes, as today's TUI).
  - *Fleet*: node → project → agent, with health and state badges. It merges #1076's duplicate rows by `node_id`.
  - *Goals*: goal → work → agent.
  - *Projects*: project → worktree → agent, today's *spaces*.
- **The inbox is the attention queue across all agents in scope:** questions, permission prompts and feedback requests.
  - It is ordered by blocking first, then urgency, then age.
  - It shows only what the server says is open. An answered or superseded item disappears when the server says so, not when the client guesses.
- **An entity row opens its tile.** The kind decides the default tile kind (§2).

### 2. The tile manager

A **tile** shows one subject:

```text
Tile { id, kind, subject: EntityRef, mode: Expanded | Collapsed | Rail, origin: Option<TileId> }
kind ∈ transcript | terminal | artifact | queue | inbox | goal_tree | work_graph | node | trace | logs | audit | diff* | context*
```

The starred kinds are reserved: `diff` for review, and `context` for what an agent sees, from ADR-0032's compiled context. Each gets a design when it is sliced.

The **layout** is a tree.
- An internal node is a split (`horizontal` or `vertical`, with weights).
- A leaf is a **stack** of tiles with one visible.
- A **rail** is a strip on the window edge that holds collapsed tiles as chips. Each chip shows the subject, a live badge (state, open questions, unread) and the time of its last change.

Operations, the same in every shell:

| Operation | Effect |
|---|---|
| `open(subject, placement)` | Placement is `replace` (the focused tile), `tab` (the focused stack), `split_right`/`split_down`, or `beside(origin)`. With no placement the default is `replace` on desktop and `push` on phone. |
| `expand_into_second(subject, origin)` | Opens a sibling split next to `origin` and sets `origin` on the new tile. It is used when a question or a transcript block refers to an artifact, and when a work node opens its agent. Closing the origin does not close the second tile. |
| `collapse(tile)` / `restore(chip)` | Moves a tile to the rail and back. A collapsed tile keeps its subscription only if it shows a badge; otherwise it unsubscribes and resubscribes from its cursor on restore (§3). |
| `focus(tile)` | Keyboard and pointer focus. It also sets the inbox's "this tile's agent" filter. |
| `close(tile)` | Unsubscribes the tile's streams if no other tile shares them. |

**Persistence.**
- **Client-local.** The layout is per device, keyed by server identity, in browser storage or the Tauri store.
- **Never sent to an agent.** It never enters an agent's context. Moving tiles is not a fact about the work.
- **Server-synced named layouts are a later option** (owner decision 9).

**Phone.** One tile is visible at a time. A tile switcher replaces splits, and the rail becomes a bottom sheet. `expand_into_second` pushes the second tile with a back gesture to its origin. The keyboard accessory bar of #1402 belongs to the composer, not to the tile manager.

### 2a. The MVP workbench: what the person sees and does

§2 defines the tile mechanics. This section defines the screen they serve. The first client preview built §2 literally: a subject/kind/placement form and per-tile buttons. The owner rejected its look and UX, and on 2026-10-17 chose the "Mission control" wireframes for the MVP (owner decision 11: typed by the owner, relayed by the GUI architect). The rule that follows: **the tile manager is machinery under the screen, never the screen itself.** No raw subject, kind or placement field appears in the product.

**Desktop: three regions.**

| Region | Content | Behaviour |
|---|---|---|
| **Sidebar** (left, fixed width, collapsible) | §1's trees. A *Projects / Nodes* toggle, then groups (a project's agents, *Architects*, other projects folded to one row each with a count such as "3 idle" or "2 blocked"). | Each agent row: a status mark, the agent name and its task, one line of current activity, and a count of its open inbox items. A click focuses the agent in the centre. |
| **Focus pane** (centre) | The focused agent's tile (§2), by default one `transcript` tile. | A header line: agent, task and goal, PR, branch, model, a state chip with its age, and the actions *Terminal*, *Diff* and *Fork*. The transcript reads as turns. A tool run is one line (command, exit code, duration) that expands on demand. Older turns fold to "N earlier turns". A composer sits at the bottom and says what will happen to a message: queued until the turn ends, or delivered now. |
| **Inbox** (right, fixed width, collapsible) | §1's attention queue: questions, permission prompts, feedback requests and question threads (#1717), blocking first. | Every card answers in place: one button per choice, plus *Edit…* for free text and *Take as thread*. A thread card shows its message count and, once the fork has an answer, *Review and merge back*. Below the cards, *Recent* lists the last few landed or notable events. |

- **One command palette** (⌘K, and a search field in the top bar) jumps to any agent, issue or PR and runs commands. It is the only way to "open a subject" by name.
- **Splits are an advanced action.** `split_right`, `tab` and `expand_into_second` are reached from a tile's menu, the palette, or by following a reference (an artifact link in a transcript opens beside it). The default is one tile. The rail holds collapsed tiles as chips in the focus pane's header strip.
- **Top bar:** product mark, search/palette, node summary ("2 nodes live"), agent summary ("9 agents · 7 working") and *New agent*.

**Phone: inbox first.**
- A bottom tab bar: **Inbox** (the default screen), **Agents** and **Activity**.
- **Inbox:** cards as on desktop, with primary actions as full-width 48 px buttons. A header line says whether anything is blocked on the person.
- **Agent screen:** a back control, the agent's name and state, and a segmented control *Summary · Transcript · PR · Terminal*. *Summary* shows the goal with step chips (done, running, next), the latest turn, and any open thread with its *Merge back* action. The composer is pinned to the bottom.
- No splits on a phone. `expand_into_second` pushes a screen, and back returns to its origin (§2 Phone).

**Status marks** differ in lightness and in shape, never in hue alone:

| State | Mark | Use |
|---|---|---|
| working | filled circle, blue | turn in progress |
| needs you | filled circle, amber, with a count badge | an open blocking item |
| idle or held | square, grey | no turn, nothing open |
| done or passed | check, green | a settled step or check |
| failed or blocked | outlined circle, dark red | a failed step, or a blocked agent with no open item |

**Visual baseline:**
- Light and dark themes from one token set: ground, surface, ink, muted ink, border, plus the four status colours. Text meets 4.5:1 contrast in both themes.
- The system UI font stack and a system monospace. §6's bundle rule (no web fonts at runtime) stands. The wireframes' typeface is not a requirement.
- 14 px body text on desktop and 15 px on the phone. Every touch target is at least 44 px.

**What provides each part, and what waits.** The MVP is what S7-min (#1746), G3a (#1747) and G2 deliver. The rest of the screen is drawn now and filled later:

| Part | Provided by | In the MVP |
|---|---|---|
| Sidebar agent rows (status, task, activity) | S7-min's fleet-lite list, live before G4. The full trees, node grouping and *Projects / Nodes* come with G4 #1596. | yes, flat list of local structured agents. Each row shows state and activity only: fleet-lite's `FleetAgentRow` has no task field. The task, collapsible regions and the node line follow in #1770 |
| Focus pane transcript and composer | G3a over S7-min (`agent/<id>`, `agent.send`) | yes |
| Inbox cards with in-place answers | G5 #1597 (inbox stream); S6-MVP for permission prompts | fixture data until G5 |
| *Take as thread*, *Review and merge back* | #1717 (question threads) | post-MVP |
| *Terminal* and *Diff* actions, phone *Terminal* tab | the `terminal` tile (no slice yet) and the reserved `diff` kind | post-MVP |
| *Fork* | ADR-0036 S12 (forks at any boundary) | post-MVP |
| Phone *PR* tab, palette search over issues and PRs | a PR/issue source (no slice yet) | post-MVP: the MVP palette searches agents only |
| Phone *Activity* tab and *Recent* list | the `events` stream (S7-rest) | post-MVP |
| *New agent* | ADR-0036 S17 (harness, model and effort picker) | post-MVP |
| Node summary ("2 nodes live") | G4 #1596 and the mesh | post-MVP: the MVP shows this node only |

A post-MVP part is either hidden in the MVP, or shown disabled with the name of what it waits for. It is never a button that does nothing.

**Out of the MVP:** an *Agent board* (agents as cards in columns: needs you, working, idle) was the rejected alternative for the main screen. It may return later as a second view of the same data (Alternatives considered).

### 3. The client state machine

The client follows ADR-0036 §5: **it holds no session logic and computes nothing the server has not sent.** This record makes that line precise.

The client may derive:
- **Presentation:** grouping by turn (§10a), collapse state, sort order inside a list, relative times, counts of server-sent items, text search over what it holds, and layout.

The client must never derive:
- agent status;
- whether a question is open, or which answer is valid;
- queue order or delivery;
- permission outcomes;
- goal or work state;
- anything sent to an agent other than a `cmd`.

**Connection lifecycle.** The states and transitions below are normative. ADR-0036 §5a (#1546) sets the parameters this record uses by name, as proposed values that S7 and G3 test:

| Parameter | Value (ADR-0036 §5a) | Where it acts here |
|---|---|---|
| connect deadline | 10 s per attempt | Connecting |
| handshake deadline | 5 s from socket open to `welcome` | Hello |
| heartbeat | the server sends `ping { nonce }` every 15 s; the client answers `pong { nonce }` at once | Syncing, Live, ReadOnly |
| liveness window | 45 s without any valid server frame, `ping` included | heartbeat missed → Stale |
| subscription progress deadline | 30 s without a frame that advances that `sub`, while it waits for `synced` or a snapshot's end | Replaying, Resyncing |
| command ack deadline | 30 s from each send | the outbox: `sent` → `uncertain` |
| reconnect backoff | attempt n (counting from 0) waits a uniform value in [c/2, c], c = min(30 s, 500 ms × 2ⁿ); it resets only after 60 s of continuous Live or ReadOnly | Reconnecting |

Every state that waits on the server has a deadline that ends it (ADR-0037: waits fail, they never hang). The client measures them on a monotonic clock. A suspended page or app does not count frozen time as liveness: on foreground it closes the old socket and connects again. No deadline ever makes a stream Current or a command acknowledged; expiry only fails. The diagram shows the main path. The tables are complete and win where they differ.

The lifecycle has two levels. The **connection** has one state at a time. Each **stream** subscribed on that connection has its own state, because `synced`, `gap` and `lagged` (ADR-0036 §5) are per `sub`.

```text
Disconnected ─▶ Connecting ─open─▶ Hello ─welcome─▶ Syncing ─every stream settled─▶ Live
                  ▲    │ fail or     │ │ │ client
                  │    │ connect     │ │ │ newer
                  │    ▼ deadline    │ │ ▼
                  │  Reconnecting ◀──┘ │ ReadOnly
                  └── backoff ──┘ handshake deadline, protocol error
   from Hello, Syncing, Live, ReadOnly: socket lost ──▶ Reconnecting
   from Syncing, Live, ReadOnly: heartbeat missed ──▶ Stale ──▶ Reconnecting
   Hello, Syncing, Live, ReadOnly: close 4401 or 4403 ──▶ NeedsToken ──new token──▶ Connecting

per stream, while the connection is Syncing, Live or ReadOnly:
   sub ─▶ Replaying ─synced─▶ Current
   Replaying or Current ─gap with snapshot_to > to─▶ Resyncing ─ev seq snapshot_to applied─▶ back
   Replaying or Current ─gap with snapshot_to == to (empty)─▶ Resyncing, ended at once ─▶ back
   Replaying ─progress deadline─▶ unsub, re-sub from the cursor ─▶ Replaying
   Resyncing ─progress deadline─▶ unsub, re-sub with the marker ─▶ Resyncing, returning to Replaying
   any ─subfail cursor_ahead─▶ cleared, re-sub without after ─▶ Replaying
   any ─subfail operator_only, scope, unknown_stream─▶ Refused
```

**Connection states.** A dash means the event cannot occur in that state, and the cell says why. From every state, the user's `disconnect` closes any socket and goes to Disconnected, holding the outbox.

| State | Own exits | Heartbeat missed | Socket lost | `gap` | Auth refused (close 4401 or 4403) |
|---|---|---|---|---|---|
| Disconnected | `connect` → Connecting | — no socket | — no socket | — no socket | — no socket |
| Connecting | socket open → Hello; failure, or the connect deadline elapsed → Reconnecting | — no heartbeat before `welcome` | → Reconnecting (a failure) | — no `sub` yet | — the token is sent in `hello` |
| Hello | `welcome`, client not newer → Syncing; `welcome`, client newer → ReadOnly; the handshake deadline elapsed without `welcome` → Reconnecting; any other frame is a protocol error: close, show the error, → Reconnecting. A server that keeps sending protocol errors loops through Reconnecting under capped backoff **by design**: the error stays shown, and a server restart or upgrade can cure it without user action. The user can stop it with `disconnect`. | — the handshake deadline covers Hello | → Reconnecting | — no `sub` is sent before `welcome`, so a `gap` here is the protocol error | close 4401 or 4403 → NeedsToken |
| Syncing | every seq stream is Current or Refused → Live | → Stale | → Reconnecting | a snapshot stream → Resyncing; a history stream gets its missing-range marker; the connection stays Syncing | close 4401 or 4403 → NeedsToken |
| Live | — Live is the steady state: it ends only on the events in the other columns | → Stale | → Reconnecting | a snapshot stream → Resyncing; a history stream gets its missing-range marker; the connection stays Live | close 4401 or 4403 → NeedsToken |
| ReadOnly | user reload after a server upgrade → Connecting | → Stale | → Reconnecting; the next `welcome` decides again | a snapshot stream → Resyncing; a history stream gets its missing-range marker; the connection stays ReadOnly | close 4401 or 4403 → NeedsToken |
| Stale | → Reconnecting at once: it closes the socket and is entered only to mark tiles while the reconnect begins | ignored: the socket is already being closed | ignored: already leaving | ignored: frames after Stale are discarded, and the stored cursor replays them | ignored: the next `hello` reports it again |
| Reconnecting | backoff elapsed, or a network-change or foreground event → Connecting. Only one attempt runs at a time: such events cancel the backoff for one immediate attempt and coalesce while it runs | — no socket | — no socket | — no socket | — no socket |
| NeedsToken | new token → Connecting | — the socket was closed on entry | — the socket was closed on entry | — the socket was closed on entry | — the socket was closed on entry |

What the columns count, from ADR-0036 §5a:
- **Heartbeat missed** means no valid server frame, `ping` included, for the liveness window. The client answers every `ping` on the current socket with `pong { nonce }` at once, ahead of rendering, reducers and queued `cmd`s. Heartbeats never reset a stream's progress deadline.
- **Socket lost** covers a drop without a close, a close with any code other than 4401 or 4403, and a protocol error the client detects. A protocol close shows its diagnosis. A drop keeps the applied cursors, the resync markers and the outbox.
- **Auth refused** is only the two close codes: 4401 answers `hello`, 4403 revokes the token mid-session. Either code leads to NeedsToken in every state that has a socket, and the code only picks the message shown. A code arriving in the "wrong" state (4403 during Hello, 4401 after `welcome`) still says the token is no longer valid. Treating it as a protocol error instead would reconnect, again and again, with a token the server refuses. Before a 4403, the `cmd` that found the revocation gets an `ack` with `auth_revoked`, which fails that command. An `ack` with `scope` is not auth refused: it fails that one command and the socket stays up.
- **Abandoned sockets.** Once the client leaves a socket (Stale, a deadline, a reconnect, `disconnect`), it discards every frame that socket still delivers, late `ping`s included: none answers a `pong`, advances a cursor, renews liveness or resolves a command.
- **No compression.** The upgrade never negotiates `permessage-deflate`; browser and webview shells cannot keep it off the `hello` that carries the token (ADR-0036 §5a).

**Stream states.** Each seq stream (every stream but `*/live`, ADR-0036 §5) has one. `*/live` streams have no cursor and no `synced`, and do not count toward Live. A connection-level exit (socket lost, Stale, NeedsToken) ends every stream's state. After the reconnect, each `sub` starts from its stored cursor and its stored resync marker.

Seq streams come in two kinds, and a `gap` means something different on each:
- **Snapshot streams** have an inline snapshot after every `gap`: `queue/<id>`, `terminal/<pane>` (ADR-0036 §5a) and §5's `fleet`, `inbox`, `artifacts` and `work`. Their `gap` always carries `snapshot_to`.
- **History streams** have none: `agent/<id>`, `events`, and §5's `telemetry/traces`, `telemetry/logs` and `audit`. Their `gap` never carries `snapshot_to`; it leaves a missing-range marker (below), and the stream continues from `to + 1`.

A `gap` of the wrong kind for its stream is a protocol error: close → Reconnecting.

**Sub ids and closed subs.** A `sub_id` is unique for the life of a connection: the client never reuses one, even for the same stream, so one transport and one id counter serve every view on that connection. `unsub` crosses the wire, so the server may still deliver frames for a sub the client has already closed (`ev`, `gap`, `synced`, `lagged` or `subfail`). After the client sends `unsub`, or receives `subfail`, it silently drops every frame for that `sub_id`. Such a frame is not a protocol error and never touches another stream's state. A frame for a `sub_id` the client never opened on this connection is still a protocol error.

The **progress deadline** runs while a stream waits for `synced` or for a snapshot's end. It starts when the `sub` is sent, or when a `gap` starts Resyncing on a Current stream. Only a frame that advances that `sub` resets it: a newly applied seq, an advancing `gap` or snapshot boundary, or its `synced`. Duplicates, a repeated identical `gap`, other streams' frames and heartbeats do not. On expiry the client sends `unsub`, then a new `sub` from the applied cursor and the stored resync marker. The connection and the other streams stay up, and the stream's tiles show it recovering. A replay that keeps advancing may take longer than 30 s in total.

| Stream state | Exits |
|---|---|
| Replaying (after `sub`, from the stored cursor) | `synced` → Current; `gap` with `snapshot_to` (a snapshot stream) → Resyncing, returning to Replaying; this includes the bootstrap `gap` that answers a first `sub` without `after`; `gap` without `snapshot_to` (a history stream) → the marker below, stays Replaying; `lagged` → re-`sub` from the last applied seq, stays Replaying; progress deadline → `unsub` and re-`sub` from the applied cursor, stays Replaying; `subfail` → the rule below the table |
| Current | `synced` cannot arrive: it is sent once per `sub` and Current was entered by it, so a second one is a protocol error (close → Reconnecting); `gap` with `snapshot_to` → Resyncing, returning to Current; `gap` without `snapshot_to` → the marker, stays Current; `lagged` → re-`sub` from the last applied seq → Replaying; no progress deadline: a Current stream waits for nothing, and live traffic may be quiet for any length of time; `subfail` → the rule below the table (the server may end an accepted `sub`, for example `unknown_stream` when its pane is removed) |
| Resyncing | the snapshot ends (below) → it drops every entity still unconfirmed, clears the resync marker, then returns to its return state; another `gap` with `snapshot_to` on the same stream (the log was rewritten again) → restarts Resyncing on the new snapshot and marks **all** of that stream's entities unconfirmed again, including any the earlier partial snapshot confirmed, since one may have been deleted since; `lagged` → re-`sub` from the last applied seq, and the return state becomes Replaying (the new `sub` gets its own `synced`); the server replays the rest of the snapshot from the log, so it stays Resyncing with the same `snapshot_to`; `synced` cannot arrive first, because it follows seq `snapshot_to` in seq order; `gap` without `snapshot_to` cannot arrive: only snapshot streams enter Resyncing, and ADR-0036 §5a forbids a snapshot stream's `gap` without `snapshot_to`, so one is a protocol error (close → Reconnecting); progress deadline → `unsub` and re-`sub` from the applied cursor with the stored marker, stays Resyncing with Replaying as its return state; `subfail` → the rule below the table |
| Refused (after `subfail` with `operator_only`, `scope` or `unknown_stream`) | Final for that `sub`: no re-`sub` and no reconnect. The tiles keep their content with a stale marker and say why: the principal is not the operator, the token lacks the scope, or the stream is unavailable (an unknown kind or a removed subject, never shown as an empty success). It ends when the cause changes: a new token or principal (`operator_only`, `scope`) or the user picking a valid stream (`unknown_stream`). After a reconnect it is re-`sub`bed only if the new `welcome` shows a change that can cure it. For `operator_only`, that is another principal (ADR-0036 §5a). For `scope`, another principal or other capabilities. For `unknown_stream`, another `server_version`: this one is a client policy choice, not an ADR-0036 rule, because a newer server may know the stream kind; it costs one `sub` per upgrade. |

- **`subfail` ends only the named `sub`**, in any of the states above. Its code decides:
  - `cursor_ahead`: the stored cursor is ahead of the stream's head (the server lost state). The client clears that stream's entities, cursor and resync marker, then re-`sub`s without `after`, so the stream bootstraps → Replaying. It never clamps the cursor or treats the stream as caught up.
  - `operator_only`, `scope`, `unknown_stream` → Refused.
- **Refused counts as settled.** Syncing ends when every seq stream is Current or Refused, so a refused stream never holds the connection out of Live.

- **When a snapshot ends.** A `gap` with `snapshot_to > to` announces snapshot lines with seqs `to + 1` through `snapshot_to`. The snapshot ends when the `ev` with seq `snapshot_to` has been applied. A `gap` with `snapshot_to == to` announces an **empty snapshot**: the stream has no items, for example when every queue item had already been delivered when the rewrite ran. That Resyncing ends as soon as the `gap` is processed: every entity of the stream is dropped, the cursor advances to `to` (which keeps the stream's high-water, never zero once it has been nonzero), and the stream returns to its return state without waiting for any `ev`. A `gap` without `snapshot_to` belongs to a history stream, never an empty snapshot.
- **The first `sub` on a snapshot stream** (no `after`) is answered through the same path: `gap { from: 1, to, snapshot_to }`, where the snapshot lines are `to + 1` through `snapshot_to`. A brand-new empty stream answers `gap { from: 1, to: 0, snapshot_to: 0 }`, which ends at once with the cursor at 0. The client fetches nothing else and has no separate bootstrap reducer.
- **`terminal/<pane>`'s snapshot is one complete frame**: its `gap` is `{ from: cursor + 1, to: H, snapshot_to: H + 1 }`, followed by the full terminal frame and rendering state at H + 1, then deltas. The tile keeps showing the old frame, marked stale, until the full frame has been applied, because a delta cannot rebuild a missing base, and `synced` never stands in for the frame.

- **Streams are independent.** A `gap` on a second stream while the first is Resyncing starts the second's own Resyncing. Neither waits for the other.
- **Syncing ends on Current or Refused, not on Resyncing.** If Syncing would end while a stream that was already Current is Resyncing, the connection waits for that resync to finish too. Once the connection is Live, a stream's later Resyncing never takes the connection out of Live. Only that stream's tiles show a "resyncing" marker. The outbox keeps sending, because every verb is verified by the server against its own current state (atomic answers, below).
- **The resync marker** (`snapshot_to` and the unconfirmed set) is stored with the stream's cursor and cleared when the snapshot ends. After a reconnect or reload, the stream's new `sub` from the stored cursor starts in Replaying. If a marker is stored and the cursor is below its `snapshot_to`, the stream enters Resyncing at once with Replaying as its return state, and the server replays the rest of the snapshot from the log. If the server no longer holds those lines, it sends a new `gap`, which restarts Resyncing and marks every entity unconfirmed again.

- **Hello.**
  - It sends `hello { protocol, client, token }`.
  - On `welcome`, it records `capabilities` and `principal`.
  - **Version skew follows ADR-0036 §5 exactly.**
    - Only a client **newer** than the server enters **ReadOnly**: the server's `welcome` carries read-only capabilities. The client can view, but cannot send verbs the server doesn't know.
    - A client **older** than the server keeps working normally. Event types it doesn't know arrive in the §8 unknown wrapper and render as the JSON dump.
- **ReadOnly.**
  - Subscriptions, stream states and rendering work as in Live, including Stale on a missed heartbeat and Resyncing on a `gap`.
  - The outbox is **held**, not sent and not dropped, and the composer and answer controls are disabled with a banner naming the version gap.
  - Its exits are in the connection table: Connecting on a reload after the server is upgraded, Reconnecting when the socket is lost (the next `welcome` decides again), Stale, and NeedsToken.
- **Syncing and Live** handle the same stream signals. They differ only in that Syncing has not yet seen every seq stream reach Current (its `synced` frame, ADR-0036 §5).
  - **`ev`:** dedupe on `(stream, seq)`. Apply through the stream's reducer. Advance that stream's cursor **only after** the reducer has applied the event.
  - **`lagged { skipped_to }`:** the server dropped events for a slow subscriber. Re-subscribe with `after: <cursor>`, the last applied seq, so the log replays everything after it. Never jump to `skipped_to`, because that would skip replayable entries.
  - **`gap { from, to }`:** the log no longer holds that range.
    - **Snapshot streams** follow ADR-0036 §4 and §5a: `queue/<id>`, `terminal/<pane>`, and this record's §5 streams (`fleet`, `inbox`, `artifacts`, `work`). The server sends the snapshot inline, right after the `gap`. Its lines take the seqs `to + 1` through `snapshot_to`, the field the `gap` carries for exactly this purpose (ADR-0036 §5), and they carry the full current state of every item. That stream enters **Resyncing**:
      - it marks that stream's entities as unconfirmed;
      - it applies the snapshot lines through the normal reducer, advancing the cursor as usual;
      - when the `ev` with seq `snapshot_to` has been applied, or at once when `snapshot_to == to` (an empty snapshot, which advances the cursor to `to`), the snapshot has ended. It drops every entity still unconfirmed, because an item the snapshot doesn't mention no longer exists.
      - `snapshot_to` is the only way a snapshot ends. The client never infers the end from a frame's content or from timing.
      - Rendering stays batched per animation frame, so a snapshot delivered in one burst never shows an empty list.
      - There is no client-side fetch: the snapshot comes on the same stream as everything else.
    - **History streams have no snapshot.** On `events`, `telemetry/*` and `audit` a gap means retention removed those entries; the client shows the same marker and continues. `agent/<id>` has no other source either: it is the stored session stream itself (ADR-0036 §4). A gap there means flk no longer holds those lines, for example after retention removed them (ADR-0036 S16). Nothing else holds them either: the harness's native transcript is a disposable working copy that flk does not serve.
      - The client renders a fixed marker: "events `from`–`to` of this session are no longer stored".
      - It continues from `to + 1` and never implies the transcript is complete.
      - Recovery for a reader who needs the missing turns is outside the client: the operator restores the session stream from a backup, if one exists. Otherwise the range is gone.
- **`*/live` streams** (partial deltas, no cursor) are re-subscribed after every reconnect, with no `after`. In-flight blocks are dropped (§4).
- **Stale.**
  - Entered when no valid server frame, `ping` included, has arrived within the liveness window (45 s). A stalled stream or a missing `ack` never tears the connection down. Only a missed heartbeat, a lost socket, a protocol error, an expired connect or handshake deadline, or a close with 4401 (auth refused at `hello`) or 4403 (token revoked) does.
  - **A server handoff** is not a teardown cause of its own. Under ADR-0036 §5a's single-writer handoff, the old server keeps serving reads until the fenced ownership transfer. Its 10 s deadline covers the whole handoff. Meanwhile, new commands get an error `ack` with `restart_pending`, which fails them like any other error `ack`. At the transfer, the old server closes every `/v1/ws` socket. The client sees that as socket lost → Reconnecting, and resends under the outbox rules below. A failed handoff (`handoff_failed`) keeps the old server serving, and the connection never notices it.
  - Tiles keep their content with a visible stale marker. The outbox is **held**.
  - Reconnecting uses the backoff in the parameter table. A network-change or foreground event cancels it for one immediate attempt, and a foreground event first closes a socket that may have been suspended.
  - The client always offers `disconnect`, which cancels the attempt and its timer. While it reconnects it shows the latest failure.
- **NeedsToken.**
  - Entered on close 4401 (the token was refused at Hello) or 4403 (it was revoked mid-session). The client clears the stored token and asks the user. It does not reconnect until a new token arrives.
  - The outbox is **held** until a new token connects. If the new principal differs from the old one, the outbox is **dropped** and the user is told, because commands must not run under another identity.
  - A Tauri shell may unlock a stored token with biometrics (§6).

**The store.**
- Entities are kept normalised by kind and id.
- Each stream has a cursor, persisted per device so a reload resumes without a full replay. It is persisted only after the event (or the empty snapshot) it names has been applied, and together with the stream's resync marker. The `*/live` streams are the exception: they carry deltas with no cursor (§5).
- The outbox is persisted too, each command before its first send (below).
- Ephemeral state lives in the store but is not persisted: in-flight streaming blocks (§4).

**The command outbox.**
- Every `cmd` gets a `cmd_id` (UUIDv7) and is stored with its `verb`, `args`, the principal that queued it and its first-send time. It is persisted before its first send. Commands, first sends and resends alike, go out only in Live: Syncing waits for Live, and ReadOnly holds them (a newer client must not send verbs the server may not know).

| Command state | Meaning | Exits |
|---|---|---|
| `pending` | queued, not yet sent | Live → sent with `resume_only: false` → `sent` |
| `sent` | on the current socket, waiting for its `ack`: a first send (`resume_only: false`) or a resend (`resume_only: true`) | `ack` ok → `acked`; an error `ack` refusing a resume-only lookup as uncertain (`session_unconfirmed`, or an absent or expired key) → `unknown_outcome`; any other error `ack` → `failed`; the ack deadline (30 s) → `uncertain`; socket lost → waits for the resend below |
| `uncertain` | its delivery is unknown; the connection stays up, and nothing is resent on the same socket | a late `ack` on the same socket → as for `sent`; socket lost → waits for the resend below |
| `unknown_outcome` | the client cannot learn whether it ran ("unknown outcome" in the prose below and in the UI) | none automatic: the user inspects the subject's server state and decides on any new command |
| `acked`, `failed` | settled by the server | none |

- **State tokens.** The client-observable command states are this closed set: `pending`, `sent`, `uncertain`, `unknown_outcome`, `acked`, `failed`. Fixtures, the store and generated types use exactly these tokens. ADR-0036 §5a uses "uncertain" in two senses, neither of them a seventh state: the ack deadline passing, which is the client's `uncertain` state, and a resume-only lookup the server refuses "as uncertain" (`session_unconfirmed`, or an absent or expired key), which makes the command `unknown_outcome`, as in the `sent` row.
- **Window expiry runs in every connection state.** For each `sent` or `uncertain` command, the client checks its age against `idempotency_window_ms` (from the last `welcome`, persisted with the outbox) in every connection state: Connecting, Hello, Syncing, Live, ReadOnly, Stale, Reconnecting, NeedsToken, and after a user `disconnect`. It does not check only when a resend would go out. A command is expired when its age is **at least** `idempotency_window_ms` (age ≥ window, where age is measured from its first send). Age is evaluated at the instant of each event that could change the command, such as the actual reconnect attempt or the `welcome` that follows it, never at a deadline's upper bound. A command that is still within its window at the instant the resend goes out, once the reconnect has reached Live, is resent, even if the connect deadline would have run past the window edge. A command whose window passes before that instant is not resent. At expiry the command becomes `unknown_outcome` at once, so the UI never shows a command as "will be resent" after its window has passed. A `pending` command has no first send, so no window applies to it. It waits for Live, unless a changed principal drops the outbox (ADR-0036 §5a). An `uncertain` command that expires while its socket is still up also becomes `unknown_outcome`, so a very late `ack` on that same socket is dropped. This is intended: the window defaults to 24 h, and the client has already told the person that the outcome is unknown.
- **Resend after a reconnect.** A `sent` or `uncertain` command whose socket was lost is resent when the connection is next Live, and becomes `sent` again (if that socket is lost too, it is resent again after the next reconnect, still within the window), under the same principal, with the same `cmd_id`, `verb` and `args` and `resume_only: true`, and only within the `idempotency_window_ms` that `welcome` reports, counted from the first send. The server answers with the first `ack`, or refuses the lookup when it holds no claim (`session_unconfirmed`), which makes the command `unknown_outcome`.
- **Unknown outcome** also follows, without a resend, when the window has passed by the time the resend would go out, or when the client cannot establish a command's age after a restart. It is never resent automatically and never re-sent under a new `cmd_id`: the original may still run once, because it can arrive late on the dead socket after the lookup was refused. So the client says "unknown outcome", never "cancelled" or "not run". `session_unconfirmed`'s `retry: later` permits only the same resume-only resend, never a first send.
- **Optimism is visual only.** A pending command marks its subject "pending" and never changes an entity. The entity changes when the server's event arrives.
- **Answers are atomic.** An answer to a question or a permission prompt is the server's atomic verify-then-apply (ADR-0011 §6: same question id, index not text). A stale answer comes back as `failed` with the question's current state.

### 4. The render flow

```text
wire ev ─▶ reducer(kind) ─▶ store (normalised) ─▶ selector(tile) ─▶ view model ─▶ component
                                   ▲                     │ memoised per tile and subject
             blob fetch ◀── lazy ──┘                     ▼
          (/v1/blob/<sha256>)                       Solid fine-grained signals
```

- **Reducers.**
  - Reducers are generated per event kind or hand-written against generated types. Hand-written wire shapes are forbidden (ADR-0036 §6).
  - Each takes `(store, ev)` and returns the changed entity ids. They are pure, with no I/O.
  - Unknown kinds go to the §8 JSON dump reducer.
- **View models.** These plain immutable values are all that passes to the renderer. Components never touch the socket or the store directly. The main ones:
  - **`TranscriptVM`:** turns (collapsed or expanded, §10a), blocks with their stable block ids (§12 item 2), tool cards, and the in-flight block.
  - **`ArtifactVM`:** title, version list, sandbox URL or blob handle, comments by anchor, and open feedback requests.
  - **`InboxVM`:** ordered items, each with its kind, source agent, blocking flag and the actions the server allows.
  - **`FleetVM`**, **`GoalTreeVM`** and **`QueueVM`**.
- **Which source feeds a transcript tile (reconciling ADR-0012).**
  - A `structured` agent's tile reads ADR-0036's session stream. Its events are the canonical entries ADR-0012 §1 asked for, with stable ids and an unknown-event fallback.
  - A `pty` agent's tile uses the terminal view (ADR-0011 §7) until ADR-0012's canonical entries and derived index are built for its harness. Then the same `TranscriptVM` renders them, and the client doesn't care which producer made them.
  - ADR-0012 §6's rule, that reads are authorization-scoped, is applied by the server per stream through the token's scopes (ADR-0036 §5).
- **Streaming text.**
  - `agent/<id>/live` deltas append to an in-flight block keyed by `(message uuid, block index)`.
  - When the complete `assistant` event arrives on `agent/<id>`, it replaces the in-flight block (ADR-0036 §4: deltas are not persisted).
  - A reconnect drops in-flight blocks. The next complete event redraws them.
- **Tool cards and large content.**
  - A tool call renders collapsed, showing its name, a one-line input summary, status and size.
  - Content over `blob_min_bytes` arrives as `{blob, size, preview}` and is fetched only on expand, with its HTTP cache as ADR-0036 §5 describes.
- **Long transcripts.**
  - Virtualised: only visible turns hold DOM.
  - Collapsed turns render their one-line summary from the turn boundaries the server sends.

### 4a. Observability: trace and log views on vigil's output

The owner chose vig-os/vigil for flk's telemetry (§11 item 5). vigil bridges `tracing` into OpenTelemetry and writes OTLP/JSON Lines files: one `LogsData`, `MetricsData` or `TracesData` per line, one signal per file. Its opt-in `audit` feature adds a hash-chained, signed trail. #1614 decides what flk emits, where the files live and the retention; this record decides only how the GUI shows them.

- **The server reads; the client never touches files.** The flk server exposes read-only, scope-gated views over vigil's files on the same wire:
  - `telemetry/traces` (a `sub` with a filter: trace id, agent id or time range);
  - `telemetry/logs` (filter: level, target, agent id, time range);
  - `audit` (verification status per segment).
  - `telemetry/traces` and `telemetry/logs` need the `read` scope. `audit` is operator-only: it needs the `operate` scope, because it names actors (§5).
  - Clients get records already decoded from OTLP/JSON into the same generated-type discipline as §5. There are no hand-written shapes.
- **Correlation, not a second history.**
  - Spans carry the agent id, session, `cmd_id` and turn as attributes (decided in #1614).
  - A transcript turn or a tool card shows "trace" when a trace id is attached, and opens a **trace tile** (`kind = trace`): a span waterfall for that turn, with tool calls, queue delivery and mesh hops.
  - The session stream stays the transcript of record. Telemetry explains timing and failures; it is not a copy of content.
- **Tiles.**
  - `trace`: the waterfall.
  - `logs`: a filtered, virtualised list that follows new lines.
  - `node` gains health metrics from `MetricsData`.
  - `audit`: the chain's verification status (verified up to segment N, or a broken link at segment M), and actor and action rows.
  - All are ordinary tiles: they collapse to the rail and open beside a transcript.
- **No content in telemetry.** Message bodies, comment text and tokens never enter spans or logs (#1614, item 4), so the telemetry views need no artifact-style sandboxing.
- **External tools stay optional.** Loki, Prometheus or any OTLP consumer can read the same files. The GUI does not depend on them.
- **Slice:** G13 (§10). It waits for vigil's first release and for #1614.

### 5. Contracts: what the wire gains

All types are Rust types with `ts_rs::TS` derives behind the `ts` feature (S8) and appear in `web/client/src/gen`. Each release that adds a stream or event kind bumps `WIRE_JSON_VERSION` once. Older clients fall back to §8's unknown rendering.

**New streams**, alongside ADR-0036 §5's `agent/<id>`, `agent/<id>/live`, `queue/<id>`, `events` and `terminal/<pane>`:

| Stream | Seq | Carries | Inline snapshot after `gap` (ADR-0036 §4 rule) |
|---|---|---|---|
| `fleet` | yes | node, project and agent summary upserts and removals in the client's scope | full summary set |
| `inbox` | yes | open questions, permission prompts and feedback requests across agents in scope: opened, updated, closed | open items |
| `artifacts` | yes | artifact record versions in scope, plus comments | records and the latest N comments per artifact |
| `work` | yes | goal and work node upserts and edges | the goal and work subgraph in scope |
| `telemetry/traces` (G13, §4a) | yes, the unfiltered stream's | decoded OTLP spans matching the `sub`'s filter (trace id, agent id, time range) | none: a `gap` means vigil's retention removed those files, and the tile shows a "no longer retained" marker, as for `agent/<id>` |
| `telemetry/logs` (G13, §4a) | yes, the unfiltered stream's | decoded OTLP log records matching the filter (level, target, agent id, time range) | none, as `telemetry/traces` |
| `audit` (G13, §4a) | yes, in chain order | audit segments with their verification status, plus actor and action rows | none: a `gap` means retention removed segments, and the status covers only the retained segments, saying so |

- **Telemetry streams.** A filtered `sub` sees the seqs of the unfiltered stream, so they are sparse. The client relies only on the `gap` frames the server sends, never on holes in the seqs. All three are read-only: no `cmd` verb writes them. `telemetry/traces` and `telemetry/logs` need the `read` scope. `audit` needs the `operate` scope (ADR-0036 §5's token scopes), the scope that carries the operator's identity, because the trail names actors and records operator-only verbs (ADR-0014; #1614 item 5). Any other principal's `sub` gets `subfail { code: operator_only }`, and a token without `read` gets `subfail { code: scope }` on the other two. They ship in G13's release, which bumps `WIRE_JSON_VERSION` once under the rule above. An older server answers `subfail { code: unknown_stream }`, and the client hides the tiles.

- **ADR-0036's own streams, by kind (§3).** `queue/<id>` and `terminal/<pane>` are snapshot streams. `terminal/<pane>`'s snapshot is one complete terminal frame, and a removed pane is refused with `subfail { code: unknown_stream }`, never served as an empty terminal (ADR-0036 §5a). `agent/<id>` and `events` are history streams. `agent/<id>/live` has no seq.

**Types** (sketch; names final in S8):

```rust
pub enum StreamId { Agent(AgentId), AgentLive(AgentId), Queue(AgentId), Events,
                    Terminal(PaneId), Fleet, Inbox, Artifacts(ArtifactScope), Work(WorkScope),
                    TelemetryTraces(TraceFilter), TelemetryLogs(LogFilter), Audit }

pub struct NodeSummary { node_id: NodeId, name: String, online: bool, route: Route,
                         health: Option<NodeHealth>, counts: AgentCounts }
pub struct AgentSummary { agent_id: AgentId, name: Option<String>, node_id: NodeId,
                          runtime: Runtime /* pty | structured */, status: AgentStatus,
                          project: Option<ProjectKey>, goal: Option<GoalId>,
                          open_questions: u32, last_change_ms: u64 }

pub struct InboxItem { id: InboxItemId, kind: InboxKind /* question | permission | feedback */,
                       agent_id: AgentId, blocking: bool, urgency: Urgency, opened_ms: u64,
                       prompt: RenderedText, choices: Vec<Choice>, about: Option<EntityRef>,
                       actions: Vec<AllowedAction> /* what this principal may do */ }

pub struct ArtifactRecord { artifact_id: ArtifactId, version: u32, agent_id: AgentId,
                            title: String, kind: ArtifactKind, sha256: Sha256, size: u64,
                            created_ms: u64, interactive: bool, about: Option<EntityRef> }
pub enum ArtifactKind { Html, Markdown, Svg, Image { mime: String }, Json }
pub struct ArtifactComment { comment_id: CommentId, artifact_id: ArtifactId, version: u32,
                             anchor: Option<BlockAnchor>, quote: Option<String>, revision: Sha256,
                             anchor_state: AnchorState /* exact | reanchored | orphaned */,
                             text: String, author: Principal, created_ms: u64, delivered: DeliveryState }

pub struct WorkNode { work_id: WorkId, kind: WorkKind /* goal | package | task | issue | pr */,
                      title: String, state: WorkState, parent: Option<WorkId>,
                      assigned_to: Vec<AgentId>, refs: Vec<ExternalRef> }
pub struct WorkEdge { from: WorkId, to: WorkId, kind: WorkEdgeKind /* contains | blocks | implements | delivered_by */ }
```

**New `cmd` verbs.** Each uses socket-API params types and scopes (ADR-0036 §5). Each is refused for another node's agent until #1380.

| Verb | Scope | Effect |
|---|---|---|
| `artifact.comment { artifact_id, version, anchor?, text }` | `send` | Records the comment and queues it to the publishing agent as an input item (ADR-0036 §3, §12 item 2). |
| `inbox.answer { item_id, choice_index? , text? }` | `answer` | Is `question.answer` or the permission decision. It is atomic (§3). |
| `inbox.dismiss { item_id }` | `answer` | Only for non-blocking feedback requests. It records a refusal the agent sees. |

Layout and tile state have **no wire type** (§2).

### 6. Shells: one bundle, two hosts, one transport

- **One bundle.**
  - `web/client` builds to static assets once, with **Vite and `vite-plugin-solid`** (owner decision, 2026-10-16; §11 item 10). Vite gives hot reload in development and is the standard Tauri v2 front-end setup.
  - **Bun stays** the package manager and test runner, with `bun.lock` pinning unchanged. ADR-0036 §7's build bullet is amended to match.
  - The output is still static assets: embedded by the `web` feature with `rust-embed` and served by `flk`, and loaded unchanged by the Tauri v2 shell.
  - The bundle has no same-origin assumption: the server URL and token come from shell config. #1402's first spike item becomes a requirement here.
- **Browser.**
  - The `flk` binary with the `web` feature embeds the assets and serves them beside `/v1/ws`.
  - `flk client open` mints a one-time code over the Unix socket and opens the browser (ADR-0036 §5). Loopback is the default bind, and `--allow-non-loopback` stays explicit.
- **Tauri v2** (#1402). It carries the same assets in a native shell: WKWebView, WebView2, WebKitGTK and Android WebView. It is a separate binary and repo or crate, never linked into `flk` (bin-only).
  - **Connection:** to any `flk` server by URL with a client token. The token lives in the keychain or keystore, optionally behind biometric unlock.
  - **Native additions:**
    - the keyboard accessory bar;
    - deep links to an agent, question or artifact;
    - mDNS discovery;
    - a tray, global shortcuts and a dock badge on desktop;
    - local notifications while the app runs.
  - **Background push on iOS needs a relay**, which is new infrastructure (owner decision 6).
  - **Hardening, kept from ADR-0011 §3.**
    - The capability manifest forbids `shell`, `fs`, `http` and arbitrary `dialog`.
    - `default-src 'self'` applies.
    - Navigation outside the bundle is refused.
    - DevTools are gated in release builds.
    - Builds are signed and notarised.
- **One transport.** ADR-0011 §3's desktop path, Tauri IPC straight to `flock.sock`, is **superseded** by the WebSocket and client tokens of ADR-0036 §5 for every shell (owner decision 1). There is one code path, one auth model and one set of tests. A desktop app on the server's host mints its own token through `flk client open`, as a browser does.
- **Identity, and exactly what changes in ADR-0011.**
  - **Superseded for the GUI:** ADR-0011 §4's requirement that a WSS *gateway* (#150) authenticate GUI clients by a non-empty `allowed_users` list and the `Tailscale-User-Login` header, and propagate that login as the request principal. For the `/v1/ws` endpoint, the principal is the client token's (ADR-0036 §5), on any network.
  - **Kept:**
    - the loopback default and the funnel refusal;
    - the terminal bridge's own `allowed_users` check (`src/web/mod.rs`, ADR-0001), which serves `pty` terminal attach and is unchanged;
    - ADR-0011 §7's rules for proxying terminal attach.
  - A deployment may still put `/v1/ws` behind `tailscale serve` as defence in depth.
  - **No migration is needed.** ADR-0011's gateway, the IPC transport and the use-case layer were never built: there is no `src/usecase/`, no `src/gui/` and no `RpcTransport` in the tree on 2026-10-15. Nothing in use changes.

### 7. Artifacts, and the artifact MCP

An **artifact** is agent-authored content that a person looks at and answers: an HTML report, a plot, a diagram or a rendered table. It is versioned and immutable per version. It is also untrusted (ADR-0011 §5).

**Publishing (MCP).** These tools are in the flock MCP for `pty` and `structured` agents. They are pre-allowed in the structured launch profile beside `flock_question_ask` (ADR-0036 S6c).

| Tool | Input | Result |
|---|---|---|
| `flock_artifact_publish` | `title`, `kind`, `content` (inline, up to the size limit) or `path` (a file inside the agent's worktree: the server resolves it with symlinks followed and refuses any canonical path outside the worktree root, or any non-regular file), `supersedes?` (artifact id: becomes a new version), `about?` (work or issue ref), `interactive?` (default false) | `{ artifact_id, version, sha256 }` |
| `flock_artifact_request_feedback` | `artifact_id`, `version?` (default latest), `prompt`, `choices?`, `blocking?` (default false), `context?`, `recipient?` (ADR-0034 routing: the operator by default, or a named agent or owner) | It opens a question record whose `about` is the artifact (ADR-0036 §2a), routed by ADR-0034 like any question. An agent recipient answers it through its own question tools. Non-blocking returns the §2a placeholder at once. Blocking waits like `flock_question_ask`. |
| `flock_artifact_comments` | `artifact_id`, `since?` | anchored comments with authors, in order |
| `flock_artifact_list` | `about?`, `mine?` | records |

**Storage.**
- Content is a blob by sha256 (ADR-0036 §4). A record is a durable-log event `artifact.published`.
- Limits:
  - `[artifacts] max_bytes`: default 2 MiB per version.
  - `max_versions`: default 50 per artifact.
  - Retention: 14 days after the last view or comment, with an operator purge command (ADR-0011 §5).
- A version is immutable, and a published artifact is its hash.

**Rendering and isolation.**
- The client renders an artifact only inside `<iframe sandbox>` **without** `allow-same-origin`, `allow-top-navigation*`, `allow-popups`, `allow-forms` or `allow-modals`. The frame's origin is opaque, so it has no cookies, no storage, no access to the parent and no access to the client token.
- The content goes in through `srcdoc` (HTML) or a `blob:` URL (other kinds), with a CSP `<meta>` injected first:
  - Static artifacts (the default) get ADR-0011 §5's policy: `default-src 'none'; img-src data: blob:; style-src 'unsafe-inline'; script-src 'none'; connect-src 'none'; form-action 'none'; base-uri 'none'`.
  - With no script, no forms and no top navigation, a static artifact has no way to make a request beyond the inline data it carries.
  - **Interactive** artifacts add `allow-scripts` to the sandbox and `script-src 'unsafe-inline'`. `connect-src 'none'` stays.
- **Residual risk of interactive artifacts: navigation of the frame itself.** A script can navigate its own frame (`location = …`, a meta refresh, a link) and so leak data in a URL as a beacon. CSP has no widely shipped `navigate-to`, so the sandbox flags cannot fully stop this.
  - The mitigation is the frame's own CSP plus `default-src 'none'`, so the navigated-to page cannot load anything in the same frame. A beacon is still one outbound GET.
  - That is why interactive artifacts are opt-in per publish, carry a visible "interactive" badge, and are owner decision 2.
- **Tauri.** The shell's own policy is `default-src 'self'`, so it adds `frame-src blob: 'self'` so that `srcdoc` and `blob:` frames load. The frame's injected CSP still governs what runs inside it.
- This **replaces ADR-0011 §5's distinct hostname** (owner decision 3). An opaque origin is a stronger boundary than a separate site, and it works the same in Tauri, which has no hostname.
- **Untrusted chrome.** The frame always sits under visible chrome naming the publishing agent and "untrusted content". It is never full-window and never `window.open`.
- **The TUI** shows the text fallback (ADR-0036 §12 item 3): title, version, size, a `flk artifact open <id>` hint, and the comments.

**Feedback.** It reaches the agent through existing paths only.
- **Comments.**
  - The viewer selects a block. Every rendered block carries `data-block-id`, ADR-0036 §12 item 2's stable id.
  - The viewer writes a comment. The client sends `artifact.comment`.
  - The server records it and queues it to the agent as an input item through the gate of ADR-0036 §3.
- **Feedback requests.**
  - `flock_artifact_request_feedback` creates a question whose `about` is the artifact.
  - It appears in the inbox with an **Open artifact** action.
  - That action calls `expand_into_second(artifact, origin = the question's tile or the agent's transcript)`.
  - The answer, a choice or text, goes through `inbox.answer`, which is atomic.
- **Interactive artifacts** may post exactly one message shape to the parent: `{ type: "flock.feedback", block_id?, value: <JSON ≤ 16 KiB> }`.
  - Every opaque frame posts with origin `null`, so the parent identifies the sender by `event.source === iframe.contentWindow` for the artifact's own frame, never by origin. Messages from any other source are dropped.
  - The parent validates the message against the record's `interactive` flag and the size limit.
  - It shows the user what will be sent, and sends it as an `artifact.comment` only on the user's confirmation.
  - Nothing the frame posts reaches an agent without a person confirming it.
- **Which path, and why.**
  - Comments and confirmed interactive posts go through the **queue** (ADR-0036 §3) as input items. They are unsolicited information the agent should take at its next idle point, and the queue's gate and FIFO rules already apply.
  - Answers to a feedback request go through the **question** path (`inbox.answer`). They resolve something the agent asked, possibly while blocked, and need the atomic verify-then-apply.

### 8. Goals and work

- **Phase 1** (read-only, no new records). The `work` stream is a projection the server computes from:
  - goal records (ADR-0030/0031);
  - delegate edges (ADR-0027), as agent nodes under the delegating goal;
  - owner intake decisions (ADR-0035, when built);
  - the issues and PRs those records name.

  This gives the goal tree tile and the *Goals* sidebar tree with no new storage.
- **Phase 2** (`work` records). A `work` record kind is folded from the durable event log exactly as ADR-0031 folds goals.
  - Events: `work.created`, `work.updated`, `work.edge_added`, `work.edge_removed`, `work.closed`.
  - Each event carries the actor principal, so history is the log itself: who changed what, and when.
  - Architects and owners write it through MCP tools (`flock_work_*`, designed in its slice) instead of private files. ADR-0036 S6d's hold-or-continue check reads it.
- **No separate graph storage engine** in this record. flock's needs for goals and work today are append, fold and project. The durable event log already gives what a work graph needs:
  - typed nodes and edges, as event payloads;
  - an actor on every change;
  - full history, because the log is the history.

  A versioned graph engine, for example one that lets agents plan on a branch and merge, stays a future option. The record shapes above keep it open: every change is an actor-attributed event with typed edges, so a later engine could be fed from the log or replace the fold. Owner decision 8.

### 9. Mesh

- The `fleet` stream on node A includes nodes and agents learned over the mesh (ADR-0026), with `route` from the actual path, which fixes #1076's "via itself".
- A tile for an agent on node B reads B's state and messages through A, which is ADR-0036 §5's rule.
- Commands for B's agent are refused with `unsupported_for_agent` until #1380.
- Artifacts published on B are visible on A when their record and blob have been carried. That needs a blob fetch over the mesh, which is a slice of its own (G11) and out of the MVP.

### 10. Slices

Each slice is at most about 400 production lines; TypeScript counts the same. **Before S3** means it can start now, independent of the -p chain (#1518 → #1439 → #1426 → …).

| # | Slice | Depends on | Before S3? | Est. |
|---|---|---|---|---|
| S8a | `ts` feature and ts-rs derives for types that exist today: the socket API types the client needs first (agents, workspaces, panes, questions) and ADR-0036 S1's `SessionEvent` (landed). Drift test, `just gen-ts`. This splits ADR-0036 S8. | — | yes | 200 |
| S7a | The §5 wire message types as Rust types with ts derives: `hello`, `welcome`, `sub`, `unsub`, `cmd`, `ev`, `synced`, `gap` (with `snapshot_to`), `lagged`, `ack`, and what ADR-0036 §5a adds: `ping`, `pong`, `subfail`, the 4401 and 4403 close codes, and `welcome`'s recovery fields. Also the scripted fake-server fixture format. Types only, with no endpoint. This splits ADR-0036 S7. | #1546 settled (ADR-0036 §5a) | yes | 200 |
| G1 | Client shell skeleton (Solid, built with Vite + `vite-plugin-solid`, Bun as package manager and test runner), with a fixture transport that replays recorded S1 session fixtures (#1379) as `SessionEvent`s through the S8a types. No wire frames. | S8a | yes | 400 TS |
| G2 | Tile manager: layout tree, stacks, rail, the open/expand/collapse/close operations, per-device persistence, the phone mode, rendered as §2a's MVP workbench (three regions on desktop, inbox-first tabs on the phone, command palette; the sidebar and inbox regions show fixture data until G4 and G5) | G1 | yes | 400 TS |
| G3 | Client state machine: the §3 lifecycle, cursors, store, outbox; tested against a scripted fake server built on S7a's types: each §3 deadline, heartbeat and stale, gap with snapshot (including the empty and bootstrap forms), lagged, each `subfail` code, the progress deadline, protocol and version gaps, 4401 and 4403, and an `uncertain` command resent with `resume_only` | G1, S7a | yes | 400 TS |
| S7 | The wire endpoint (ADR-0036), implementing S7a's types, with ADR-0036 §5a's liveness and recovery | S3 (S3a #1518/#1439, S3b #1426), S7a | no | 350 |
| S8b | ts derives for the cmd params and result types and the §5 stream payloads the endpoint adds | S7 | no | 150 |
| G4 | The `fleet` stream and the sidebar *Fleet* and *Projects* trees, merging nodes by `node_id` (#1076) | S7 | no | 350 |
| S9a | Transcript tile (ADR-0036 S9a) on G2/G3 | S8b, G2, G3 | no | 400 TS |
| G5 | The `inbox` stream and inbox tile, with atomic `inbox.answer` | S7, S6b | no | 350 |
| G6 | Artifact records, blobs, limits, retention; the `flock_artifact_publish`, `_list` and `_comments` tools; the `artifacts` stream | S3b #1426 (the blob store), S7 | no | 400 |
| G7 | Artifact tile: sandbox, CSP injection, block anchors, comments with dual anchors and re-anchoring across versions (orphaned, never dropped), escaped untrusted framing toward the agent, `expand_into_second`; `flock_artifact_request_feedback` with ADR-0034 routing | G6, G5, S6c | no | 400 (TS + Rust) |
| G8 | The `work` stream, phase 1 projection, goal tree tile | S7 | no | 350 |
| G9 | Tauri v2 desktop shell: config, keychain token, accessory bar, deep links, tray | G1–G3 | yes (shell only) | 300 + config |
| G10 | Tauri mobile and push, if decided | G9, owner decision 6 | no | — |
| G11 | Artifact carriage over the mesh | G6, ADR-0026 | no | 300 |
| G12 | `work` records, phase 2 (§8) | G8, owner decision 7 | no | 400 |
| G13 | Telemetry views (§4a): `telemetry/traces`, `telemetry/logs` and `audit` reads in the server; trace, logs and audit tiles | S7, #1614, a vigil release (#1300) | no | 400 |

Before S3 means a slice depends on nothing in the -p chain's runtime (S3a, S3b). S8a, S7a, G1, G2, G3 and the G9 shell qualify. Everything that serves live data needs S7 and therefore S3.

The first end-to-end milestone is **"a structured agent in a browser tile"**: G1 + G2 + G3 + S7 + S8b + S9a, against the -p MVP's runtime. It follows the MVP and does not compete with it.

### 11. Owner decisions (decided by the owner, 2026-10-16, recorded on #1591)

Items 1–9 were decided by the owner on 2026-10-16. Items 1–4 and 6–9 follow the recommendation. Item 5 was decided differently from the recommendation. Item 10 was added and decided the same day. Item 11 was added on 2026-10-17, when the owner chose between two wireframe directions after rejecting the first preview.

| # | Decision | Outcome | Consequence |
|---|---|---|---|
| 1 | Supersede ADR-0011 §3's direct-socket desktop transport with ADR-0036 §5's WebSocket and tokens everywhere | **Decided: yes** | A desktop app on the server host gets its token through `flk client open`. |
| 2 | Opt-in **interactive** artifacts: scripts in an opaque-origin sandbox, no network, posts confirmed by a person | **Decided: yes**, opt-in per publish, off by default | One more security surface, reviewed in G7. Agents can render live plots and forms. |
| 3 | Replace ADR-0011 §5's distinct hostname with the opaque-origin sandbox | **Decided: yes** | Isolation rests on correct sandbox flags and injected CSP in every shell. A client bug such as a stray `allow-same-origin` would expose the client token, so G7 carries a security test per shell. Interactive artifacts keep §7's navigation-beacon risk. Artifacts render only inside the client. |
| 4 | Solid web client vs GPUI (#1401) | **Decided: Solid**; #1401 is closed | The Bun/TS toolchain stays. |
| 5 | Telemetry and history backend (was: does #1323's Grafana evaluation gate S7?) | **Decided: vig-os/vigil.** Log, metric and trace output goes through vigil's OTLP/JSON Lines files and its optional signed audit trail, not Grafana and not a separate pipeline. **#1323 no longer gates S7.** The live wire and history stay ADR-0036 §4/§5's own streams and blobs. | §4a describes the GUI's views on vigil's output. The integration ADR is #1614, adoption is #1300, and vigil is pre-alpha, so the views wait for it. |
| 6 | A push relay for mobile background notifications | **Decided: defer.** Mobile v1 is foreground only. | No pings while the app is closed. |
| 7 | `work` records as first-class flk state (§8 phase 2) | **Decided: yes**, after phase 1 has been used | Architects move their task files into flk. |
| 8 | A separate versioned graph engine for goals and work | **Decided: not now.** The event log covers typed nodes and edges, actors and history. | No branch and merge of plans for now. |
| 9 | Layout persistence | **Decided: client-local per device**; server-synced named layouts later | A layout doesn't follow you between devices. |
| 10 | Client build tool (added 2026-10-16) | **Decided: Vite with `vite-plugin-solid`.** Bun remains the package manager and test runner. Output stays static assets embedded by `rust-embed`, and the Tauri v2 shell loads the same bundle. | Hot reload in development and the standard Tauri v2 setup. A Vite config joins the toolchain, pinned through `bun.lock`. ADR-0036 §7 is amended to match. |
| 11 | MVP workbench design (added 2026-10-17, after the owner rejected the first preview's look and UX) | **Decided: "Mission control"** (§2a): sidebar, focus pane and inbox on desktop; inbox-first tabs on the phone | The tile manager becomes machinery under that screen. G2's views follow §2a. |

## Failure modes and known limits

- **An agent floods the inbox.**
  - The inbox orders blocking items first and caps per-agent visible items.
  - This record does not limit how many questions an agent may ask. That is ADR-0036 §2a's and S6d's job.
- **Artifact phishing.** A sandboxed artifact can still draw a convincing fake approval button inside its frame. The untrusted chrome, a frame that is never full-window, and answers that only happen through the inbox's own controls are the defence. ADR-0011 §5 already called pixel-level phishing otherwise undefendable.
- **Opaque-origin limits.** Interactive artifacts can't use storage, cookies or workers that need an origin. That is intended.
- **Stale reads.** While Stale, tiles show old state with a marker. A user can read but not act. The outbox holds actions until Live.
- **A stale command after the idempotency window** is shown as "unknown outcome", never resent automatically.
- **Cross-node commands** are refused until #1380. The UI shows the refusal in place.
- **Layout loss.** Clearing browser storage resets the layout. Server state is unaffected.

## Prior art: vig-os/revkit

vig-os/revkit (Apache-2.0) is an HTML-first review surface: rendered docs and ADRs with persistent anchored comments, a GitHub PR round-trip, and rich question UIs for agents. Its design is accepted, but no code is runnable yet. It overlaps with §7 and the inbox. What this record takes from it, and what not:

- **Adopt: dual anchors with re-anchoring (revkit ADR-0006).**
  - A comment stores its block id, a text-quote selector and the revision (content hash) it was made on.
  - When an artifact gets a new version, the comment is re-anchored:
    - the diff decides where the anchor maps;
    - quote similarity only decides whether it still matches;
    - otherwise the comment is **orphaned, kept and never dropped**.
  - This fills a gap in §7, where comments were tied to one version. G7's acceptance gains it.
- **Adopt: untrusted framing on the way to the agent (revkit ADR-0007 amendment).**
  - Comment bodies and quotes are untrusted input to the agent. Before flk queues a comment or a confirmed interactive post (§7), it escapes every user-supplied field so it cannot close or forge the framing the harness wraps it in, and it caps the size.
  - A comment is a request from a person, not an instruction, and the agent may decline it.
  - This applies to ADR-0036 §3 input items from the GUI generally.
- **Adopt, later: question widget vocabulary.** revkit's agent questions go beyond choice lists: choices with previews, ranking, sliders, and pick-a-region on a plot. The inbox (G5) starts with ADR-0036 §2a's choices and text. Richer widgets are a follow-up that extends `Choice` in §5. Each widget has a text fallback, per ADR-0036 §12 item 3.
- **Shared stack.** Solid, with Kobalte primitives in revkit, matches ADR-0036 §7's choice. A future shared component package is possible but is not assumed here.
- **Not adopted, and why.**
  - revkit renders authored docs through a static-site build (Astro and Starlight) and hosts them on a Cloudflare Worker with a GitHub App. flk's artifacts are runtime blobs rendered by the client, and flk has no hosted tier.
  - revkit's local daemon has its own cookie session. flk's wire already has client tokens (ADR-0036 §5).
  - revkit's delivery modes (`handover`, `live`, `quiet`) map onto ADR-0036 §3's `at_idle`, `steer` and the queue's gate, so no new mode is needed.
  - Depending on revkit itself waits until it ships. The integration point, if one ever exists, would be an artifact kind that is a revkit-rendered document.

## Alternatives considered

- **Amend ADR-0036 instead of a new record.** Rejected. ADR-0036 is the session runtime's record. A GUI record also has to reconcile ADR-0011 and ADR-0012, and add records for artifacts and work that are not session concerns.
- **A server-rendered UI (htmx or similar).** Rejected. ADR-0036 decided client rendering, and it would need a second render path for Tauri.
- **Layout synced through the server from day one.** Deferred. There is no multi-device demand yet, and a layout would become state that someone must migrate.
- **An agent board as the main screen (§2a).** Agents as cards in columns (needs you, working, idle), with the selected agent in a side panel; on the phone, a filtered card feed. Not chosen for the MVP (owner decision 11): it shows every agent at once but makes the person hunt for what needs them, and it has no standing inbox. It may return as a second view of the same data.
- **Artifacts on a second hostname (ADR-0011 §5).** Superseded by the opaque-origin sandbox. A second hostname needs DNS or a tailnet node per install and doesn't exist in Tauri.

## Consequences

- The client becomes testable against a scripted fake server before S7 exists (G1–G3 with S8a and S7a).
- Agents get one way to show a person something and get it answered, through the question and queue machinery that already exists.
- Goals and work become visible without new storage (phase 1). Phase 2 moves planning state out of private files into flk's log.
- ADR-0011 §3, §4 and §5 are partly superseded if decisions 1 and 3 are accepted. ADR-0011 will need a status note.
