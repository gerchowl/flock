# ADR-0038: The GUI is a tiled workbench over one wire: a client that holds only presentation state, artifacts in opaque-origin sandboxes with feedback through questions, and goals and work as records on the same log

- Status: Proposed
- Date: 2026-10-15
- Issues: [#1307](https://github.com/gerchowl/flock/issues/1307) (ADR-0036 slices), [#1310](https://github.com/gerchowl/flock/issues/1310), [#1323](https://github.com/gerchowl/flock/issues/1323), [#1380](https://github.com/gerchowl/flock/issues/1380), [#1401](https://github.com/gerchowl/flock/issues/1401), [#1402](https://github.com/gerchowl/flock/issues/1402), [#1546](https://github.com/gerchowl/flock/issues/1546), [#1076](https://github.com/gerchowl/flock/issues/1076)
- Builds on: ADR-0036 §5–§8, §10a, §12 (wire contract, generated types, reference client, unknown events, collapsible turns, reserved extension points); ADR-0011 (conversation-first GUI, Accepted, not built); ADR-0012 (read model); ADR-0026 (mesh); ADR-0027 (delegate edges); ADR-0030 and ADR-0031 (goals); ADR-0034 (question routing); ADR-0035 (repo owner)
- Wireframes: [`0038-gui/wireframes.html`](0038-gui/wireframes.html) (static, self-contained; open it in a browser)
- Implemented: nothing. This is a design proposal for the owner.

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
- **The client's connection behaviour is unspecified beyond "reconnect with the last seq".** #1546 asks for liveness, reconnect policy and in-flight command rules.
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
kind ∈ transcript | terminal | artifact | queue | inbox | goal_tree | work_graph | node | diff* | context*
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

**Connection lifecycle.** The states and transitions below are normative. #1546 settles the liveness parameters.

```text
Disconnected ──connect──▶ Connecting ──open──▶ Hello ──welcome──▶ Syncing ──caught up──▶ Live
     ▲                         │                 │  ╲                │                       │
     │                         │ fail            │   ╲ protocol gap  │ gap{from,to}          │ heartbeat missed /
     │                         ▼                 │    ▼              ▼                       │ socket error
     └──────backoff────── Reconnecting ◀─────────┘  ReadOnly    snapshot fetch ──▶ Live      ▼
                               ▲                                                       Stale ──▶ Reconnecting
                               └───────────────────── auth refused ──▶ NeedsToken (user action)
```

- **Hello.**
  - It sends `hello { protocol, client, token }`.
  - On `welcome`, it records `capabilities` and `principal`.
  - A server protocol newer or older than the client's puts the client in **ReadOnly**: it can view but not send commands, as ADR-0036 §5's protocol gating requires.
- **Syncing.**
  - For every open tile's streams, it sends `sub { stream, after: cursor }` with the stored cursor.
  - The replay applies through the same reducers as live events.
  - `gap` triggers the stream's snapshot path (ADR-0036 §4 queue snapshots). `lagged` re-subscribes from `skipped_to`.
- **Live.**
  - It dedupes on `(stream, seq)` and advances each cursor only after its reducer has applied.
- **Stale.**
  - Entered when no frame or heartbeat has arrived within the liveness window, which #1546 sets.
  - Tiles keep their content with a visible stale marker. Commands queue in the outbox and are not sent.
  - Reconnecting uses backoff with jitter and a cap. It also reconnects immediately on network-change or foreground events.
- **NeedsToken.**
  - Entered on a refused token: the client clears the stored token and asks the user.
  - A Tauri shell may unlock a stored token with biometrics (§6).

**The store.**
- Entities are kept normalised by kind and id.
- Each stream has a cursor, persisted per device so a reload resumes without a full replay. The `*/live` streams are the exception: they carry deltas with no cursor (§5).
- Ephemeral state lives in the store but is not persisted: in-flight streaming blocks (§4) and the outbox's sent-but-unacked set.

**The command outbox.**
- Every `cmd` gets a `cmd_id` (UUIDv7), is stored with its `verb`, `args` and `created`, and is sent when Live.
- Each command's state goes `pending`, then `sent`, then `acked` or `failed`.
- On reconnect, every `sent` command is resent with the same `cmd_id`. The server's idempotency window (ADR-0036 §5) returns the first `ack`.
- A command older than the window is not resent. It is shown as "unknown outcome" with the server state at hand, and the user re-issues it deliberately.
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

### 5. Contracts: what the wire gains

All types are Rust types with `ts_rs::TS` derives behind the `ts` feature (S8) and appear in `web/client/src/gen`. Each release that adds a stream or event kind bumps `WIRE_JSON_VERSION` once. Older clients fall back to §8's unknown rendering.

**New streams**, alongside ADR-0036 §5's `agent/<id>`, `agent/<id>/live`, `queue/<id>`, `events` and `terminal/<pane>`:

| Stream | Seq | Carries | Snapshot on `gap` |
|---|---|---|---|
| `fleet` | yes | node, project and agent summary upserts and removals in the client's scope | full summary set |
| `inbox` | yes | open questions, permission prompts and feedback requests across agents in scope: opened, updated, closed | open items |
| `artifacts` | yes | artifact record versions in scope, plus comments | records and the latest N comments per artifact |
| `work` | yes | goal and work node upserts and edges | the goal and work subgraph in scope |

**Types** (sketch; names final in S8):

```rust
pub enum StreamId { Agent(AgentId), AgentLive(AgentId), Queue(AgentId), Events,
                    Terminal(PaneId), Fleet, Inbox, Artifacts(ArtifactScope), Work(WorkScope) }

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
                             anchor: Option<BlockAnchor>, text: String, author: Principal,
                             created_ms: u64, delivered: DeliveryState }

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
  - `web/client` builds to static assets once, with `bun build` as ADR-0036 §7 specifies.
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
- **Identity.**
  - ADR-0011 §4's tailnet identity gate becomes optional defence in depth: a client token is required either way.
  - Its funnel refusal and loopback default stand.

### 7. Artifacts, and the artifact MCP

An **artifact** is agent-authored content that a person looks at and answers: an HTML report, a plot, a diagram or a rendered table. It is versioned and immutable per version. It is also untrusted (ADR-0011 §5).

**Publishing (MCP).** These tools are in the flock MCP for `pty` and `structured` agents. They are pre-allowed in the structured launch profile beside `flock_question_ask` (ADR-0036 S6c).

| Tool | Input | Result |
|---|---|---|
| `flock_artifact_publish` | `title`, `kind`, `content` (inline, up to the size limit) or `path` (a file in the agent's worktree), `supersedes?` (artifact id: becomes a new version), `about?` (work or issue ref), `interactive?` (default false) | `{ artifact_id, version, sha256 }` |
| `flock_artifact_request_feedback` | `artifact_id`, `version?` (default latest), `prompt`, `choices?`, `blocking?` (default false), `context?` | It opens a question record whose `about` is the artifact (ADR-0036 §2a). Non-blocking returns the §2a placeholder at once. Blocking waits like `flock_question_ask`. |
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
- The client renders an artifact only inside `<iframe sandbox>` **without** `allow-same-origin`. The frame's origin is opaque, so it has no cookies, no storage, no access to the parent and no access to the client token.
- The content goes in through `srcdoc` (HTML) or a `blob:` URL (other kinds), with a CSP `<meta>` injected first:
  - Static artifacts (the default) get ADR-0011 §5's policy unchanged: `default-src 'none'; img-src data: blob:; style-src 'unsafe-inline'; script-src 'none'; connect-src 'none'; form-action 'none'; base-uri 'none'`.
  - **Interactive** artifacts add `allow-scripts` to the sandbox and `script-src 'unsafe-inline'`. `connect-src 'none'` stays, so a script can draw and react but cannot reach any network.
- This **replaces ADR-0011 §5's distinct hostname** (owner decision 3). An opaque origin gives a stronger boundary than a separate site, and it works the same in Tauri, which has no hostname.
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
  - The parent validates it against the record's `interactive` flag and the size limit.
  - It shows the user what will be sent, and sends it as an `artifact.comment` only on the user's confirmation.
  - Nothing the frame posts reaches an agent without a person confirming it.

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
- **No embedded versioned graph engine** in this record.
  - A content-addressed, branchable graph store was evaluated. Branching and three-way merge of the work graph would let agents plan on a branch and merge.
  - But flock's needs today are append, fold and project. The event log already is the version history. And such an engine brings its own crash-safety, reclamation and growth risks into the server process.
  - The record shapes above keep that option open: every change is an event with an actor and typed edges.
  - Owner decision 8.

### 9. Mesh

- The `fleet` stream on node A includes nodes and agents learned over the mesh (ADR-0026), with `route` from the actual path, which fixes #1076's "via itself".
- A tile for an agent on node B reads B's state and messages through A, which is ADR-0036 §5's rule.
- Commands for B's agent are refused with `unsupported_for_agent` until #1380.
- Artifacts published on B are visible on A when their record and blob have been carried. That needs a blob fetch over the mesh, which is a slice of its own (G11) and out of the MVP.

### 10. Slices

Each slice is at most about 400 production lines; TypeScript counts the same. **Before S3** means it can start now, independent of the -p chain (#1518 → #1439 → #1426 → …).

| # | Slice | Depends on | Before S3? | Est. |
|---|---|---|---|---|
| G1 | Client shell skeleton (Bun + Solid), with a fixture transport replaying recorded S1 session fixtures (#1379) through generated types | S8a | yes | 400 TS |
| S8a | `ts` feature and ts-rs derives for the existing socket API types (agents, workspaces, panes, questions); drift test, `just gen-ts`. This splits S8 so the client can start before S7. | — | yes | 200 |
| G2 | Tile manager: layout tree, stacks, rail, the open/expand/collapse/close operations, per-device persistence, the phone mode | G1 | yes | 400 TS |
| G3 | Client state machine: the §3 lifecycle, cursors, store, outbox; tested against a scripted fake server (connect, gap, lagged, stale, protocol gap, token refusal) | G1, #1546 settled | yes | 400 TS |
| S7 | Wire endpoint (ADR-0036), with #1546's liveness | S3 (S3a #1518/#1439, S3b #1426), #1323 decided | no | 400 |
| S8b | Remaining ts derives for the session events and the S7 wire messages | S7, S8a | no | 150 |
| G4 | `fleet` stream and the sidebar *Fleet* and *Projects* trees, merging nodes by `node_id` (#1076) | S7 | no | 350 |
| S9a | Transcript tile (ADR-0036 S9a) on G2/G3 | S8b, G2, G3 | no | 400 TS |
| G5 | `inbox` stream and inbox tile, with atomic `inbox.answer` | S7, S6b | no | 350 |
| G6 | Artifact records, blobs, limits, retention; the `flock_artifact_publish`, `_list` and `_comments` tools; the `artifacts` stream | S7 (records only need S3) | partly | 400 |
| G7 | Artifact tile: sandbox, CSP injection, block anchors, comments, `expand_into_second`; `flock_artifact_request_feedback` | G6, G5, S6c | no | 400 (TS + Rust) |
| G8 | `work` stream, phase 1 projection, goal tree tile | S7 | no | 350 |
| G9 | Tauri v2 desktop shell: config, keychain token, accessory bar, deep links, tray | G1–G3 | yes (shell only) | 300 + config |
| G10 | Tauri mobile and push, if decided | G9, owner decision 6 | no | — |
| G11 | Artifact carriage over the mesh | G6, ADR-0026 | no | 300 |
| G12 | `work` records, phase 2 (§8) | G8, owner decision 7 | no | 400 |

The first end-to-end milestone is **"a structured agent in a browser tile"**: G1 + G2 + G3 + S7 + S8b + S9a, against the -p MVP's runtime. It follows the MVP and does not compete with it.

### 11. Owner decisions required

| # | Decision | Recommendation | Cost |
|---|---|---|---|
| 1 | Supersede ADR-0011 §3's direct-socket desktop transport with ADR-0036 §5's WebSocket and tokens everywhere | Yes | A desktop app on the server host needs a token. `flk client open` already makes one. |
| 2 | Allow opt-in **interactive** artifacts: scripts in an opaque-origin sandbox, no network, and posts confirmed by a person (ADR-0011 §5 requires an ADR for any script relaxation) | Yes, opt-in per publish, off by default | One more security surface to review. Agents can render live plots and forms. |
| 3 | Replace ADR-0011 §5's distinct hostname with the opaque-origin sandbox | Yes | Its boundary is the browser's sandbox, which is well tested. No second DNS name or listener. |
| 4 | Solid web client vs GPUI (#1401) | Solid, as ADR-0036 decided. GPUI has no HTML engine for artifacts and no browser reach. | The Bun/TS toolchain stays. |
| 5 | #1323 (Grafana stack) gates S7 | Decide "ours" now for the live wire and history, revisiting Loki for long-term history later | We keep the JSONL and blob store we already have. |
| 6 | A push relay for mobile background notifications | Defer. Mobile v1 is foreground only. | No pings while the app is closed. |
| 7 | `work` records as first-class flk state (§8 phase 2) | Yes, after phase 1 has been used | Architects move their task files into flk. |
| 8 | Adopt an embedded versioned graph engine for goals and work | Not now. Revisit when branch-and-merge planning is a real need and a candidate engine is mature. | No branch and merge of plans for now. |
| 9 | Layout persistence | Client-local per device now; server-synced named layouts later | A layout doesn't follow you between devices. |

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

## Alternatives considered

- **Amend ADR-0036 instead of a new record.** Rejected. ADR-0036 is the session runtime's record. A GUI record also has to reconcile ADR-0011 and ADR-0012, and add records for artifacts and work that are not session concerns.
- **A server-rendered UI (htmx or similar).** Rejected. ADR-0036 decided client rendering, and it would need a second render path for Tauri.
- **Layout synced through the server from day one.** Deferred. There is no multi-device demand yet, and a layout would become state that someone must migrate.
- **Artifacts on a second hostname (ADR-0011 §5).** Superseded by the opaque-origin sandbox. A second hostname needs DNS or a tailnet node per install and doesn't exist in Tauri.

## Consequences

- The client becomes testable against a scripted fake server before S7 exists (G1–G3 with S8a).
- Agents get one way to show a person something and get it answered, through the question and queue machinery that already exists.
- Goals and work become visible without new storage (phase 1). Phase 2 moves planning state out of private files into flk's log.
- ADR-0011 §3, §4 and §5 are partly superseded if decisions 1 and 3 are accepted. ADR-0011 will need a status note.
