# ADR-0038: The GUI is a tiled workbench over one wire: a client that holds only presentation state, artifacts in opaque-origin sandboxes with feedback through questions, and goals and work as records on the same log

- Status: Proposed; the §11 owner decisions 1–11 were taken on 2026-10-16 and 2026-10-17 (recorded on #1591), 12–15 (§5b) on 2026-10-19 (recorded on #1717), 16–17 on 2026-10-25 (recorded on #1601 and #1598), and 18 on 2026-10-26 (recorded on #2500 and #2489)
- Date: 2026-10-15, amended 2026-10-16 (#1606; then #1641: the client side of ADR-0036 §5a's transport recovery), 2026-10-18 (#1717: §5b, question threads), 2026-10-25 (#1601: §6 shell tokens and origins; #1598: §7a–§7f artifact lifecycle; #2443: §7 residuals), and 2026-10-26 (#2500: §6 the local shell bridges `/v1/ws` over the server's unix socket `flock-ws.sock`; #2489: the app-session TTL clock)
- Issues: [#1307](https://github.com/gerchowl/flock/issues/1307) (ADR-0036 slices), [#1310](https://github.com/gerchowl/flock/issues/1310), [#1323](https://github.com/gerchowl/flock/issues/1323) (superseded by the vigil decision), [#1614](https://github.com/gerchowl/flock/issues/1614), [#1380](https://github.com/gerchowl/flock/issues/1380), [#1401](https://github.com/gerchowl/flock/issues/1401), [#1402](https://github.com/gerchowl/flock/issues/1402), [#1546](https://github.com/gerchowl/flock/issues/1546) (settled by ADR-0036 §5a), [#1641](https://github.com/gerchowl/flock/issues/1641), [#1076](https://github.com/gerchowl/flock/issues/1076), [#1717](https://github.com/gerchowl/flock/issues/1717) (question threads, §5b), [#1719](https://github.com/gerchowl/flock/issues/1719), [#1673](https://github.com/gerchowl/flock/issues/1673), [#2500](https://github.com/gerchowl/flock/issues/2500) (the local shell's unix-socket bridge), [#2489](https://github.com/gerchowl/flock/issues/2489) (the TTL clock)
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
| **Focus pane** (centre) | The focused agent's tile (§2), by default one `transcript` tile. | A header line: agent, task and goal, PR, branch, model, a state chip with its age, and the actions *Terminal*, *Diff* and *Fork*. The transcript reads as turns. A tool run is one line (command, exit code, duration) that expands on demand. Older turns fold to "N earlier turns". A composer sits at the bottom. Next to it, it shows the agent's reported phase (for example working, idle or offline), and under each message that message's outbox state (§3). It never predicts when or whether a message will be delivered (§3, "queue order or delivery"). Delivery is shown only when the server reports it: the message's own user turn in the transcript, or, after the MVP, the agent's `queue/<agent>` stream (ADR-0036). |
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

**Sub ids and closed subs.** Here a connection is one socket. A `sub_id` is unique for the life of that socket: the client never reuses one on it, even for the same stream, so one transport and one id counter serve every view. The server refuses a `sub` that reuses an id already open or closed on the socket with a typed `subfail` (ADR-0036 §5a). Three events close a `sub_id`: the client sending `unsub`, and the server sending `subfail` or `lagged`. After `subfail` or `lagged` the server sends nothing more on that id. Every re-sub, after `lagged`, `cursor_ahead` or the progress deadline, uses a fresh id. The re-sub after `lagged` needs no `unsub`. `unsub` crosses the wire, so frames for an id already closed (by `unsub`, `subfail` or `lagged`) may still arrive: `ev`, `gap`, `synced`, `lagged` or `subfail`. The client silently drops every one of them. Such a frame is not a protocol error and never touches another stream's state. A frame for a `sub_id` the client never opened on this socket is still a protocol error. The set of used and closed ids starts empty on each new socket. After a reconnect, each subscription sends its current id again in its new `sub`.

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
  - The outbox is **held** until a new token connects. If the new principal differs from the old one, the outbox is **dropped** and the user is told (the connection strip names how many unsent commands went, beside any restore problem, until dismissed or the next acked command), because commands must not run under another identity. Only `welcome.principal` equality decides this; the client never parses a principal.
  - A shell with a token provider (§6) is asked instead of the user. An app-session re-mint keeps the principal: ADR-0036 gives every app-session token of one operator UID the same principal across re-mints and restarts (server PR `feat/2442-app-principal`), so the held outbox and the cursors survive it. Another token or another person is a different principal and still drops the outbox, with the same notice.
  - A Tauri shell may unlock a stored token with biometrics (§6).

**Getting and keeping the token (browser shell).** ADR-0036 §5 decides the exchange: `flk client open` runs as the operator over the unix socket, mints a one-time code (single use, 60 s) and opens the loopback URL with the code in the **fragment** (`#code=…`), never the query, so it never reaches a server log or a `Referer`.
- **On load,** the client reads `code` from `location.hash` and removes it at once with `history.replaceState`, before any other script or request runs. It then `POST`s `/v1/client/exchange` (same origin) with `{"code"}` and gets `{"token"}` exactly once. Every failure is one uniform 401. The client shows one message, "This link has expired or was already used. Run `flk client open` again.", and never retries the code.
- **The token goes into `hello` only** (unchanged), never into a URL, a log, an error message or the outbox records.
- **Storage: `sessionStorage`, decided here.** An `operate`-scoped token is the operator's identity, so it is kept no longer than the tab that received it. `sessionStorage` survives a reload, which the reconnect, the outbox and a dev reload need, but not closing the tab. Each tab needs its own `flk client open`.
  - Rejected: memory only, because every reload would need a new `flk client open` and the outbox could not resume.
  - Rejected: `localStorage`, because it keeps the identity on disk with no end, shares it across tabs, and survives the browser closing.
  - What remains is script running in the page, which can read `sessionStorage`. The enforcing Content-Security-Policy on the served page (`script-src 'self'`, no inline or eval; #1875) is the control for that, so the client never needs inline scripts. The browser shell needs no bootstrap configuration: with no config global it derives same-origin defaults (`serverUrl` is `location.origin`, the socket is `ws://` or `wss://<host>/v1/ws`), and the server serves no config script. An external config script is only for the Tauri shell.
- **NeedsToken** clears the stored token, as above. A Tauri shell keeps its token in the platform keystore instead (§6).

**The store.**
- Entities are kept normalised by kind and id.
- Each stream has a cursor, persisted per device so a reload resumes without a full replay. It is persisted only after the event (or the empty snapshot) it names has been applied, and together with the stream's resync marker. The `*/live` streams are the exception: they carry deltas with no cursor (§5).
- The outbox is persisted too, each command before its first send (below).
- Ephemeral state lives in the store but is not persisted: in-flight streaming blocks (§4).

**The command outbox.**
- Every `cmd` gets a `cmd_id` (UUIDv7) and is stored with its `verb`, `args`, the principal that queued it and its first-send time. It is persisted before its first send. A composer message's record may also carry an optional `echo_after`: the seq of the agent's transcript at the send, kept so the message can be matched to its event after a reload (#1773). It is never sent, and it says nothing about delivery. A record without it stays readable, so the stored format stays `flock.outbox.v1`. Commands, first sends and resends alike, go out only in Live: Syncing waits for Live, and ReadOnly holds them (a newer client must not send verbs the server may not know).

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
- **Optimism is visual only.** A pending command marks its subject "pending" and never changes an entity. The entity changes when the server's event arrives. The composer lists an accepted message until a person's turn arrives after its send cursor (`echo_after`), and the cursor is taken only while the transcript is Current. Known limit: any person's turn after the cursor folds a waiting message, including a line typed into the agent's terminal. Matching is not widened by heuristics. The fix is server-side correlation: the transcript event carries the command's `cmd_id` (#1677).
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
- **Permission presentation (#2559).** One prompt has one transcript entry:
  the canonical `flk_permission_request` presents it. A native `control_request`
  with the same nonempty `request_id` in that session stream is transport detail
  and does not render a second prompt, even if the canonical fact arrives later.
  Native requests without a known canonical twin remain visible; unknown facts
  retain their JSON fallback. Canonical closed records remain visible too.
  This is presentation grouping (§3), not permission-state inference: raw facts,
  stream cursors and event counts are unchanged. Reset clears the correlation.
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

### 5b. Question threads (#1717)

The owner's direction of 2026-10-16 (#1717, quoted there verbatim) asks for three things:
1. One question surface. Every question an agent asks reaches the person through the GUI inbox.
2. A question does not block, unless the agent has literally nothing else to work on.
3. A person can take a question as a **thread**. The asking agent is forked at the question, the person discusses it with the fork, and the fork's compact answer is **merged back**. That closes the question, while the original agent keeps working throughout.

This section is the GUI side. It builds on ADR-0036 §2a, which already holds:
- the question record, with its `ask_point`;
- `question.fork` and `question.resolve`, and exactly one reply;
- ADR-0036 §9's forks at a message boundary.

The changes ADR-0036 needs are for its owner to make, and are listed at the end of this section. The fork-and-merge-back mechanism is shared with #1719's aside capture (below).

**One channel, for questions to the operator.** An agent asks through `flock_question_ask` (ADR-0034 §3, ADR-0036 S6c). Every question record whose recipient is the operator (ADR-0034 §2's routing) becomes an `InboxItem { kind: question }` (§5), whatever produced it: the MCP tool, a stdio `AskUserQuestion` until ADR-0036 retires that as a question path, or ADR-0036's E9 recap parsers. The inbox never shows which channel a question came through. The card shows the prompt, the choices, `context` and `refs`, and the blocking state below. A question addressed to an agent stays on ADR-0034 §3's path: a delegate never asks the human directly, and its question reaches the inbox only if routing sends it to the operator.

**The five decisions.** Each is stated with its reason.

1. **Origin: the asker's lease.**
   - A question's `asked_by` is the agent whose session lease admitted the `flock_question_ask` call. That is ADR-0036 §1a's lease-based caller identity (S3b #1426), with the scoped session credential of #1734.
   - A `pty` asker has no lease. Its origin is the pane attribution that ADR-0008 and ADR-0026 already give its socket calls.
   - The tool takes no asker field. A call that reaches neither a lease nor a pane is refused, as every anonymous agent send is (ADR-0036 §1a).
   - The card shows `asked_by` as server-attested. The prompt, `context` and `refs` are the agent's own words: they are rendered as untrusted text, never as a control.
   - *Reason:* a thread forks the asker, and an answer is delivered into the asker's queue. An origin the agent could name itself would fork, and answer, the wrong agent.
2. **Fork points: at the question when the session allows it, said plainly when it doesn't.**
   - *Take as thread* sends `question.fork` at the question's `ask_point`.
   - Inside the current compaction segment, that is ADR-0036 §9's `resume_at` path (E9), a warm cache read.
   - Behind the last `compact_boundary` (ADR-0036 E11), it is that section's `rehydrate` path from the native transcript's byte prefix (ADR-0036 owner decision 14). It is a cache write, not a read.
   - Before forking, the card shows the path and the expected cost (ADR-0036 §9's `dry_run`), and the person confirms.
   - If that prefix is no longer stored, the fork at the question is refused with a distinct code, `fork_prefix_gone` (S6e). It is not ADR-0036 §9's `invalid_fork_point`, which names a cut-rule violation. The card then offers only *Thread from now*.
   - A `pty` asker can fork only at its latest point (ADR-0036 §9, `latest`). Its card offers only *Thread from now* and says that the fork starts after the question, with everything the agent did since.
   - *Reason:* the person must know what the fork has seen. A thread presented as "at the question" that started later, or that paid a cold write unannounced, would mislead both the discussion and the bill.
3. **Restart and handoff: ADR-0028's lifetime rules, nothing new.**
   - A **recorded** question (a `flock_question_ask` call, or a structured session's question record, ADR-0036 §2a) is durable (ADR-0028 §8a). It stays open across a restart or an orderly handoff of its asker.
   - A **screen-sourced** dialog of a `pty` asker is not recorded that way. ADR-0028 §8a withdraws it when it leaves the screen, and a restart makes it leave. The question then closes as `withdrawn`, and a thread on it fails under the stale-answer rule below.
   - Once S6e amends ADR-0036, a held `blocking` call that ends with the asker's process leaves its record open, and a later answer is delivered as the deferred `reply` item. This held-to-deferred rule is new: ADR-0036 §2a has no such rule, and S6e adds it.
   - A thread's fork is an agent of its own, so it survives with its tile. Any wait pinned to the asker's old epoch ends with `observation_reset` (`after_inspecting`; ADR-0028, #1577). The thread tile re-reads the question record and the asker, then re-subscribes under §3.
   - If the asker's identity is replaced, its pinned question ends with `subject_replaced` (`never`). Once S6e amends ADR-0036, the record then closes as `withdrawn`, and the thread's merge-back is refused under the stale-answer rule below. Until then the question stays open with no asker to deliver to, and the card says that the asker was replaced.
   - The fork's own restart follows the same rules.
   - *Reason:* ADR-0028 already says which waits survive and which never come back. A question tied to a replaced agent must not be answered into its successor.
4. **Who may answer: the question's recipient, through its own surfaces.**
   - **A question to the operator** (its recipient under ADR-0034's routing) is answered only from the operator's authenticated surfaces:
     - a client token (ADR-0036 §5; `answer` or `operate` scope: the `answer` scope exists for a narrower owner token, for example on a phone);
     - the local operator's CLI or TUI (ADR-0028 §8a);
     - a channel adapter binding with `reply` scope (`channel:<binding>`, ADR-0028 §8a), for example Telegram from a phone, to which ADR-0034 §2 routes operator questions under E9c.

     It is never answered from another agent's pane input or tool call, and so not through MCP `flock_question_answer` either.
   - **A question to an agent** keeps ADR-0034 §3's path. The architect answers with `flk question answer` or `flock_question_answer`, as §7's agent recipients do through their question tools.
   - **A merge-back proposal** is applied by the question's recipient, under the same rule: for an operator question, from one of the operator's surfaces. In the GUI that is `inbox.answer`, and §5's row is unchanged.
   - **Amendment:** excluding MCP `flock_question_answer` for operator-addressed questions narrows ADR-0028 §8a, which lets MCP answer any question. That is an ADR-0028 §8a amendment, made with S6e and listed under "What ADR-0028 must change" below. An agent's tool call must never answer for the operator (#1673). Agent-addressed questions keep MCP (ADR-0034 §3). Refusing another agent's keystrokes into a `pty` dialog is #1673's own follow-up (ADR-0008, ADR-0029), not this record's.
   - The fork composes the proposal, and the recipient applies it. Once S6e amends ADR-0036, the fork's `question.resolve` records a **proposal**, not an answer. Until then it answers directly (ADR-0036 §2a), so the GUI offers no thread before S6e (G14 depends on it).
   - *Reason:* in #1673 a stray keystroke from another agent was recorded as an owner decision. An answer to the operator carries the owner's authority, so it comes only from the operator's own surfaces. Agents keep answering what is addressed to them.
5. **Blocking: the asker decides, and says why (owner decision 15).**
   - S6e makes `flock_question_ask { blocking: true }` require a `blocking_reason` (1–512 bytes, trimmed UTF-8, one paragraph): what the asker cannot proceed on, and why nothing else is open.
   - Owner decision 15 retakes ADR-0036 owner decision 3a. Once S6e amends ADR-0036, a question without `blocking` is non-blocking, and the server never decides on the agent's behalf. S6d's check no longer holds a turn, so a held turn is always the asker's choice under the tool contract.
   - Until S6e lands, ADR-0036 §2a still applies to an absent `blocking`: S6d's check decides. A turn it holds has no reason to show, and its card states only the server's `blocking` flag.
   - The GUI orders a blocking card first (§1) and shows "Blocking: <reason>" in the agent's own words. The agent's row shows the amber *needs you* mark (§2a), and its composer line states the server's phase (`waiting_on_human`).
   - A non-blocking card puts no mark on the agent. The GUI never judges whether a reason is honest.
   - *Reason:* "literally nothing else to work on" is a fact only the agent can know. A server-side guess either blocks an agent that had other work, or misses an honest blocker. A stated reason makes the choice visible and open to review.

**A thread's life.** Threads ship with G14, after S6e has amended ADR-0036; this describes the state after both. States follow the question record: `open → threaded → proposed → answered | withdrawn | expired`.
- *Take as thread* forks the asker (decision 2) and opens the fork's `transcript` tile (§2) with its composer (§2a). It is not a new tile kind: the subject is the fork, and the tile header names the question it answers.
- The thread fork runs read-only through the general permission-tier machinery: ADR-0036 S19's presets (#1321), with a read-only tier. It never gets a thread-specific flag, so there is one source of truth (owner decision 14). A thread is for discussion; work stays with the asker. Widening is #1321's follow-up: tiers selectable per model, and a fork that requests more, which the person grants.
- The fork's last step is the proposal (decision 4).
- *Review and merge back* shows the proposal as editable text, next to the question and its choices. The person always reviews it, and may edit it, before *Apply* (owner decision 12). *Apply* sends `inbox.answer { item_id, text, proposal_id }`.
- Apply is §3's atomic verify-then-apply: the same question id, still open. The server then records `question.answered { via: fork, proposal_id, edited }` and queues the one `reply` item to the asker (ADR-0036 §2a).
- **What is logged (owner decision 12).** These are durable records on the log:
  - the proposal (`question.proposed`);
  - the applied answer;
  - whether it was edited. The edit is the difference between the two kept texts.

  They are kept so that a later analysis can improve skills and agent context. That analysis is a follow-up (#1717), not designed here.
- After the merge-back, the fork is stopped and its transcript is kept in the lineage (owner decision 13). No retention limit is stated yet: ADR-0036 S16, which owns retention, has not decided one.

**The card in each state** (§2a's inbox region):

| State | The card shows | Actions |
|---|---|---|
| `open` | prompt, choices, `context`, `refs`; "Blocking: <reason>" when blocking | one button per choice, *Edit…*, *Take as thread* (or *Thread from now*, decision 2) |
| `threaded` | as `open`, plus each thread's turn count | as `open`, plus *Open thread*. Answering in place stays possible. |
| `proposed` | as `threaded`, plus the proposal's first lines | *Review and merge back* |
| `answered`, `withdrawn`, `expired` | nothing: the card leaves when the server closes the item (§1). Open thread tiles state how the question ended. | none |

**The stale-answer rule.** The question may be answered in place, answered from another thread, or closed (`withdrawn` or `expired`) while a thread runs. Applying the proposal then fails with `question_closed`, as §3 already requires for any stale answer, and the card shows the question's current state. The proposal is kept as text. The person may send it to the asker as an ordinary message through the composer, but nothing sends it automatically. Of several threads on one question, the first applied wins, and the others' cards say so.

**Shared with #1719.** #1719's aside capture routes a remark to a side context and leaves a breadcrumb. It reuses this mechanism:
- a fork at a boundary (ADR-0036 §9);
- a return that is a proposal the person reviews;
- the fork's tile and its proposal review;
- the atomic apply.

The only difference is what the return becomes. A question thread applies an answer through `inbox.answer`. An aside's return is an ADR-0036 §9 `finding` item, or a filed issue, applied the same way. The question record, the stale-answer rule and the blocking contract are not shared.

**Wire additions** (sketch; names final in G14, §10):

```rust
// §5's InboxItem gains:
blocking_reason: Option<String>,               // 1–512 bytes, trimmed UTF-8, one paragraph; untrusted card text
threads: Vec<ThreadRef>,
fork_points: ForkPoints,                       // which forks are possible, and their cost

pub struct ThreadRef   { fork_agent: AgentId, turns: u32, proposal: Option<Proposal>, closed: Option<ThreadEnd> }

// Durable records on the log (owner decision 12):
question.proposed { question_id, fork_agent, proposal_id, text }   // kept after the apply
question.answered { question_id, by, answer, via: fork, proposal_id, edited: bool }
pub struct Proposal    { proposal_id: ProposalId, text: RenderedText, created_ms: u64 }
pub struct ForkPoints  { at_question: Option<ForkCost> /* None: pty, or the prefix is gone */, latest: ForkCost }
pub struct ForkCost    { mechanism: ForkMechanism /* resume_at | rehydrate | fork_session */, prefix_tokens: u64, warm: bool }
```

| Verb | Scope | Effect |
|---|---|---|
| `inbox.thread { item_id, at: question \| latest, dry_run? }` | `operate`: it starts an agent and costs tokens | `question.fork` with `return_to_parent` and `answers`, read-only by default. `dry_run` returns the cost only. |
| `inbox.answer { item_id, choice_index?, text?, proposal_id? }` | `answer`, unchanged from §5 (decision 4) | As in §5, and with `proposal_id` it records `via: fork`. Atomic (§3). |

**What ADR-0036 must change** (its owner amends it; this record does not):
- owner decision 3's channel order, for questions to the operator;
- `blocking_reason`, required if and only if `blocking: true` (1–512 bytes, trimmed UTF-8, one paragraph);
- owner decision 15 retakes its decision 3a: `blocking` false by default, and S6d's check no longer holding;
- the lease origin;
- `question.resolve` as a proposal, applied by the question's recipient (decision 4);
- the question states `threaded` and `proposed`, beside ADR-0028's `withdrawn` and `expired`;
- `subject_replaced` closing a question as `withdrawn`;
- the held-to-deferred rule (decision 3);
- the fork code `fork_prefix_gone`, distinct from `invalid_fork_point`;
- the durable `question.proposed` record, and `question.answered`'s `proposal_id` and `edited`;
- the fork-point cost on the inbox item.

**What ADR-0028 must change** (with S6e): §8a's MCP answer binding (`flock_question_answer`) no longer answers a question whose recipient is the operator (decision 4). The CLI, the TUI and `reply`-scoped channel bindings still do. Agent-addressed questions keep MCP.

### 6. Shells: one bundle, two hosts, one transport

- **One bundle.**
  - `web/client` builds to static assets once, with **Vite and `vite-plugin-solid`** (owner decision, 2026-10-16; §11 item 10). Vite gives hot reload in development and is the standard Tauri v2 front-end setup.
  - **Bun stays** the package manager and test runner, with `bun.lock` pinning unchanged. ADR-0036 §7's build bullet is amended to match.
  - The output is still static assets: embedded by the `web` feature with `rust-embed` and served by `flk`, and loaded unchanged by the Tauri v2 shell.
  - The bundle has no same-origin assumption: the server URL and token come from shell config. #1402's first spike item becomes a requirement here.
  - **The shell config contract** (`web/client/src/config.ts`). A shell sets one global, `__FLOCK_SHELL_CONFIG__`, before the bundle runs. Without it, the bundle takes its page's origin, which is the browser shell's case. The global holds one of:
    - `{ transport: "wire", serverUrl, token?, tokenProvider? }`, for a live server. `tokenProvider` (#1601) is a pull-only function `() => Promise<{ token, expiresInMs? }>` that the shell's script defines, never beside `token` (`src/wire/provider.ts`). `expiresInMs` is the remaining lifetime the server reported at mint (`issued.info.expires_in_ms`), when it reports one.
      - The client asks it at connect, when the person presses *Connect* in NeedsToken (which replaces the token prompt and cancels a pending backoff wait), and whenever the server ends the shell's token:
        - a close 4403 (revocation, expiry, process exit);
        - a 4401 at `hello` for a token that once got a `welcome`, which is what a server restart or crash leaves, because app-session tokens live in its memory only. The person sees one reconnect;
        - a 4401 for a token that never got a `welcome`, such as a freshly minted token refused at once. It counts toward the same streak, so a broken mint is bounded too.
      - One call runs at a time, and a token that resolves after a *Disconnect* waits for the next *Connect*.
      - **Renewal before expiry.** With `expiresInMs`, the client asks again at **¾ of it**, the lead derived from the server's own figure and never from a hard-coded TTL. The renewal time is kept on the monotonic clock and armed in steps of at most 2³¹ − 1 ms (about 24.8 days), re-armed until it is due, because browsers fire a longer `setTimeout` at once; so any lifetime, up to `Number.MAX_SAFE_INTEGER`, renews on time and never early. It switches to the new token through §3's resume path, so with the same principal the outbox and cursors survive. A *Disconnect* clears the renewal timer, and a renewal renews only a running session: in NeedsToken, a spent re-mint cap included, the reactive path owns the provider. A failed renewal is not retried; the session stays Live and the shell's message shows as a dismissible notice in the connection strip, and the reactive path above takes over when the server ends the token. A token whose `expiresInMs` is not a positive, finite number is refused like any other bad value. Without `expiresInMs` no timer is set. The client owns this one timer; the shell needs no push channel.
      - The token is kept **in memory only**, so a reload asks again. A link's `#code=` is never exchanged or stored: the client removes it from the fragment as usual and says the shell provides the token.
      - A rejection, a synchronous throw or a value that is not `{ token, expiresInMs? }` shows its message in NeedsToken and is not retried. Offering the remote path (#2293) belongs to that message, which the shell writes.
      - **Re-mints are bounded by §3's backoff.** After consecutive ends, the first re-mint is immediate and later ones wait §3's backoff for attempts 0 to 5 (ceilings 0.5 s to 16 s). After **7** in a row, the next wait would be the backoff's 30 s ceiling, so the provider is no longer asked and NeedsToken says why until the person presses *Connect*. A session Live for §3's healthy period (60 s, the one that resets the backoff), or an `ack`ed command (counted monotonically, whatever the outbox drops), ends the streak.
      - The Tauri shell uses a provider only for a remote server (#2293). On the same machine it uses the `bridge` transport below, so the token never reaches the webview.
    - `{ transport: "bridge", openBridge }`, for a shell whose native side carries `/v1/ws` itself (#2500; the Tauri shell on the same machine, zero-touch below). `openBridge` is a pull-only function `({ renew }) => Promise<{ socket, expiresInMs? }>`, never beside `serverUrl`, `token` or `tokenProvider`.
      - `socket` is a duplex with the WebSocket subset the client uses: `send(text)`, `close(code?, reason?)`, and `onmessage`, `onclose(code, reason)` and `onerror` handlers. It carries the same JSON text frames and the same close codes as `/v1/ws`, verbatim, so §3's resume and every close-code rule hold unchanged.
      - The client sends `hello` **without a token**. The native side adds its bearer to that first frame before forwarding it, and passes every later frame through unchanged in both directions. A first frame that is not a `hello`, or a `hello` that already carries a `token`, is not forwarded: the native side closes the session with 1008 (policy violation) and a reason naming the frame. That is a protocol close, so §3's "Socket lost" applies unchanged: the client shows the diagnosis and goes to Reconnecting under capped backoff, as for a server that sends protocol errors. Only a client defect can cause it.
      - Each `openBridge` call is one session, the way `tokenProvider` plus a new WebSocket is one session: the client calls it at connect, on *Connect* in NeedsToken, on every reconnect, and at ¾ of `expiresInMs`, the one call that passes `renew: true`. It applies the provider path's rules above unchanged: one call at a time, the streak of ends (4403, 4401 after a `welcome`, 4401 before one) bounded by §3's backoff and the cap of 7, renewal armed in safe steps, a *Disconnect* clearing the timer, and a rejection shown in NeedsToken and not retried.
      - **A rejection is a refusal, not an outage.** `openBridge` rejects only when the native side cannot obtain a bearer or the server refuses the upgrade itself: a mint refusal (`operator_only`, `peer_process_changed` after its one fresh retry, `session_busy`), the Linux `ptrace_scope` check, no server it can start, or an upgrade refusal other than 503 `preauth_capacity` ("Refusals before the upgrade", below). Its message says why and offers the remote path. When the unix socket is briefly unreachable, as during a restart or handoff, `openBridge` resolves and the `socket` closes with 1006, so §3's ordinary reconnect handles it.
      - `expiresInMs` is the remaining lifetime of the bearer the native side used for that session, so the renewal lead still comes from the server's `expires_in_ms`.
    - `{ transport: "fixture", fixtureJsonl, source }`, for a recorded replay;
    - `{ transport: "error", message }`, when the shell has no configuration to give, such as the Tauri shell reading a malformed config file. The client shows `message` as the reason in its ConfigError state. A shell reports its own errors this way and never through an unknown transport, which the client refuses with a message of its own.
- **Browser.**
  - The `flk` binary with the `web` feature embeds the assets and serves them beside `/v1/ws`.
  - `flk client open` mints a one-time code over the Unix socket and opens the browser (ADR-0036 §5). Loopback is the default bind, and `--allow-non-loopback` stays explicit.
- **Tauri v2** (#1402). It carries the same assets in a native shell: WKWebView, WebView2, WebKitGTK and Android WebView. It is a separate binary and repo or crate, never linked into `flk` (bin-only).
  - **Connection.** The shell speaks `/v1/ws` with a client token, like the browser. Where the server is decides how the token is got and which socket carries the session (owner decisions 16 and 18; the server side is ADR-0036's amendments "App-session tokens and shell origins" and #2500's unix-socket `/v1/ws`).
    - **The server on the same machine (zero-touch), macOS and Linux.**
      - **The mint.**
        - At launch, the shell's native side opens `flock.sock` and calls the mint verb.
        - The server admits the call only from the LocalOperator. That means the kernel-checked peer UID and PID (`getpeereid` + `LOCAL_PEERPID` on macOS, `SO_PEERCRED` on Linux, `src/platform/peer.rs`), and no flk pane in the caller's ancestry. It is exactly the authority `flk client open` has.
      - **Scopes, chosen by the server.**
        - The verb takes no scope parameter. It grants exactly `{read, send, answer, operate}`, the set `flk client open` gives a browser, because the shell is the operator's own console.
        - A call that names scopes is refused with `invalid_params`. The caller can neither narrow nor widen the set.
      - **What the token is.** The app-session token is a **bearer** secret, like every client token, and with `operate` scope it **is** the operator's identity for ADR-0014's operator-only verbs, exactly as ADR-0036 §5 says of an `operate`-scoped file token.
        - **Usable only by its minter, only on the unix socket.** The TCP listener refuses an app-session bearer at `hello` with 4401, and the unix-socket `/v1/ws` admits it only from its minting process, matched by PID **and birth** ("One protocol, two sockets", below). A process that obtains the token therefore cannot open a session with it. The bearer still stays secret, because the binding is the server's check and a defect there would expose it.
        - **Residual risk (#1177).** Processes started from a flk pane, which includes agents, are refused the mint. Reading the token from the shell's memory no longer gives them a session (above). What remains is control of the shell process itself, for example by attaching to it or injecting code, which would let them act as the shell. This design does not close that; #1177 tracks same-UID agent isolation. It narrows it:
          - the token is never written to disk or logged;
          - its lifetime is bounded (below);
          - the shell ships with the macOS hardened runtime, without `get-task-allow`, so a same-UID process cannot attach to it.
          - On Linux, the protection depends on Yama `ptrace_scope` ≥ 1, and the native shell refuses to mint when it reads 0. It says “Local connection unavailable: Linux ptrace_scope is 0, so other processes could read the operator credential from this app. Use a remote connection or ask your administrator to enable ptrace_scope ≥ 1.” It offers explicit remote pairing, never a silent fallback. G9a owns this native-side check.
      - **Binding to the app's process.**
        - The PID comes from the **minting connection's peer credentials** (`LOCAL_PEERPID`; on Linux `SO_PEERPIDFD` where the kernel has it, else `SO_PEERCRED`), never from a parameter.
        - The start time comes from `src/platform/peer.rs`: `/proc/<pid>/stat` field 22 on Linux, and `flock_process_facts` on macOS.
        - **Order.** First arm the exit watch (kqueue `EVFILT_PROC`/`NOTE_EXIT` on macOS; on Linux the peer pidfd itself, or else `pidfd_open`), then read and compare the start time. A process that exits, or a PID that is reused, between the two steps is caught by the armed watch or by the mismatch. The server never polls with `kill(pid, 0)`.
        - If the watch cannot be armed, or the start time does not match the one taken when the connection was accepted, the mint is refused with `peer_process_changed`.
      - **Lifetime.**
        - App-session tokens live in the server's memory only, never on disk, so a server restart **or handoff** revokes them. A handoff does not carry them or their watches to the new process. The shell sees close 4403 and mints again, so the person sees one reconnect.
        - Each one has a fixed **TTL of 12 hours** from the mint, with no sliding renewal.
        - `client.app.mint` returns `issued.info.expires_in_ms`, the server-computed remaining lifetime in unsigned milliseconds at issuance. The native side passes this relative value to the client as `expiresInMs`, without hard-coding the TTL or relying on matching wall clocks. The client owns the single renewal timer on the monotonic clock, as described in the shell config contract above. The TTL counts time the machine spent asleep: owner decision 18 (#2489) chose a **suspend-inclusive** clock, so a 12 h token expires after 12 elapsed hours including sleep. ADR-0036 owns the server's suspend-inclusive TTL clock and its enforcement (#2489); this section does not restate them. After sleep, a late client renewal is caught by the server's 4403 expiry path, which triggers reactive renewal. Persistent file-token metadata omits it; older responses may omit it.
        - Before expiry, the client opens a new bridged session (the contract's renewal, at ¾ of the `expiresInMs` the native side passes on from `expires_in_ms`) and resumes through §3's resume path. Each mint is its own bearer, with its own name, revocation, TTL and exit watch, but under the operator UID's one app-session principal (ADR-0036), so the cursors and the outbox survive and nothing is lost. A mint result without `expires_in_ms` leaves `expiresInMs` out, and renewal is then reactive (a 4403 at expiry).
        - **When the native side mints.** It keeps one current bearer. It mints a new one when it has none, when the open passes `renew: true`, and when the server refused or ended a session on the current bearer with 4401 or 4403, whether or not that session got a `welcome`: a refused bearer is never offered again. It sees those codes because it forwards the close. Any other close, such as 1006 or 1001, keeps the bearer, so an ordinary reconnect mints nothing. The client's streak still bounds a mint that the server refuses every time. A replaced bearer is not revoked early; it ends at its TTL or when the app exits, and no session uses it.
      - **Revocation.** A token is revoked when its process exits, when its TTL ends or when the server restarts or hands off. The server then closes every `/v1/ws` session authenticated by it through ADR-0036's existing close 4403. A command that detects revocation first receives an `auth_revoked` error acknowledgement; idle sessions close without needing a command. No new auth error names are introduced (#2442). The bridge forwards the close verbatim; the client counts it in its streak and opens a new bridged session, for which the native side mints, so the person sees one reconnect. After a restart, a reconnect with the old bearer is refused at `hello` with 4401, which counts the same way (the contract's bullet).
      - **Never in the webview.** The bearer stays in the native process. The webview gets the contract's `bridge` transport (above), backed by one Tauri v2 IPC channel per session, and each channel by one connection to the server's unix-socket `/v1/ws`. No Tauri command returns the local bearer, and on the same machine the capability manifest grants only the bridge's open, send and close. A remote server's keychain token reaches the webview through `tokenProvider` (#2293), which is that path's own design. The native side never logs or persists the bearer or the `hello` frame it rewrote. A reload opens a new session; nothing is persisted.
      - **One protocol, two sockets.** `flock.sock` carries the mint, and the server's unix-socket `/v1/ws` carries the session. That is a **sibling socket, `flock-ws.sock`**, in the same directory as `flock.sock`, with mode 0600 (ADR-0036's #2500 amendment, #2530). It uses the same WebSocket protocol (HTTP/1.1 upgrade, the same JSON text frames and close codes) on a unix-domain listener instead of TCP. ADR-0011 §3's direct-socket RPC transport stays superseded (owner decision 1): there is still one protocol, one auth model and one set of client tests.
        - **Admission on the unix socket.** The LocalOperator check runs before the upgrade, on the accepted connection's kernel peer credentials: a peer that fails it never reaches `hello` and gets the HTTP 403 `operator_only` refusal ("Refusals before the upgrade", below). After the upgrade, a `hello` is admitted only with a bearer, which stays the one admission path. An app-session bearer is bound to its minting process by **PID and birth**: every unix-socket `hello` must come from the minting peer PID with the same birth, and any other peer process is refused with close 4401 and reason `peer_process_changed`, so a token read from the shell's memory is useless over the unix socket. A reused PID is refused by that check, not by waiting for the exit watch's callback: under ADR-0036's admission/revocation fence (#2500 amendment), the server verifies the token's live exit watch and the peer instance before it registers the session or sends `welcome`, and an exit that is already ready invalidates admission even if the owner has not yet serviced its notification. These sessions never count against the TCP listener's pre-auth budget.
        - **The TCP listener refuses app-session bearers** at `hello` with close 4401 and reason `peer_process_changed`, exactly that string, because TCP cannot prove the minting process. They exist only for the unix socket.
        - **Refusals before the upgrade.** `flock-ws.sock` can refuse the HTTP upgrade itself with a typed JSON body (ADR-0036's #2500 amendment). The native side maps each one and never forwards the HTTP body as a frame or turns it into a 4401 or 4403:
          - **403 `operator_only`:** `openBridge` rejects with a message that says the app is not running as the local operator (for example, it was launched from a flk pane) and offers the remote path. Not retried.
          - **403 `peer_process_changed`:** the native side retries once on a fresh connection, as for the mint. If it is refused again, `openBridge` rejects with the server's message. Not retried further.
          - **503 `preauth_capacity`:** `openBridge` resolves, and the `socket` closes at once with 1013 (try again later) and reason `preauth_capacity`. The bridge makes this close itself, like 1006 and 1008; the server never sends 1013. That is §3's Socket lost, so the client reconnects under capped backoff, the retry class `later`. It is not an auth end and never counts toward the re-mint streak.
          - **Any other non-101 response, or a 403/503 body with a code that is not one of these:** `openBridge` rejects with the status and code, as a defect to report. Not retried.
        - **The local shell opens no TCP connection.** The shell-origin allow-list below serves only shells that talk to a remote server.
      - **No server running.** The shell starts one through the existing daemon path.
      - **No silent fallback.** An app launched from inside a flk pane fails the LocalOperator check, and a sandboxed (App Store) build cannot reach `flock.sock`. Either one says why and offers the remote path; it never falls back silently.
    - **A remote server** (#2293), or a platform without `flock.sock`. The person pastes or deep-links a one-time code, and the token lives in the keychain or keystore, optionally behind biometric unlock.
  - **Shell origins, on `/v1/ws` only.**
    - A shell's webview does not present the server's origin. The listener's guard (`shell_origin_allowed`, `src/web/gui.rs`, used by the listener in `src/web/client_listener.rs`) admits an **exact allow-list of shell origins** on the `/v1/ws` upgrade only.
    - For those origins only, it relaxes the `Sec-Fetch-Site` check to the value the shell actually sends (`cross-site` against `127.0.0.1:port`). Every other origin keeps today's `none`/`same-origin` rule.
    - The loopback-peer and exact-`Host` checks are unchanged.
    - **The values are captured, not assumed.** The allow-list holds only values a real Tauri v2 shell sent. The documentation is inconsistent, giving `tauri://localhost` and `null` for macOS and `http://tauri.localhost` for Windows, so nothing is admitted on that basis. G9a captured them with the `desktop/` shell's release protocol (`tauri build`), from a `fetch` and a `WebSocket` upgrade to a loopback echo server; both requests sent the same values:

      | Platform | Origin | Sec-Fetch-Site | Tauri | Date |
      |---|---|---|---|---|
      | macOS 26.6 (arm64, WKWebView) | `tauri://localhost` | `cross-site` | 2.12.3 | 2026-10-25 |
      | Linux (x86_64, WebKitGTK 2.54.1) | `tauri://localhost` | `cross-site` | 2.12.3 | 2026-10-25 |
      | Windows (WebView2) | not captured: remote-only, no `flock.sock` | | | |

      Neither local platform sends `null`. Admit the captured pair only on the exact `/v1/ws` path; it grants no general cross-site fetch permission.
    - **`null` is never admitted.** `null` is also what an artifact frame (§7), a `file://` page, a `data:` URL and other sandboxed pages send, so admitting it would leave only the token and the frames' CSP as the barrier.
    - **No registered port.** The capture supersedes the earlier registered-loopback design. No `origin_port` parameter, mint-time port-ownership check or `origin_port_not_owned` code is needed on the captured platforms.
    - **The threat model.** Admitting a shell origin opens no ambient authority: `/v1/ws` authenticates by the token in `hello` alone (ADR-0036 §5), and the server has no cookie or other ambient credential. ADR-0036 §5 already notes that local processes can forge `Host` and `Origin`, so the origin check is a defence against browsers, not against local code. Here is how each origin that could reach the listener stays out:
      - A web page in any local browser has a real web origin, which is refused. Even if it were admitted, it has no token.
      - Artifact frames (§7) are `null`, so they are refused. Their injected CSP `connect-src 'none'` is **required** as a second barrier, and G7's per-shell security test checks it.
      - `file://` and `data:` pages are `null` or their own scheme, so they are refused.
    - **Admission policy (#2500, owner decision 18).** The owner chose the unix-socket bridge above over per-UID attribution on TCP, which review had found could fail into the unknown bucket and amplify the accept loop. The TCP listener keeps one shared pre-auth budget for browsers and remote shells, and the local shell's reconnects never compete for it. The Origin-only split (#2505) kept the runtime's existing limits: four pending hellos, 16 upgraded sockets and 32 incomplete HTTP connections. The 16-concurrent and 10-per-second figures below were the original target and were not built in that split. ADR-0036's #2500 amendment decides the final TCP numbers. The registry reserves `preauth_capacity` (HTTP 503) and `auth_rate_limited` (HTTP 429), each with the single retry class `later`.
    - **The original pre-auth target on TCP (not built; ADR-0036's #2500 amendment decides).**
      - At most **16** concurrent unauthenticated `/v1/ws` connections per listener. Beyond that, an upgrade is refused with `preauth_capacity`.
      - Each connection gets exactly one `hello`; a failed `hello` closes it.
      - After **10** failed `hello`s within any **1 s**, a new upgrade is refused with `auth_rate_limited` for the rest of that second.
      - This global policy gives no per-UID reconnect isolation, and none is needed: the local shell reconnects over the unix socket (owner decision 18). ADR-0036's #2500 amendment fixes the final TCP numbers.
      - The ADR-0036 §5a handshake deadline still applies. These limits bound resource use; guessing a token (32 random bytes) is infeasible.
    - **`/v1/client/exchange` keeps its strict checks** (`src/web/client_exchange.rs`: loopback peer, exact `Host` and `Origin`, `Sec-Fetch-Site` `same-origin`). Its one-time code is a bearer secret, and the shell never uses it locally.
  - **Tests that must fail before the change** (server slice and G9a):
    - a captured shell origin is admitted on `/v1/ws` and refused on `/v1/client/exchange`;
    - `Origin: null` is refused on both, with and without a token;
    - a shell origin with no token, or with a wrong one, is refused at `hello`;
    - a mint that names scopes is refused, and a mint from a non-LocalOperator peer is refused;
    - the token dies when its process exits (a real child process, exit detected without polling), and a live session is closed with 4403;
    - the token dies at its TTL (an injected clock), and a live session is closed with 4403; the TTL counts suspended time, as owner decision 18 (#2489) decided; the server-side clock tests are ADR-0036's;
    - a start-time mismatch is refused with `peer_process_changed`, and a restart or handoff revokes every app-session token;
    - `same-site`, `null`, duplicate headers, lookalike shell origins and non-WS paths keep their existing refusal treatment;
    - the shared TCP pre-auth cap test follows ADR-0036's #2500 amendment;
    - on the unix-socket `/v1/ws`: a non-LocalOperator peer is refused before the upgrade with HTTP 403 `operator_only` and never reaches `hello`; an app-session bearer presented by a process other than its minter is refused, including a process that reuses the minter's PID with another birth after it exits, and a `hello` whose minter's exit is already ready but not yet serviced; those sessions leave the TCP pre-auth budget untouched;
    - on TCP: an app-session bearer is refused at `hello` with close 4401 and reason `peer_process_changed`, from any origin;
    - the bridge's pre-upgrade mapping: a 403 `operator_only` rejects `openBridge` with its message and no retry; a 403 `peer_process_changed` retries once on a fresh connection and then rejects; a 503 `preauth_capacity` resolves and closes with 1013, so the client reconnects under backoff without counting toward the streak; another status, or a 403/503 whose body carries an unknown code, rejects; no HTTP body ever reaches the webview as a frame;
    - the bridge: the bearer never appears in a frame or call result delivered to the webview, and the capability manifest allows only the bridge's commands; frames other than the first `hello` pass through byte for byte; a 4403, a 4401 after a `welcome`, a 4401 before one and a `renew: true` open each lead to one new mint, and the bearer that was refused is never offered again; a 1006 or 1001 close reuses the bearer without a mint; renewal resumes with the outbox kept; a first frame that is not a token-free `hello` is closed with 1008 and not forwarded; an unreachable unix socket resolves `openBridge` and closes the `socket` with 1006, while a mint refusal rejects with its message;
    - the client in `bridge` mode: `hello` carries no token; only the renewal open passes `renew: true`; a 1008 close shows its diagnosis and goes to Reconnecting, per §3's "Socket lost"; and the provider path's streak, cap, renewal and *Disconnect* tests pass unchanged against a fake `openBridge`;
    - the Linux shell refuses to mint when `ptrace_scope` is 0.
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
- **One transport.** ADR-0011 §3's desktop path, Tauri IPC straight to `flock.sock`, is **superseded** by the WebSocket and client tokens of ADR-0036 §5 for every shell (owner decision 1). There is one code path, one auth model and one set of tests. A desktop app on the server's host mints its own app-session token over `flock.sock`, which is the same LocalOperator authority that `flk client open` uses (owner decision 16), and carries the same `/v1/ws` protocol over the server's unix socket rather than TCP (owner decision 18).
- **Identity, and exactly what changes in ADR-0011.**
  - **Superseded for the GUI:** ADR-0011 §4's requirement that a WSS *gateway* (#150) authenticate GUI clients by a non-empty `allowed_users` list and the `Tailscale-User-Login` header, and propagate that login as the request principal. For the `/v1/ws` endpoint, the principal is the client token's (ADR-0036 §5), on any network.
  - **Kept:**
    - the loopback default and the funnel refusal;
    - the terminal bridge's own `allowed_users` check (`src/web/mod.rs`, ADR-0001), which serves `pty` terminal attach and is unchanged;
    - ADR-0011 §7's rules for proxying terminal attach.
  - A deployment may still put `/v1/ws` behind `tailscale serve` as defence in depth.
  - **No migration is needed.** ADR-0011's gateway, the IPC transport and the use-case layer were never built: there is no `src/usecase/`, no `src/gui/` and no `RpcTransport` in the tree on 2026-10-15. Nothing in use changes.

#### Amendment: concurrent token admissions (#2450, 2026-10-25)

The file-token authorization recheck and bounded dispatch hold a **shared** store
lock. Concurrent admissions can proceed together. Creation, replacement and
revocation retain the **exclusive** lock. A revocation intent is published before
waiting for exclusive ownership; admissions check intent both before and after
acquiring their shared lock, so readers cannot starve revocation. Revoke waits
for admitted callbacks to finish; after it returns, no dispatch under the revoked
token remains in flight or can begin. Callbacks cannot wait for model execution
or recursively manage tokens.

A subscription encountering pending mutation receives `subfail session_busy`,
using the registered `session_busy` retry class `later`, on that ID only. Sibling
subscriptions and the authenticated socket remain connected. A client may open a
fresh ID later. Unsafe or unreadable storage still closes the transport.

Review amendment: intent probes also share their lock; live revokers retain
exclusive intent ownership. Startup and exclusive revocation reclaim unlocked
stale intents, checking exclusive ownership, link count and inode identity so
concurrent sweepers cannot remove a newly published intent at a reused name.
The client automatically retries only the busy stream under fresh IDs after
250, 500 and 1000 ms, at most three same-socket retries since its last `synced`.
Unsubscribe or socket loss cancels the pending timer. Exhaustion leaves the
stream visibly Refused. Reopening starts a new budget; a later reconnect may
also retry through its new welcome, without resetting this timer budget.
Siblings keep their IDs and cursors.

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
  - Retention: §7a's lifetimes (a `session` default, then `idle_ttl`, 14 days by default after the last view, comment or answer), with holds, quotas and an operator purge command (ADR-0011 §5).
- A version is immutable, and a published artifact is its hash.

**Rendering and isolation.**
- The client renders an artifact only inside `<iframe sandbox>` **without** `allow-same-origin`, `allow-top-navigation*`, `allow-popups`, `allow-forms` or `allow-modals`. The frame's origin is opaque, so it has no cookies, no storage, no access to the parent and no access to the client token.
- The content goes in through `srcdoc` (HTML) or a `blob:` URL (other kinds), with a CSP `<meta>` injected first:
  - Static artifacts (the default) get ADR-0011 §5's policy: `default-src 'none'; img-src data: blob:; style-src 'unsafe-inline'; script-src 'none'; connect-src 'none'; form-action 'none'; base-uri 'none'`.
  - With no script, no forms and no top navigation, a static artifact has no way to make a request beyond the inline data it carries.
  - **Interactive** artifacts add `allow-scripts` to the sandbox and `script-src 'unsafe-inline'`. `connect-src 'none'` stays.
- **Residual risk of interactive artifacts: navigation of the frame itself.** A script can navigate its own frame (`location = …`, a meta refresh, a link) and so leak data in a URL as a beacon. CSP has no widely shipped `navigate-to`, so the sandbox flags cannot fully stop this.
  - The frame's own CSP plus `default-src 'none'` means the navigated-to page cannot load anything in the same frame. That narrows the channel but does **not** bound it: a navigation to a URL that answers 204/205 fires no `load` and can repeat, and WebRTC reaches the network with no navigation at all (§7e).
  - So an interactive artifact can send out the data it carries inline, with or without a stream grant. This is §7e's accepted residual, not a bounded leak.
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

### 7a. Artifact lifecycle: nothing piles up (#1598, owner decision 17)

**Lifetimes.** `flock_artifact_publish` gains `lifetime?: "session" | "persistent"`, **default `session`**.
- **The session** is the **publishing agent's** identity, not the server's. A `session` artifact's session ends when that agent is stopped or its identity is replaced (ADR-0028 §8a). A restart, a handoff or a compaction does not end it.
- **Persistent.** A `persistent` artifact, and a `session` artifact whose session has ended but which is still held (below), expires after it has been **idle** for `[artifacts] idle_ttl` (default 14 days).
- **Idle** means no view, comment or answer, and no open tile, for that long. A non-operator principal's view or open tile counts only within its lease budget (Holds, below).

**Holds: what keeps an artifact.** An artifact is **held**, and is never expired or evicted, while any of these is true:
1. A person has pinned it.
2. A tile shows it. The client sends `artifact.viewing { artifact_id, version, open: bool }` from a client session holding at least `read` scope.
   - The server keeps a **viewing lease** per client session. The client renews it by heartbeat every 30 s, and the lease lapses after 90 s without one, after **8 h** in any case, and on disconnect.
   - A principal holds at most **8** viewing leases at once.
   - A viewing lease defers expiry and eviction under both quotas. It **never** migrates a blob (§7c), and a held version's bytes still count against its publisher's quota and the total: a hold makes a publish refuse, never frees room.
   - **Lease budget, for non-operator principals only.** A lease from the **operator principal** (ADR-0036 §5: an `operate`-scoped token *is* the operator's identity, which covers the operator's client token and an app-session token, §6) holds for as long as it is renewed, so a version a person has open is never expired or evicted, however long or often they view it.
   - A lease from **any other principal**, such as a read-scope token given to an agent, holds one version for at most **8 h in total** over that version's lifetime. Past that budget, the server still accepts the lease and the tile still shows the version, but the lease neither holds the version nor counts as a view for the idle clock. So an agent cannot keep its artifacts alive by re-leasing them.
   - A process that obtains an operator token is outside this bound; that is ADR-0036's named residual (#1177).
3. An open question has the artifact as its `about`.

When a hold ends, the artifact follows the persistent idle rule from that moment. It is never deleted at once.
- **A withdrawn question.** If the asker's identity is replaced, its open questions are withdrawn with `subject_replaced` (§5b). The hold then ends, and the artifact turns idle-persistent. A replaced identity therefore never deletes what a person was asked about.

**Quotas.**
- `[artifacts] quota_bytes_per_agent` (default **32 MiB**) counts one agent's stored versions. A publish over it evicts that agent's **oldest idle, unheld** versions first. It never evicts another agent's.
- `[artifacts] quota_bytes_total` (default **2 GiB**, which fits 64 agents at their full quota) never evicts across agents. A publish that would exceed it first evicts **the publisher's own** idle, unheld versions. A viewed, pinned or otherwise held version is never part of that eviction. If the total is still full, it is refused with `artifact_total_quota_exceeded`.
- **Pressure is visible.** At 80 % and at 100 % of the total, the operator gets an inbox notice naming the largest holders, with a one-click purge of idle, unpinned artifacts. So a full store is never a silent block for 14 days.
- Most artifacts are `session` and die with their agent, so the idle TTL only governs persistent and held ones.
- If eviction cannot make room, the publish is refused with `artifact_quota_exceeded` (the agent's own quota) or `artifact_total_quota_exceeded` (the server total). These are two codes because the registry gives each code one retry class (§7e).

**Pinning.**
- A person pins or unpins through a client token with `answer` or `operate` scope, or through the local CLI. This is recorded as `artifact.pinned` / `artifact.unpinned`.
- Agents cannot pin.
- Only an explicit operator purge removes a pinned artifact.

**Purge is a record, not a hole.**
- Every removal writes `artifact.purged { artifact_id, version?, reason: idle | session_end | quota | operator | blob_missing }`.
- The records, comments and questions stay. The tile shows "content expired". An open question about a purged artifact stays open and answerable, and its card says the content expired.
- **What an operator purge removes.**
  - It also removes every anchor excerpt for the purged versions: the `quote.exact`, `prefix` and `suffix` of §7d are replaced by `[purged]`, so no text of the content survives.
  - Automatic purges (idle, session_end, quota, blob_missing) keep the excerpts, as part of the comments.
- **Purge versus deduplication.** A purge drops **this version's reference** to its blob. The bytes are removed only when no record references them (§7c). Identical content that a stream line also holds stays, as part of that line. A purge therefore removes the artifact, not every copy of its bytes; erasing those is S16's retention work.
- **A §5b fork** that is asked about a purged artifact reads the `artifact.purged` record, not the content.
  - A fork's kept transcript (owner decision 13) is not rewritten by a purge, so text that it already read stays in the lineage.
  - Removing that text is S16's retention work.

**Configuration (new keys this ADR introduces).** They join ADR-0041's inventory (#2384) as plain, non-secret values:

| Key | Type | Default | Classification (ADR-0041 §2) |
|---|---|---|---|
| `[artifacts] max_bytes` | bytes | 2 MiB per version | non-sensitive operational limit; operator write; reload: server |
| `[artifacts] max_versions` | int | 50 per artifact | non-sensitive operational limit; operator write; reload: server |
| `[artifacts] idle_ttl` | duration | 14 d | non-sensitive operational limit; operator write; reload: server |
| `[artifacts] quota_bytes_per_agent` | bytes | 32 MiB | non-sensitive operational limit; operator write; reload: server |
| `[artifacts] quota_bytes_total` | bytes | 2 GiB | non-sensitive operational limit; operator write; reload: server |
| `[artifacts] allow_stream_grants` | bool | false | security policy (enables an accepted exfiltration residual, §7e); operator write only; reload: server |
| `[blobs] session_backend` | enum | `session_tmp` | non-sensitive selector; operator write; reload: restart. A future backend's credentials are secret references, never values |

Any future backend credential (for example S3) must use ADR-0041's secret-reference shape, never an inline value.

### 7b. Question sets inside an artifact (#1599, owner decision 17)

`flock_artifact_request_feedback` generalises `choices?` into a **typed question set**. `choices?` stays, as shorthand for one single-choice question.

```
questions: [{
  id,                                   // stable within the set
  prompt,
  kind: "single" | "multi" | "free",
  options?: [{ id, label }],            // single and multi only
  other?: bool,                         // single and multi: adds a free-form "other"
  anchor?: Anchor,                      // §7d
}]
blocking?: bool                         // for the set as a whole
```

**One question, one ADR-0036 §2a record.**

Each question maps onto §2a's existing `question.asked` fields:

| Set field | §2a field |
|---|---|
| `kind: single` | `options` with `multi_select: false` |
| `kind: multi` | `options` with `multi_select: true` |
| `kind: free` | no `options` |
| `other` | `allow_other` |
| `prompt` | `prompt` |

- **`about`** is `{ artifact_id, version, set_id, question_id, anchor? }`. That is the one new shape for the backend to type.
- **`ask_point`** is the same for every question in a set: the boundary that closes the `flock_artifact_request_feedback` call (§2a's rule for a tool call). A thread on any question forks there.
- Routing (ADR-0034), answer authority (§5b), `question_closed` and "exactly one reply" apply **per question**.

**Answers keep the main context lean.**
- Each answered question queues **its own** `reply` item at once.
- A **blocking** set releases the asker when its last question is answered.
- An "other" answer is free text within the question's answer limit.

**Comments and threads on each question.**
- **A comment thread** can sit on any question or option: an `artifact.comment` whose anchor names `question_id` and, optionally, `option_id`.
- **Threads.** Any question can be taken as a **thread** through §5b unchanged. The fork starts at the set's `ask_point`, and only its final proposal comes back, applied after review (owner decision 12). The main agent sees one answer, not the discussion.

**Rendering.** The question set is drawn by the **client**, in trusted chrome beside the frame. An artifact cannot draw its own answer buttons, which keeps the phishing defence (failure modes).

### 7c. Blob lifetime and the storage backend

Blobs stay in ADR-0036 §4's content-addressed store (`<data dir>/blobs/<sha256>`, shared and deduplicated, also used by stream lines). §7a's lifetimes govern **records**. A blob's lifetime follows **the records that reference it**:

- **Reference counting.** A blob lives while any record references it: any artifact version, stream line or other record. The store counts references. Purging a record drops its reference, and the blob is removed when the count reaches zero.
- **The session temp backend.** The owner asked for "tmp folder sessions" now. A blob may live in the publishing agent's session temp directory (`[blobs] session_backend = "session_tmp"`) **only while every record that references it is a `session`, unheld artifact version of that agent**. It is removed when that agent's session ends.
- **Migration.** A pin, a change to `persistent`, a question hold or a reference from anything else migrates the blob to the durable store before that change is acknowledged.
  - **Order:** copy the bytes, fsync, verify the sha256, add the durable reference, then drop the temp reference and copy.
  - The session-end sweep **skips** any blob with a migration in progress.
  - If migration fails, the triggering change is refused with `blob_migration_failed`, and the blob stays where it was, whole.
  - A **viewing** lease never triggers migration. While a viewed blob's session has ended, the sweep defers deleting it until the lease ends.
- **What the GUI relies on.** Records carry only the hash and size, and their shape never depends on the backend. A blob that is gone reads as `artifact.purged { reason: blob_missing }` (§7a).

The backend trait and its config are ADR-0036 §4 / S3b #1426, owned by the server.

### 7d. Text-range anchors (#1599, owner decision 17)

Comments and questions anchor to a block, or to text inside it:

```
Anchor = {
  block_id,                                             // data-block-id, ADR-0036 §12 item 2
  quote?: { exact, prefix?, suffix? },                  // W3C Web Annotation TextQuoteSelector
  position?: { start, end },                            // character offsets in the block's text, a hint only
}
```

- **Selecting.** The client computes the anchor from the selection. It takes up to 32 characters of prefix and suffix, enough to make the quote unique in the block.
- **Re-anchoring across versions.** When a newer version is shown, each older anchor is mapped:
  1. **Re-anchored.** The same `block_id` exists and `exact` occurs in it. Pick the occurrence that best matches `prefix`/`suffix`, then the nearest to `position`.
  2. **Detached (text).** The block exists but the quote is gone. The comment attaches to the block, marked "text changed".
  3. **Detached.** The block is gone. The comment is listed beside the frame with its quoted text.
- **The server keeps the original anchor and its version, and never rewrites them**, except for an operator purge's `[purged]` (§7a). Re-anchoring is a view, recomputed per version.

### 7e. Network for artifacts: default deny now, the grant shape fixed now (#1599, owner decision 17)

`connect-src 'none'` stays the default for every artifact, interactive or not. Live data later must not need a new security model, so its shape is fixed now.

**Declared capabilities.**
- Only an interactive artifact may declare `capabilities?` at publish. The declaration is part of the version and is shown under its badge.
- v1 defines one kind: `{ kind: "flock_stream", stream, access: "read" }`, a read-only subscription to a named flock stream that the viewer could open anyway.
- External origins are a later kind, behind a server proxy with a per-artifact scoped token, and are not defined yet.

**Exfiltration through a stream grant is an accepted residual, disclosed and off by default.**
- **What can leak.** An interactive frame that holds a stream grant **can send out what it reads**, and no sandbox flag or CSP can stop it:
  - it can navigate itself to a URL that answers 204/205, which fires no `load` and can repeat;
  - with `allow-scripts`, WebRTC STUN/ICE reaches the network without any navigation, because `connect-src` doesn't cover it;
  - browsers ship no `navigate-to`.
- **Why live data still runs as interactive code.** A non-interactive render cannot consume a stream: drawing live data needs script. A **declarative binding**, where the parent fills stream values into a static, script-free frame, would avoid this residual. It is the preferred later kind, and v1 does not define it.
- **Off by default.** Stream grants need `[artifacts] allow_stream_grants = true` (default **false**) set by the operator. Each grant is explicit, per version.
- **The grant prompt says it in plain words:** *"This artifact runs code. It can send anything it reads from \<stream\> to the internet. Grant only if you trust the agent that wrote it."*
- **A best-effort tripwire, not a bound.**
  - The parent counts the frame's `load` events, resetting the counter whenever the parent itself (re)loads the frame, for example on a version change.
  - An unexpected extra load tears the frame down and revokes that viewer's grants on the version (`artifact.grant_revoked { reason: navigated }`).
  - It catches careless code, not a determined frame.

**Grants belong to a person, per artifact, per version.**
- A person grants a declared capability through a client token with `operate` scope. This is recorded as `artifact.granted { artifact_id, version, capability, by, at }`, and revoked as `artifact.grant_revoked { reason }`.
- A new version needs a new grant.
- **No confused deputy.** The broker serves a call only under a grant whose `by` is the principal of **the client session that renders this frame now**. A frame viewed by person B never uses person A's grant; B is asked.

**The only channel is parent-brokered.**
- The frame posts `{ type: "flock.rpc", id, capability, request }`.
- The parent identifies the frame by `event.source` (§7), checks the declaration and the current viewer's grant, and does the work itself over its own `/v1/ws` session. It answers with `{ type: "flock.rpc.result", id, ok | error }`.
- **The frame never receives the `/v1` token or any credential.** Its CSP keeps `connect-src 'none'` even with a grant, so all data arrives by postMessage.
- **Limits per frame.**
  - 20 RPCs per second;
  - 4 concurrent subscriptions;
  - 16 KiB per request and 64 KiB per result message.

  Over a limit, the call is refused and the parent's own session is never flooded.

**Refusals.** Each code is registered in the error-code registry (`src/api/error_code.rs`) with one of its retry classes:

| Code | Retry | Meaning |
|---|---|---|
| `capability_not_declared` | `never` | not in the version's declaration |
| `capability_not_granted` | `after_human` | no grant from the current viewer; the parent may prompt |
| `capability_unsupported` | `never` | the kind is not implemented, or stream grants are disabled (v1: all) |
| `rpc_too_large` | `never` | over the size limit |
| `rpc_rate_limited` | `later` | over the rate or subscription cap |
| `artifact_quota_exceeded` | `after_human` | the agent's own quota holds only pinned or held versions; a person must unpin or close |
| `artifact_total_quota_exceeded` | `later` | the server total is full; idle versions expiring, or the operator purging after the 100 % notice (§7a), frees space. Pinned and held bytes never expire |
| `blob_migration_failed` | `maybe` | §7c; the triggering change was refused, nothing is half-moved |

A refused call is never retried automatically by the parent.

**G6/G7 ship default deny only.** Every `flock.rpc` is answered `capability_unsupported`. The grant record and the broker land with the live-data slice.

### 7f. Tests that must fail before each change

- **Lifecycle.**
  - A `session` artifact is purged with `session_end` when its agent stops, and not on a restart or compaction.
  - A held one (open tile, open question, pin) is not purged.
  - After a hold ends, it expires only after `idle_ttl` (an injected clock).
  - A question withdrawn by `subject_replaced` leaves the artifact idle-persistent, not deleted.
- **Quota.**
  - The per-agent quota evicts that agent's oldest idle unheld version, and never a pinned, held, on-screen or other agent's version.
  - The total quota never evicts a viewed or other held version, and refuses with `artifact_total_quota_exceeded`.
- **Pin.** Only a person can pin; an agent's pin is refused. A pinned artifact survives every automatic purge.
- **Blobs.**
  - A shared blob survives the purge of one of its two referencing records.
  - Pinning a session artifact migrates its blob, and a failed migration refuses the pin.
- **Question sets.**
  - Each answered question produces exactly one `reply`.
  - A blocking set releases on its last answer.
  - Each kind maps to the §2a fields.
  - A thread forks at the set's `ask_point`.
- **Anchors.** Each of the three re-anchor outcomes, across a real version pair.
- **Purge.**
  - An operator purge replaces the anchor excerpts with `[purged]`.
  - An automatic purge keeps them.
  - A fork reads the purged record.
- **Network.**
  - A static artifact makes no request at all. An interactive one makes no `fetch`, XHR, WebSocket or EventSource request (`connect-src 'none'`), and no frame reaches the listener with a credential. Its navigation and WebRTC channels are §7e's accepted residual and are not asserted closed.
  - Every `flock.rpc` gets `capability_unsupported` in G6/G7.
  - With `allow_stream_grants = false` (the default), a grant is refused.
  - The broker tests come later: a viewer-B frame with only A's grant is refused, the limits refuse, an unexpected extra load revokes the grants, and a parent reload resets the counter.
- **Quota pressure.** The 80 % and 100 % notices are emitted. The total quota evicts only the publisher's own versions, and then refuses with `later`.
- **Migration.** A sweep during an in-progress migration skips that blob. A failed hash verification refuses the pin and leaves the blob whole.
- **Viewing lease.** It lapses without a heartbeat and at 8 h, at most 8 are held per principal, and a held version's bytes still count against its publisher's quota. An operator lease renewed past 8 h of total viewing still holds the version, and the version is not evicted. A non-operator principal re-leasing past its 8 h budget on a version neither holds it nor resets its idle clock.

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
| S6e | ADR-0036's question amendment, by its owner (§5b, "What ADR-0036 must change"): `flock_question_ask` as the one path for questions to the operator, `blocking_reason` required iff `blocking: true`, `blocking` false by default (owner decision 15), the lease as the attested origin, `question.resolve` as a proposal, the `threaded` and `proposed` states, `subject_replaced` closing a question as `withdrawn`, the held-to-deferred rule, `fork_prefix_gone`, the logged proposal and edit records; with it, the ADR-0028 §8a amendment: no MCP `flock_question_answer` for operator-addressed questions | S6b, S6c, S3b #1426, #1734 | no | ADR + 250 |
| G14 | Threads on the wire: `InboxItem`'s `blocking_reason`, `threads` and `fork_points` (with `dry_run` cost), `inbox.thread`, `inbox.answer { proposal_id }`, and answers to operator questions only from the operator's surfaces (#1673) | S6e, G5, S12, S8b | no | 300 |
| G15 | The thread UX: *Take as thread* and *Thread from now*, the fork's tile, *Review and merge back* with editing, the card states and the stale-answer display (§5b) | G14, S9a, G2 | no | 400 TS |
| G16 | Thread lifecycle against the scripted server: asker restart and handoff (`observation_reset`), identity replacement (`subject_replaced` → `withdrawn`), two threads racing on one question, a `pty` asker | G15, #1577 | no | 200 TS |

Before S3 means a slice depends on nothing in the -p chain's runtime (S3a, S3b). S8a, S7a, G1, G2, G3 and the G9 shell qualify. Everything that serves live data needs S7 and therefore S3.

S6e, G14, G15 and G16 are post-MVP and run in that order. #1719's aside capture reuses G14's proposals and G15's review (§5b); it is not sliced here.

The first end-to-end milestone is **"a structured agent in a browser tile"**: G1 + G2 + G3 + S7 + S8b + S9a, against the -p MVP's runtime. It follows the MVP and does not compete with it.

### 11. Owner decisions (decided by the owner, 2026-10-16, recorded on #1591)

Items 1–9 were decided by the owner on 2026-10-16. Items 1–4 and 6–9 follow the recommendation. Item 5 was decided differently from the recommendation. Item 10 was added and decided the same day. Item 11 was added on 2026-10-17, when the owner chose between two wireframe directions after rejecting the first preview. Items 12–14 were added on 2026-10-18 with §5b (#1717) and decided by the owner on 2026-10-19 (recorded on #1717). Item 15 was added and decided by the owner on 2026-10-19 (recorded on #1717). Item 16 was added after a spike and decided by the owner on 2026-10-25 (recorded on #1601). Item 17 records the owner's typed artifact requirements of 2026-10-25 (on #1598).

| # | Decision | Outcome | Consequence |
|---|---|---|---|
| 1 | Supersede ADR-0011 §3's direct-socket desktop transport with ADR-0036 §5's WebSocket and tokens everywhere | **Decided: yes** | A desktop app on the server host gets its token through the same LocalOperator socket authority as `flk client open`; item 16 decides how (an app-session token, zero-touch). |
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
| 12 | Does the person review, and may edit, a thread's proposal before it is applied? (§5b) | **Decided: yes, and logged** ("this should be logged -- so we can later learn/improve the skills/context"). The proposal opens as editable text, and only *Apply* answers. The proposal, the applied answer and whether it was edited are durable records on the log. | One more step per thread before the answer is delivered; a blocking asker waits for it, a non-blocking one keeps working. The analysis of the logged records is a follow-up (#1717). |
| 13 | What happens to a thread's fork after the merge-back? (§5b) | **Decided: yes** ("stop fork but keep transcript"). The fork is stopped, and its transcript is kept in the lineage (the `AgentForked` edge); the sidebar folds ended threads. | Stored transcripts, and lineage rows to fold. No retention limit is stated yet: ADR-0036 S16 owns it. A rehydrated copy is deleted when the fork ends either way (ADR-0036 owner decision 14). |
| 14 | A thread fork's tools (§5b) | **Decided: read-only now, through the general machinery** ("machinery first … but with read only as per now"). The fork runs under ADR-0036 S19's permission tiers (#1321) with a read-only tier, never a thread-specific flag. | A fork that would have to run a command to answer must say so in its proposal. Widening is #1321's follow-up: tiers selectable per model, and a fork that requests more, which the person grants. |
| 15 | Does S6e retake ADR-0036 owner decision 3a, so that the asker decides blocking, with a reason, instead of S6d's System-One check? (§5b decision 5) | **Decided: yes** (the owner typed "y" on 2026-10-19, recorded on #1717), following the owner's #1717 words "not vlocking [sic] unless there is literally nothing else to work on". | The server stops catching a wrongly held or wrongly continued turn, and an agent that over-uses `blocking: true` only costs itself and puts its card first. S6d no longer holds a turn; any later use of its check is advisory only (the backend ruling on #1717). |
| 16 | How does the Tauri shell get its token from a server on the same machine? (§6; amends item 1's consequence) | **Decided: zero-touch** (the owner typed "go" on 2026-10-25, recorded on #1601), after asking for no token input on the person's own machine. The native side mints an app-session token over `flock.sock`: server-chosen scopes, a 12 h TTL, bound to the minting process (exit detected by kqueue or pidfd), in server memory only. The webview holds it in memory (amended by 18: the bearer stays in the native process, and the session runs over the unix socket). The listener admits an exact allow-list of **captured** shell origins on `/v1/ws` only; `null` is never admitted. Remote servers keep a code and the keychain. | The server gains in-memory app-session tokens and the shell-origin allow-list (ADR-0036's amendment "App-session tokens and shell origins"). The token is still a bearer secret, and the operator's identity, while the app runs. The shell needs a non-sandboxed build: an App Store sandbox cannot reach `flock.sock`. The residual risk is named: a pane-started process that obtains the token gains operator authority (#1177); amended by 18: the token works only on the unix socket from its minting process, so the residual is control of the shell process itself. |
| 17 | Artifact lifecycle, question sets, blob backend, anchors and network (§7a–§7e; added 2026-10-25, recorded on #1598) | **Decided: these requirements**, typed by the owner, scheduled after the `-p` MVP: purge handling so that artifacts do not pile up; questions of several types inside an artifact, each with comments and its own fork-and-merge-back thread; a configurable blob backend, defaulting to temp-folder sessions; comments on blocks and text; network access "if wanted" for live data, a "future feature, but get framework correct for it already now". | §7a–§7e give the design. §7f lists the tests. The defaults (`session` lifetime, the quotas, the 14-day idle TTL, the RPC limits) are proposals the review may change. The server owns the blob reference counting and backend trait, the lifecycle and viewing records, the question `about` shape and the grant record. |
| 18 | How does the local Tauri shell reach `/v1/ws`, and on which clock does an app-session token expire? (§6; amends 16; added 2026-10-26, recorded on #2500 and #2489) | **Decided: the unix-socket bridge, and a suspend-inclusive clock.** The owner chose option (a) on #2500: the shell's native side talks to the server over the local unix socket, with kernel peer credentials, and bridges `/v1/ws` for the webview. The TCP listener keeps one shared pre-auth budget for browsers and remote shells; per-UID TCP attribution is dropped. On #2489, the TTL clock counts wall time across sleep; ADR-0036 owns that clock and its enforcement. | The native side still mints (one admission path, per-mint revocation and TTL) and keeps the bearer out of the webview. The server serves `/v1/ws` on a unix-domain listener with LocalOperator admission and app-session bearers bound to the minting PID and birth, and its TCP listener refuses app-session bearers (ADR-0036, #2500). The client gains the `bridge` transport. The shell-origin allow-list now serves only shells that talk to a remote server. |

## Failure modes and known limits

- **An agent floods the inbox.**
  - The inbox orders blocking items first and caps per-agent visible items.
  - This record does not limit how many questions an agent may ask. That is ADR-0036 §2a's and S6d's job.
- **Artifact phishing.** A sandboxed artifact can still draw a convincing fake approval button inside its frame. The untrusted chrome, a frame that is never full-window, and answers that only happen through the inbox's own controls are the defence. ADR-0011 §5 already called pixel-level phishing otherwise undefendable.
- **Opaque-origin limits.** Interactive artifacts can't use storage, cookies or workers that need an origin. That is intended.
- **Stale reads.** While Stale, tiles show old state with a marker. A user can read but not act. The outbox holds actions until Live.
- **A stale command after the idempotency window** is shown as "unknown outcome", never resent automatically.
- **Cross-node commands** are refused until #1380. The UI shows the refusal in place.
- **The local shell needs the socket.** A sandboxed (App Store) build, or an app launched from inside a flk pane, cannot mint an app-session token. It shows why and offers the remote path (a code); it never falls back silently.
- **Layout loss.** Clearing browser storage resets the layout. Server state is unaffected.
- **A thread's proposal is untrusted text.** The fork read the asker's context, which can hold injected content. The proposal is shown as text, and only the question's recipient applies it, after review (§5b decision 4; owner decision 12).
- **A thread behind a compaction costs a cache write.** The rehydrate path is cold. The card shows the path and cost before forking (§5b decision 2).
- **A thread on a `pty` asker starts after the question.** The card says so, and offers only *Thread from now*.
- **The GUI cannot verify a blocking reason.** It is shown as the agent's own words. A dishonest reason costs that agent its own turn and puts its card first; it never holds another agent.
- **Racing answers.** Of an in-place answer and any number of threads, the first applied wins. The rest fail with `question_closed`, and each card says so (§5b).

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
