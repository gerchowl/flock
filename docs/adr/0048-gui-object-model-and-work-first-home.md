# ADR-0048: The GUI is mission control for delegated work: object model and work-first home

- Status: Accepted (2026-11-03, owner). OD-48.6 (views as typed queries, D9) was added at
  acceptance.
- Date: 2026-11-03
- Issues: GUI-P0 [#2895](https://github.com/gerchowl/flock/issues/2895),
  [#2896](https://github.com/gerchowl/flock/issues/2896),
  [#2899](https://github.com/gerchowl/flock/issues/2899),
  [#2900](https://github.com/gerchowl/flock/issues/2900)–[#2906](https://github.com/gerchowl/flock/issues/2906),
  [#2871](https://github.com/gerchowl/flock/issues/2871),
  [#2872](https://github.com/gerchowl/flock/issues/2872); GUI-P1 depends on
  [#1597](https://github.com/gerchowl/flock/issues/1597) (G5 inbox stream),
  [#1600](https://github.com/gerchowl/flock/issues/1600) (work projection),
  [#1603](https://github.com/gerchowl/flock/issues/1603) (work records),
  [#2726](https://github.com/gerchowl/flock/issues/2726) (fork progress);
  GUI-P2 depends on [#2293](https://github.com/gerchowl/flock/issues/2293)
  (multi-node) and [#2733](https://github.com/gerchowl/flock/issues/2733)
  (mobile delivery).
- Decision owner: repository owner. The owner asked for this ADR on 2026-11-03
  ("go for all"), after two independent GUI-vision reviews (a product review
  and a systems review, 2026-11-02) agreed on its thesis. The owner decided
  OD-48.1 to OD-48.6 on 2026-11-03 (recorded in "Owner decisions" below and in
  the sections they touch) and accepted the ADR the same day; OD-48.6 was added
  at acceptance.
- Builds on: ADR-0027 §1, §1a and §2 (delegation edges, orphaned edges,
  `delegate.list`), ADR-0028 §1
  and §8a (state axes, open questions), ADR-0030 §1–§3 and ADR-0031 (goal
  records), ADR-0032 §1 (five context layers), ADR-0033 §1 and §3 (AX forks),
  ADR-0034 §2 and §6 (question policy, permission routing), ADR-0035 §1a and
  #1167 (owner → segment architect → builder), ADR-0036 §2a, §3, §9, §10b, S6e
  (including "Authorization and proposals", #2585) and
  S12 (questions, input queue, forks, permission presets), ADR-0038 (GUI
  workbench), ADR-0041 §1, §5, §6 and ADR-0042 §3–§5 (configuration),
  ADR-0043 §5 and §7 (federated fleet, partial failure), ADR-0044 Q1
  (structured lifecycle), ADR-0045 D1, D2, D7, D8 (durable work: the spine),
  ADR-0047 (tailnet origin, unchanged here).
- Amends, on acceptance: ADR-0038 §1 (entity table and sidebar trees), §2
  (tile kinds admitted to the workbench), §2a (MVP screen, deferred-part rule,
  header actions, phone *Terminal* tab, desktop visual baseline) and §11 item
  13's consequence ("the sidebar folds ended threads"). The amended text lives
  here; ADR-0038 itself is not edited (README: decided ADRs are immutable).
- Forward reference: ADR-0049 (agent types, workflows and one authority
  model) is proposed in parallel (#2908). ADR-0049 owns workflow definitions
  (W2 "Workflow definitions"), the run runtime (W4 "Runs") and the authority
  model, including authority loss and rerouting (W5 "One authority model",
  "Authority loss: expiry, revocation and orphaning"). This ADR owns only
  their GUI projection: Run as a GUI object, the workflow definition as a
  facet (D2), and how the inbox renders delegated and rerouted items (D4).
- Implemented: none. The baseline below describes `origin/dev` at
  `c70c23c383ec`.

## Context

ADR-0038 built the GUI as a tiled workbench over one wire and chose the
"Mission control" screen for the MVP (§2a, owner decision 11). What shipped
from it is an agent-first chat client: a flat list of structured agents, one
transcript with a composer, and a permission-only inbox. Two reviews of that
client on 2026-11-02 reached the same conclusion from different directions:

- **Product review:** flock's GUI is a *supervision console for delegated
  work*, not an IDE, a chat app or an ops dashboard. The home must answer, in
  order, *what needs me, what is moving, what is proven*. Work is the noun,
  agents are the verbs, and chat is a drill-down.
- **Systems review:** the GUI is *mission control for accountable delegated
  work*. Projects and work give continuity while agents start, fork,
  hibernate and disappear. Every action needs an explicit authority, and the
  GUI must not invent state the server cannot restore.

A UX review of the same build listed concrete defects: a palette that opens
agents by id that are not in the list and a composer that stays enabled for
them (#2871), developer-facing deferred controls (`Waits` captions naming
"ADR-0036 S17"), two header bars, a passive permission-only inbox with no
count, touch-sized controls on desktop, raw protocol error codes, and a
transcript that shows raw event types. The owner's own critique (2026-11-02)
added: an integrated composer, one icon set, the connection controls in a
popover, no app-name title, hideable and resizable sidebars, and no
*Terminal/Diff/Fork* pills. The owner also directed on 2026-11-02 that
structured sessions replace terminal agents in the GUI.

Those defects are being fixed one by one (#2900–#2906). Fixing them does not
answer the larger question: *what is the GUI's subject?* ADR-0045 now gives
flock a durable, server-owned work record with stable ids, a DAG, assignment,
typed evidence, proposed-then-approved completion and an escalation chain
that ends at a GUI endpoint (D1, D2, D7, D8). ADR-0038 predates it and still
models goals and work as two entities (§1) and three sidebar trees. This ADR
makes the work record the GUI's spine and fixes the object model before more
surfaces are built on the agent-first shape.

### Inspected baseline

| Current behaviour | Evidence at `c70c23c383ec` |
| --- | --- |
| The wire serves two stream families: `fleet` and `agent/<id>`. The subscription dispatcher accepts only those two and refuses any other name with `UnknownStream`; there is no `inbox`, `work`, `queue/<agent>`, `artifacts` or run stream on the GUI wire. Every projection this ADR relies on is a proposed server object, not an existing one. | `src/web/wire/subscriptions.rs:114-120` |
| A fleet project row is keyed by the exact checkout path on one owning node: `FleetProjectRow.id` is `Record.cwd`, beside `node_id`. Two checkouts of one repository, on one node or on two, are two rows. | `src/structured/fleet_lite/wire.rs:108-118` (#2721) |
| The fleet-lite row and counts cover local structured agents only; `FleetAgentCounts.total` is the sum of disjoint phase buckets. | `src/structured/fleet_lite/wire.rs:100-101` |
| The palette can open an agent by id and the composer stays open for an agent missing from the list. | `web/client/src/workbench/Palette.tsx`, `web/client/src/workbench/Composer.tsx` (#2871) |
| Deferred parts render as captions that name ADR slices. | `web/client/src/workbench/Workbench.tsx:138` (`waitsFor="the harness picker (ADR-0036 S17)"`), `web/client/src/workbench/parts.tsx` |
| Permission decisions reuse a typed refusal map in one place only. | `web/client/src/workbench/decide.tsx` |
| Work records (ADR-0045 D1), completion proposal/approval (D2), work verbs (D7) and escalation (D8) are accepted design, not code. | ADR-0045 "Implemented: none" |
| Question records with blocking reasons, threads and proposals are accepted design (S6e), not an endpoint. | ADR-0036 S6e "design only, post-MVP" |

## Decision

Nine decisions follow (D9 was added at acceptance). Each states the recommendation, the rejected
alternatives with their cost, and the acceptance tests that must fail before
the change and pass after it. Risks are in "Consequences, risks and
pre-mortem"; the phased plan maps every decision to issues.

### D1. Thesis and home: three questions, work-first, chat as drill-down

**Recommendation.** The GUI is **mission control for delegated work**. Its
home screen answers three questions, in this order, and nothing else:

1. **What needs me.** The attention queue (D4), ranked, with a count in the
   toolbar, the window title and the top of the sidebar.
2. **What is moving.** Active Work (D2) with its assignee's phase (ADR-0028
   §1), progress, waiting subject and expiry, and pending wake age (ADR-0045
   D7's projected fields). Where no Work exists yet (before G8/#1600), the
   band shows structured agents by phase from the `fleet` stream.
3. **What is proven.** Recently *verified* Work (D5): approved completions
   with their evidence head. Claimed-but-unverified Work is never shown here;
   it is in band 1 as a completion-approval card or in band 2 as
   `review_pending`.

Selecting any row opens its subject (D7). A transcript and composer are what a
person reaches by drilling into an Agent, never the default screen. The phone
keeps ADR-0038 §2a's inbox-first layout but **drops its *Agents* tab**: the
bottom bar is *Inbox* (band 1) and *Activity* (bands 2 and 3). An Agent is
reached by drilling into the Work it serves or the card that names it, as on
the desktop.

**The GUI-P0 badge is provisional.** Until the `inbox` stream (G5, #1597) sends a
`needs_you` count, GUI-P0 shows the `fleet` stream's
`FleetAgentCounts.waiting_on_human` (agents with open non-automatic
requests). That counts agents, not attention items: one agent with two open
prompts counts once, a supervision gap counts not at all, and a `null` (unknown,
as it is for remote nodes today) renders as unknown, not zero. The GUI-P0 badge
is labelled and tested as an agent count; A1.3's exact
`needs_you` assertion applies only once the server field exists.

Five principles bind every later surface:

- Attention is the product.
- Work is the noun; agents are the verbs.
- A claim is not evidence.
- The client invents no state (D2).
- Steer through records, not keystrokes.

**Rejected.**
- *Chat-first home* (today's shape): the person must open each agent to learn
  whether it needs them. It scales to two agents, not to a fleet.
- *Agent board as home* (ADR-0038 §2a's rejected alternative): shows every
  agent and hides what needs the person. Still rejected.
- *Ops dashboard home* (node health, token spend, throughput tiles): answers
  questions the operator rarely asks and buries the three that matter.
  Telemetry stays a diagnostics view (D7).

**Acceptance.**
- A1.1: with a scripted fake server holding one open blocking question, one
  working agent and one approved Work, the first screen after `synced` shows
  them in bands 1, 2 and 3 respectively, with no transcript tile open.
- A1.2: a Work in `review_pending` never appears in band 3 (Playwright query
  over band 3 returns zero rows for it).
- A1.3: `document.title` and the toolbar badge equal the server's
  `needs_you` count, and change only on a server frame (no client timer).
  Gated on G5 (#1597). Until then A1.3-GUI-P0 asserts the provisional badge equals
  the `fleet` snapshot's `waiting_on_human`, shows unknown for `null`, and
  changes only on a `fleet` frame.
- A1.4: the phone layout's bottom bar has exactly the *Inbox* and *Activity*
  tabs; no *Agents* tab exists in the production build.

### D2. Object model: six navigable objects, the rest are facets; the client invents no state

**Recommendation.** Six kinds are first-class: each has a stable server id,
a route, a detail view and a row in search (⌘K).

| Object | Identity and source of truth | Relations (all server-owned edges) |
| --- | --- | --- |
| **Project** | A registered checkout: the ADR-0042 C2 project scope on one node (ADR-0004's main checkout and common directory). Today's `FleetProjectRow` key is the raw path; see "Project identity migration" below | contains → Work; owned_by → the resident owner Agent (ADR-0035); runs on → node; carries the `repo` identity property, by which views group checkouts across nodes (D9) |
| **Work** | `WorkId` (ADR-0045 D1). A **goal is the root Work** of a DAG, not a separate entity. Rows in ADR-0045 D7's projection phase carry projection view ids, which are not mutation targets | parent/child and blocks (DAG); assigned_to → Agent (assignment generation); supervised_by → Agent; evidence → typed descriptors (D5); about ← Question, Artifact; implements → issue; delivered_by → PR |
| **Agent** | `AgentId` (ADR-0008); structured only (D3) | delegated_by → Agent (ADR-0027 §1 edge; role from a server role field, D3); works_on → Work; forked_from → Agent (lineage, ADR-0036 §9) |
| **Question** | `question_id` + revision (ADR-0036 S6e `QuestionRecord`) | asked_by → Agent; addressed_to → Agent or operator; about → Work or Artifact; threads → fork Agents; proposals |
| **Artifact** | `artifact_id` + version (ADR-0038 §7) | published_by → Agent; about → Work; annotated_by → comments, Questions |
| **Run** | Before ADR-0049 P2: a run projection view id; from ADR-0049 P2: a durable server-minted `RunId` (ADR-0049 W4 "Runs"). See "Run projections and RunIds" below | instantiates → exactly one `WorkflowDefinitionId { project, workflow_id, def_hash }` (ADR-0049 W2 "Workflow definition identity"); drives → Work (linked WorkIds); launched_by → launcher principal; acts as → run principal `Run(RunId)` (W4 "Two principals"). Not rendered until ADR-0049 is accepted and its stream exists |

Everything else is an **attribute or facet** of one of those, shown inside
its owner's view and never as a top-level navigation root:

| Facet | Of | Shown as |
| --- | --- | --- |
| Node | Agent, Project | a badge (name, health dot, freshness) with a popover; ADR-0043 §5 classifications rendered as sent |
| Role and tier | the delegation edge | an edge label (owner, segment architect, implementer, reviewer, judge) from a server role field. ADR-0027 §1's `AgentDelegated` has no role (its `label` is free caller text, never a role); the owner comes from ADR-0035's binding and the segment architect from ADR-0035 §1a's `SegmentAssigned`. The fleet row carries no role field yet: it is a D3 prerequisite, and until it exists no tier label is shown. The client never infers a tier from names, labels or depth |
| Workflow definition | Project (adopted definitions), Run (its pinned definition) | ADR-0049 W2 "Workflow definitions" owns the definition (file, schema, validation, adoption by hash); this ADR owns only its GUI facet: a read-only definition view keyed by `WorkflowDefinitionId { project, workflow_id, def_hash }` as W2 "Workflow definition identity" defines it, reached from the Project's adopted-definitions list and from every Run; not a navigation root and not in search as a seventh kind |
| Transcript and input queue | Agent | the Agent's conversation view (D6) |
| Evidence | Work | the evidence panel (D5) |
| Rules and context | Project, Agent | a context inspector with ADR-0032 §1's five layers and each layer's provenance |
| Configuration | Node, Project, Agent | the settings inspector (D8) |
| Permission prompt | Agent | an attention card (D4) and the Agent's state; not an object of its own |
| Notification | any | a toast or attention card referring to an object; ADR-0016's log is not navigable as a list of truth |

**Projections are proposed server objects.** The `work`, `inbox`,
`queue/<agent>`, `artifacts` and run streams that back these objects do not
exist: today's dispatcher accepts only `fleet` and `agent/*`
(`src/web/wire/subscriptions.rs`). Each is a new server stream with its own
issue (phased plan), and the GUI renders an object kind only once its stream
exists. Until then a kind is hidden (D7 amendment 2), never synthesised from
`fleet` rows.

**Project identity migration.** Today `FleetProjectRow.id` is the exact
checkout path qualified by `node_id` (`src/structured/fleet_lite/wire.rs`,
#2721). D2's Project is a registered repository identity spanning nodes. The
transition is server-owned and explicit:

1. The server adds a `project_id` (the ADR-0042 C2 registered checkout
   identity on that node) to each project row. The existing `(node_id, id)`
   pair stays as the row's path attribute, never dropped.
2. **Cross-node identity is a property, not a merge** (OD-48.6). Each
   checkout stays its own Project object. The server also sends a built-in
   `repo` identity property (the normalised remote URL, computed by the
   server); a view that groups by `repo` (D9) shows two checkouts on
   different machines as one group. The client never computes `repo` itself
   and never merges Project objects by basename, display name or URL.
3. A row without `project_id` (an older server or an unregistered checkout)
   renders as its own Project labelled *unregistered checkout*, keyed by
   `(node_id, id)`.
4. Turning `project_id` on is a `fleet` snapshot reset with a committed
   mapping from checkout keys to project ids, as ADR-0045 D7 requires for the
   work phase change, so a client never holds both identities for one row.
   Per-project policy (D4) and settings (D8) are keyed by `project_id` and are
   unavailable for an unregistered checkout.

**Run projections and RunIds.** ADR-0049 P1 (W8 "Phasing and acceptance")
shows existing loops, delegate edges, queued start intents and armed goals as
implicit runs before any run record exists. Those rows follow ADR-0045 D7's
projection-phase rule: their ids are **projection view ids** derived from the
existing source key (loop id, edge, start intent or goal key, prefixed by
source kind), they may be reused on legacy re-arm, and they are not durable
and not mutation targets; the Run view offers no verb on them. When ADR-0049
P2 mints `RunId`s (W4), entering that phase is a complete snapshot reset of
the run stream with a committed projection-id → `RunId` mapping, exactly as
ADR-0045 D7 specifies for legacy-ID → `WorkId`. The client drops every
projection id at the reset and never maps one to a `RunId` itself. The same
rule holds for Work rows (projection-phase view ids, record-phase `WorkId`s).

**Run and step states are rendered as sent** (ADR-0049 W4 "Runs"). A Run in
`awaiting_approval` has finished its steps but is not done: it shows as
awaiting completion approval of its Work (a D5 claim, in band 1 or 2, never
band 3), and only `succeeded` counts as finished. A step instance in
`handled` is a failure whose failure handler was admitted: it shows as failed
with its handler, never as success and never hidden, even though the run can
still reach `awaiting_approval`. A refused approval shows the server's
`self_approval` refusal in plain language (D5).

**The client invents no state.** This restates ADR-0038 §3 and extends it to
the new objects. The client may group, sort, filter, count server-sent
items, format times and lay out tiles. It must never derive: Work status,
coverage or approval; whether a question is open or blocking; attention rank;
who waits on an item; agent phase or node health; queue order or delivery;
permission outcomes. Where the server has not sent a fact, the client shows
*unknown*, never a guess (ADR-0043 §5, ADR-0045 D7 "explicitly showing
unknown/unimplemented facts").

**No drag-to-change-state.** Every mutation is an explicit verb with an
expected revision and an idempotency id (ADR-0045 D7; ADR-0042 §5's pattern
for config). There is no kanban column drag, no drag-to-reassign and no
drag-to-reorder of a landing queue. A landing order is a recorded decision
made through a verb, shown as such. Dragging is allowed only for layout
(splitters, tile placement), which is client presentation.

**Rejected.**
- *Goal, Task and Work as three stores or three entities*: divergent status
  and duplicate ownership; ADR-0045 D7 already rejected independent goal/task
  stores, and the systems review repeats it.
- *Node as a navigation root* (ADR-0038 §1's *Fleet* tree; the UX review's
  "one Node→agents tree"; dropped as the fixed default by OD-48.5, 2026-11-03;
  a view may still group by node, D9): nodes are
  where work runs, not what the person is supervising. A node outage still
  surfaces, as a supervision-gap card (D4) and as stale badges.
- *Client-side optimistic work state* (move a card, reconcile later): the
  first restart shows a board the server cannot restore. This is the
  "object-model drift" failure in the pre-mortem.
- *Workflow definitions as a seventh navigable object*: a definition has no
  lifecycle of its own that needs supervising; its runs do. It is a facet of
  Project and Run (above), and it needs ADR-0049's runtime before it is drawn
  at all; drawing it earlier produces diagrams with no executor.

**Acceptance.**
- A2.1: `ObjectKind` is a closed union generated from the server's Rust
  enum, and a type test asserts it equals exactly `"project" | "work" |
  "agent" | "question" | "artifact" | "run"` in both directions (each member
  assignable to it, and it assignable to that literal union). The router,
  the search index and the detail-view registry each switch over it with an
  `assertNever` default, so adding, removing or renaming a kind without a
  route, a search row and a view fails to compile. A parity check alone is
  not enough: the test fails if the union widens to `string`.
- A2.2: structural, not a heuristic lint. Every server-sourced field in the
  client store is typed `readonly` (deeply, from the generated types), and
  the only writable handle to the server slice is private to the frame
  reducer module, which applies `ev`/`snapshot`/`gap` frames. A type test
  (`@ts-expect-error` assignments to `work.status`, `item.rank`,
  `question.blocking` and `agent.phase` from a component) fails to compile if
  any becomes writable, and an import-boundary check fails if any module other
  than the reducer imports the writable handle.
- A2.3: the fake server sends a Work without `coverage`; the Work view shows
  *unknown*, not *armed*.
- A2.4: a source check over the client: drag handlers (`draggable`,
  `onDrag*`, pointer-drag listeners) appear only in an allowlist of layout
  components (splitters, the tile manager), and no module in that allowlist
  imports the outbox or any verb builder; the outbox's send entry point has no
  caller reachable from a drag handler (import-graph check). Adding a drag
  handler outside the allowlist, or an outbox import inside it, fails the
  build.
- A2.5: across a run-stream snapshot reset that introduces `RunId`s (and the
  same for `WorkId`s), the client holds no projection view id afterwards and
  offers no verb on a projection row before it; a fixture asserts both.

### D3. Sidebar: project → delegation tree, structured agents only, node as a badge

**Decided (OD-48.5, 2026-11-03): the sidebar is project →
delegation, and there is no node tree.** Under OD-48.6 this structure is the
default view preset (a) of D9, not a fixed structure; the rest of this section
describes that preset. Each **Project is a section
header**, not a tree level: the header carries the project name and its
resident owner (name, phase mark, attention count), and selecting it opens
the Project. Below it the delegation tree has **at most two disclosure
levels**, as the macOS HIG recommends for sidebars: segment architects, and
their implementers as children (the owner → segment architect → builder
design of ADR-0035 §1a and #1167). Where a project has no segment
architects, the owner's implementers are the first level. The tree is built
from `delegate.list` edges (ADR-0027 §2) through the `fleet` and (later)
`work` streams. Each row shows the agent's name, the Work it is assigned to,
its phase mark, its node badge, its count of open attention items, and its tier
label. **Prerequisite:** the tier label needs a server role field on the
delegation row, which does not exist yet (ADR-0027 §1's edge has none; D2's
"Role and tier" facet). Until it ships the tree shows structure from edges and
no tier label. Above
the sections sits a *Needs you* row with the D4 count. Unattached root agents
(started by the operator with no Project binding) form an *Unassigned*
section.

- **Ephemeral agents are not sidebar rows.** Reviewers, falsifiers, AX forks
  (ADR-0033 §3: in place, reaped on report), System One judges (ADR-0030 §3)
  and question-thread forks (ADR-0036 S6e) appear as **evidence rows on the
  Work** or as thread chips on the Question they serve, with a link to their
  retained transcript. This amends ADR-0038 §11 item 13's consequence: ended
  threads fold into the Question's history, not the sidebar.
- **Node is a badge, not a group.** A toolbar health dot opens a fleet
  popover listing nodes with ADR-0043 §5's server classifications. A scope
  control filters the tree to this node or all nodes.
- **Never PTY agents.** The GUI shows structured agents only (standing owner
  direction, 2026-11-02). The filter is the server's: fleet rows, counts and
  `FleetAgentCounts.total` must be structured-only by contract, so no client
  filter is needed and no PTY agent leaks into a count. No GUI surface counts,
  mentions or advertises PTY or terminal agents: no "N terminal agents not
  shown" hint, no "open in the TUI" prompt, no empty-state copy pointing at
  them. PTY agents remain the TUI's.

**Rejected.**
- *ADR-0038 §1's three trees* (*Fleet*, *Goals*, *Projects*): the same agent
  appears three times, and the person must pick a lens before seeing anything.
- *A flat agent list* (today): loses who answers to whom, which is the one
  relation a supervisor needs.
- *Showing reviewers and forks as agents*: a busy wave adds a dozen
  short-lived rows that churn every minute; they are evidence of the work,
  not participants in its supervision.
- *Including PTY agents with a "terminal" badge*: contradicts the owner's
  direction and needs terminal tiles the workbench drops (D7).

**Acceptance.**
- A3.1: a fixture with owner → two segment architects → three builders
  renders one project section header carrying the owner, two first-level
  architect rows and the builders as second-level children of their own
  architect, not siblings; no row sits deeper than two disclosure levels
  below the header.
- A3.2: an in-place AX fork and a thread fork in the fixture produce zero
  sidebar rows and one evidence row (or thread chip) each.
- A3.3: a server-side test: with one PTY and one structured agent running,
  the `fleet` snapshot contains one agent row and `total == 1`; and the A7.2
  copy check fails on any static string that mentions PTY or terminal
  agents.
- A3.4: a node marked `stale` by the server shows a stale badge on its agents;
  the client never marks a node stale on its own timer.

### D4. Attention: one server-projected, typed, ranked queue

**Recommendation.** "Needs you" is **one queue, computed by the server**, of
typed items addressed to the operator. Six kinds:

| Kind | Source record | Primary actions |
| --- | --- | --- |
| Permission | live control request (ADR-0036 §10b, S6) | Allow once, Allow always, Deny |
| Blocking question | `QuestionRecord` with `blocking: true` and its reason (S6e) | choices, Other, *Take as thread* |
| Failure / limit / login | ADR-0028 §1 `blocked{provider_limit, auth, resume_failed, crashed}` and `needs_human{login, trust}` | the typed remedy the server offers (retry after, re-login instructions, restart) |
| Escalation | ADR-0045 D8 durable escalation at operator stage | open the Work; `work.escalation.ack` (a bounded lease, not a resolution) |
| Completion approval | root or operator-supervised Work in `review_pending` (ADR-0045 D2) | open the evidence panel; Approve or Return (D5) |
| Supervision gap | **protected** Work (ADR-0045 D1: activating it requires its required loops to be installed) whose coverage the server reports as `unarmed` or `degraded`; a delivery-degraded endpoint; an unreachable owner node | open the Work or node; the remedy the server offers |

Unarmed coverage on unprotected Work is normal and is not an attention item;
the Work view shows its coverage state as sent.

**Non-blocking questions count on the badge by default** (OD-48.4,
2026-11-03). They are listed below the blocking items, as ADR-0036 S6e orders
them, and count toward the toolbar badge unless the project's policy turns
that off (a per-project toggle, below).

- **Ranking is server-side and configurable by typed preset** (OD-48.2,
  2026-11-03). The ranking is a per-project policy value (the project policy
  below) naming a **named preset**. Under OD-48.6 a preset is a saved
  attention view (D9): its sort keys form a lexicographic key over typed
  properties. There are no free-form weights
  and no client-side ranking. The default preset, `blocking_parked_age`,
  orders blocking items first; then by the number of agents parked behind the
  item (the asker plus every agent whose registered wait, ADR-0028 §5, or
  delegation edge resolves to it); then by age. This refines ADR-0038 §1 and
  S6e's "blocking, urgency, age" by inserting *parked behind*. The server
  computes the order under the project's preset and ships `rank` and the
  preset name with each item; the client never re-ranks.
- **Grouping, cap and copy.** The inbox groups items by the Work they unblock
  (items with no Work group by asker), groups ordered by their best-ranked
  item and items by server `rank` within a group; grouping is presentation
  over server fields (D2). It shows at most three items per agent
  (the attention view's per-level cap, D9, default 3) and folds the rest into a
  "+k more from *agent*" row; the badge and counts always include every item.
  Card copy states the **effect first, then the next step**: what answering
  changes ("Unblocks 2 agents on *Work*"), then what the person does
  ("Allow once or deny `cargo test`"), never the raw request type first.
- **Every card says who waits and what answering unblocks.** The server
  sends `waiting: [AgentRef]` and `unblocks: [WorkRef]` with the item, plus
  the asker and recipient as attested identities, separate from the untrusted
  body (S6e "GUI, TUI and CLI contracts").
- **Per-project "what reaches me" policy.** An operator-owned setting keyed
  by `project_id` (ADR-0042 C2 scope, stored outside the checkout, never in a
  committed `.flk.toml`): for each kind, *badge and notify*, *badge only* or
  *list only*; the ranking preset; the per-agent visible-item cap; and the
  non-blocking-questions-on-badge toggle (default on). It defaults the
  question policy of new root agents in that project (ADR-0034 §2) and the D8
  notification level. Protected kinds cannot be lowered below *badge and
  notify*: permissions, blocking questions, operator-stage escalations and
  root completion approvals. Repository content cannot change what reaches the
  operator.
- **Permission authority: operator, or a decide delegation** (OD-48.1,
  2026-11-03). An architect may answer a structured delegate's permission
  prompt, or a delegable `request-decision` question (ADR-0049 W3), **only
  through ADR-0049 W5's `DecideDelegation`**: operator-issued, scoped,
  revocable and audited; ad-hoc delegations are finite and only adopted
  defaults may be standing (ADR-0049 W5). Every other permission decision
  needs `operate`: `agent.decide` requires `operate` or the existing specific
  operator authority (ADR-0036 S6e "Authorization and proposals", #2585,
  restating §10b's S6 rule). A question card calls `question.answer` under
  `answer` or `operate` (the same table). The two handlers stay separate even
  when rendered side by side; a principal without the authority sees the
  action disabled with the authority it needs.
- **Every pending decision item has a server-owned addressee.** This covers
  permission prompts and ADR-0049 `request-decision` questions (W3) alike.
  The server stores each item's addressee when it is raised, as ADR-0049 W5
  "Addressee assignment" specifies; the inbox projects that stored value and
  never computes, re-resolves or guesses one. The GUI's addressee type is
  generated from W5's stored shape. A decision from anyone other than the
  operator or the stored addressee is refused `not_addressee`, and the GUI
  renders that refusal in plain language. A delegated item is an evidence row on its Work (or Run)
  naming the delegate and the delegation, not an operator card; the operator
  can still decide it there under `operate`, since a delegation never removes
  operator authority.
- **Authority loss is ADR-0049's transition; the inbox only projects it.**
  ADR-0049 W5 "Authority loss: expiry, revocation and orphaning" owns when a
  pending item is rerouted to the operator and why. This ADR defines no
  trigger and no cause list of its own. The `inbox` stream projects each
  committed `PermissionRerouted` record: the item keeps its id, becomes an
  operator item showing the record's `reason`, and the new `needs_you` count
  arrives in the same frame sequence as the rerouted item. ADR-0045 D8 escalation for it starts
  at the reroute, as W5 specifies. The Work's evidence row keeps the history
  ("rerouted to operator: *reason*"). The GUI's reason type is a closed union
  generated from the reasons ADR-0049 W5 defines and rendered through an
  exhaustive switch, so a reason added there fails the client build until it
  has copy; the GUI never maps or collapses reasons. A late answer from the
  former delegate is refused at the mutation fence with `delegation_lapsed`
  (W5), and the evidence row shows the refused late answer. A reroute is not
  reversed when a later delegation appears (W5); the GUI offers no hand-back
  verb while ADR-0049's owner question 6 is open. At no point is a pending
  item in neither place: it is always an operator item or a delegated
  evidence row.
- **The inbox is not the escalation guarantee.** It projects ADR-0045 D8's
  attention record. Reading or dismissing a card does not acknowledge an
  escalation or suppress reminders; only `work.escalation.ack` does, and only
  for a bounded lease.

**Rejected.**
- *Separate inboxes per kind* (permissions here, questions there, failures in
  a log): the person polls three places, and none can rank against the others.
- *Client-side ranking or a weighted score in the browser*: two clients would
  disagree, and the client would derive state ADR-0038 §3 forbids.
- *A weighted product (blocking × parked × age), or operator-tuned free-form
  weights*: harder to explain on the card and to test, and every project
  becomes a different ranking nobody can reason about; named lexicographic
  presets keep the intent with a stated order (OD-48.2).
- *Operator-only for every structured permission*: strands unattended trees
  on routine in-worktree prompts (ADR-0049 W5's argument); rejected by OD-48.1.
- *Architect authority from the delegation edge alone*: invisible and
  unrevocable; rejected by ADR-0049 W5 and OD-48.1.
- *Letting a project file lower protected kinds*: a repository could silence
  the operator's permission prompts.

**Acceptance.**
- A4.1: a fake server sends items of all six kinds; the inbox renders six
  distinct card types, in exactly the server's `rank` order, and re-orders only
  when the server re-sends rank.
- A4.2: each card shows the `waiting` and `unblocks` fields; a card the server
  sends without them shows *unknown*, never an empty "nobody".
- A4.3: under a token with `answer` but not `operate`, the permission card's
  Allow is disabled with the required authority shown, and no `agent.decide`
  enters the outbox; the question card still answers.
- A4.4: setting a project's policy for *permission* to *list only* is refused
  by the server with a typed error. With default policy a non-blocking
  question counts on the badge; turning the project's toggle off removes it
  from the count and keeps it in the list. Setting the ranking preset to a
  name outside the closed set, or to free-form weights, is refused with a
  typed error.
- A4.5: opening and closing an escalation card leaves the server's reminder
  schedule unchanged (scripted server asserts no `work.escalation.ack`).
- A4.6: through a real isolated server and its `inbox` projection (the
  public fake model, not a scripted fake), testing what the GUI receives, not
  resolver internals: a structured delegate raises a `worktree_tool` prompt
  under a live delegation, and a run step raises a delegable
  `request-decision` question; each projected item carries the addressee the
  server stored at raise (ADR-0049 W5 "Addressee assignment"), unchanged
  across a server restart, appears as an evidence row and not in the
  operator inbox, and `needs_you` excludes it. A decide from an agent other
  than the stored addressee is refused `not_addressee` and the GUI shows that
  refusal. Then one run per reason in ADR-0049 W5's reason set, iterated from the
  generated type so a new reason adds a case: today that drives expiry,
  revocation, narrowing below the item's class or ceiling, orphaning, a
  generation change and a handoff that does not carry the delegate. In each
  run the server commits one `PermissionRerouted` with that reason; the same
  item, with the same id, arrives as an operator item showing it;
  the new `needs_you` count, one higher, arrives in the same frame sequence
  as the rerouted item; an ADR-0045 D8 escalation
  exists from the reroute's timestamp; the evidence row records the reroute;
  the former delegate's late answer is refused with `delegation_lapsed` and
  audited; and an `operate` decide succeeds with an audit record naming no
  delegation. A snapshot taken at any point of the transition contains the
  item exactly once. A client type test fails if the GUI's reason union and
  ADR-0049's reason set differ in either direction.
- A4.7: the grouping fixture: four items for one agent with the default cap
  show three cards and a "+1 more" row, the badge counts four, and every card
  headline begins with its effect.

### D5. Claim versus verified: two states, never merged

**Recommendation.** The GUI distinguishes a **claim** from **verified**
evidence in both data and visuals, following ADR-0045's record model: "a link
or a `DONE:` line is a claim until verified"; approval names the exact
acceptance revision and evidence set; old-head green CI cannot satisfy a
new-head check.

- **Data.** Work status `review_pending` is *claimed*; `done` with an
  approval record is *verified*. Each evidence descriptor carries its own
  verification state as the server sends it (verified, claimed, stale against
  the current head, unreadable). The client renders it; it never upgrades one.
- **Visuals.** A claim is an outlined mark with the label "Claimed"; verified
  is a filled check with "Verified" and the approver's identity. They differ
  in shape and text, not hue alone (ADR-0038 §2a's status-mark rule).
- **Evidence panel.** Part of every Work view: the acceptance revision; each
  evidence item (repo/PR/head, CI run and attempt and conclusion, receipt and
  content hash, check invocation and result hash) with its state; reviewer
  and judge rows (D3); and what is missing for approval.
- **Completion approval** (ADR-0045 D2). *Approve* is offered only to the
  attested supervisor or the operator, names the acceptance revision and the
  evidence set it approves, and is refused by the server if either changed.
  *Return* sends the Work back with a reason. Root completion stays
  operator-approved.

**Rejected.**
- *One "done" state with a confidence badge*: merges what ADR-0045 keeps
  apart, and a green CI on an old head would look finished.
- *Agent-reported checklist ticks as progress* (the legacy goal checklist):
  imported `done: true` is a legacy claim (ADR-0045 compatibility mapping)
  and is shown as a claim.

**Acceptance.**
- A5.1: a Work in `review_pending` with green CI at an old head shows
  "Claimed" and an evidence row marked *stale against head*; *Approve* is
  disabled with the reason.
- A5.2: approving with a stale acceptance revision returns the server's
  refusal and the panel re-renders the current revision, with no client-side
  success state at any point.
- A5.3: screenshot and accessibility tests: claimed and verified marks are
  distinguishable in grayscale and by accessible name.

### D6. Chat: the composer is the input queue; forks return proposals

**Recommendation.** The Agent view is a transcript plus one integrated
composer (send control inside the field, ⌘↩ to send).

- **The composer is the agent's input queue** (ADR-0036 §3). Messages sent
  while the agent is busy appear as queued items above the field, in server
  order, with producer and state. The person can edit (`queue.update` under an
  edit lease, `queue.edit`), reorder (`queue.move`), stash, delete, or send
  now (`queue.send_now`, with `interrupt` explicit). Order and delivery are
  shown only as the `queue/<agent>` stream reports them (ADR-0038 §3).
  Reordering is a menu or keyboard action that issues `queue.move`, not a drag
  (D2).
- **Forks start from a message boundary** (ADR-0036 §9, S12). The person
  picks a valid fork point from the transcript (the server's `fork_points`;
  invalid points are not offered), states a purpose, sees the capability
  ceiling (read-only by default, never above the parent's preset; ADR-0038
  §11 item 14, ADR-0036 §10b) and the `dry_run` cost preview (path, prefix
  size, warm or cold, expected cost) before confirming.
- **Fork results return as reviewable proposals** (S6e, `Proposal<T>` with
  `T` = Answer or Finding). The recipient reviews and may edit, then applies.
  **Merging never transfers authority:** applying a proposal is the
  recipient's own action under the recipient's own principal; the fork's
  permissions, grants and conversation state are not merged into the parent.
  A finding enters the parent's queue as a `finding` item (S13), visible and
  editable like any queued item.
- **Never chat as the control plane.** Changing a Work's acceptance,
  assignment or completion, a permission preset or a configuration value is a
  verb on that object's view, not a message asking an agent to do it.

**Rejected.**
- *A send-only composer with a separate queue panel*: the person cannot see
  that their message is fourth in line, which is the confusion ADR-0036 §3
  exists to remove.
- *"Merge back" as a conversation merge*: implies the fork's context and
  grants flow into the parent; S6e and the systems review reject it.
- *Forking only at "latest"*: loses ADR-0036 §9's main value, branching from
  the point before a wrong turn.

**Acceptance.**
- A6.1: with a working agent and two queued items, the composer shows both
  above the field in server order; `queue.move` via ⌥↑ reorders them only after
  the server's frame.
- A6.2: the fork sheet refuses to submit until purpose is set, shows the
  server's dry-run cost, and offers only server-listed fork points; a
  `fork_prefix_gone` point shows its typed reason.
- A6.3: applying a fork's proposal issues exactly one `question.answer`
  (or queues one `finding`) under the viewer's principal; the parent's
  permission preset is unchanged afterwards (scripted server asserts).

### D7. Workspace: subject-anchored workbench; terminals, raw events and telemetry stay out

**Recommendation.** The tile manager stays machinery under the screen
(ADR-0038 §2a). What changes is how the screen is arranged: by **subject**.

- **Opening a Work arranges its workspace.** The Work summary (acceptance,
  status, assignment, coverage, evidence panel) is the anchor. Beside it, by
  server relation: the PR diff (ADR-0038 §2's reserved `diff` kind), the
  Artifacts `about` this Work, open Questions `about` it, and the assigned
  agent's transcript. Relations come from the server; the arrangement is
  client layout and stays per device (ADR-0038 §11 item 9).
- **Opening an Agent** shows its conversation (D6) with its Work as a header
  link. **Opening a Question** shows the card with its subject beside it
  (`expand_into_second`).
- **Out of the workbench:** `terminal` tiles (structured agents replace
  terminal agents), raw event views and telemetry (`trace`, `logs`, `audit`;
  ADR-0038 §4a) are not tiles beside the work. **Placement (decided,
  OD-48.3):** a compact vitals strip in
  the inspector for the selected subject (its node's health and freshness,
  the agent's provider-limit and context state, as the server sends them); a
  collapsible bottom **Diagnostics drawer** holding `trace`, `logs`, `audit`
  and `node` views; and a link out to vigil's output (ADR-0038 §4a and owner
  decision 5) for history. There is no separate Diagnostics window and no
  Grafana dependency. The transcript
  shows turns and tool runs, never raw event type names; an unknown event
  shows ADR-0036 §8's generic card.
- **No free canvas.** No infinite canvas, no user-assembled dashboards, no
  pinboard of arbitrary tiles. Splits remain an advanced action.

**Amendments to ADR-0038 §2 and §2a (on acceptance):**

1. §2's tile kinds admitted in the product workbench are `transcript`,
   `artifact`, `queue`, `inbox`, `work`, `diff` and `context`. The
   goal-tree tile (ADR-0038 §2's `goal_tree`, and ADR-0045 D7's "goal-tree
   tile") is **renamed `work`**: one Work view, which also absorbs
   `work_graph` as a layout of the same view rather than a separate kind.
   `terminal` is removed; `trace`, `logs`, `audit` and `node` move to the
   Diagnostics drawer (OD-48.3).
2. §2a's rule "a post-MVP part is either hidden in the MVP, or shown disabled
   with the name of what it waits for" becomes: **in production builds a part
   that is not built is hidden.** A control is shown disabled only for a
   runtime reason the server reports (missing authority, stale connection,
   node unreachable), with that reason in plain language. No caption names an
   ADR, slice or issue. Unbuilt parts are visible only in development builds
   and a development component gallery.
3. §2a's agent header actions *Terminal*, *Diff* and *Fork* are removed from
   the header. Real actions move to a ⋯ overflow and context menu; *Diff*
   lives on the Work view; *Fork* lives on the transcript's message menu.
4. §2a's phone agent-screen segmented control drops *Terminal*, and its
   bottom tab bar drops *Agents*, leaving *Inbox* and *Activity* (D1).
5. §2a's top bar becomes one unified toolbar: no app-name title, connection
   status as a dot with a popover (disconnect inside it), search/palette, the
   D4 count, *New*. Sidebar and inspector are collapsible and resizable with
   saved widths.
6. §2a's desktop visual baseline follows the HIG-derived tokens that #2895
   establishes (13 px body, 28 px default controls, 20 px minimum; #2903);
   the phone keeps 15 px body text and 44 px targets.

**Rejected.**
- *Agent-anchored workspace* (today): the person assembles the context of one
  piece of work from three agents' transcripts by hand.
- *Free canvas or user dashboards*: every install becomes a different
  product, and tiles accumulate that no server relation explains (pre-mortem
  "canvas sprawl").
- *Keeping a terminal tile for structured agents*: contradicts the owner's
  direction and invites keystroke steering, which D1's principles reject.
- *Disabled deferred controls with captions in production* (ADR-0038 §2a as
  written): honest, but it reads as an unfinished product and puts ADR ids in
  user copy (UX review item 2).

**Acceptance.**
- A7.1: opening a Work with one PR, one Artifact and one open Question about
  it produces the Work tile plus those three, without the person placing
  them; a Question not `about` it is not opened.
- A7.2: a static check over the client's **static copy strings** only (the
  copy catalog and literal text in components, as bundled for production):
  none matches `ADR-\d{4}`, a slice id `\bS\d+[a-z]?\b` or an issue
  reference `#\d{3,}`. For D3, none matches, case-insensitively, `\bPTY\b` or
  `\bterminal[- ](agent|session|pane)s?\b`, and no copy template interpolates a
  count into a string matching those (a template such as `{n} terminal agents`
  fails). Other uses of "terminal" (for example a shell command shown in a tool
  run) do not trip it. Because a real count could only arrive as server data,
  A3.3 and a generated-types check that no wire field names PTY or terminal
  agents close that path. Server-sent data (Work titles, issue and PR numbers, labels, transcript
  text) is outside the check by construction, since D2 has the GUI render
  issue and PR numbers it receives (part of #2896).
- A7.3: the production build cannot open a `terminal` tile (the open
  operation refuses the kind; a unit test asserts it).
- A7.4: the transcript renders a fixture of every known `SessionEvent` kind
  without printing any raw event type name.

### D8. Configuration surface: resolved value, winning source, saved versus running, apply timing

**Recommendation.** The settings inspector shows, per key and per scope
(global, node, project, agent; ADR-0042 C2): the **resolved value**, the
**winning source** (ADR-0041 §1 provenance), the **saved** value and the
**running** value when they differ, and the key's **apply timing** (live,
next client attach, next agent launch, command restart, server restart;
ADR-0042 §4). A write goes through `config.set` under `operate` with an
expected revision (ADR-0042 §5) and reports `applied`, `restart_required` or
`rejected(cause)` per section (ADR-0041 §5).

- A restart-required key is never a live toggle: it shows "saved, applies on
  restart" until the running value changes (ADR-0041 §6).
- A file edit shadowed by env or a flag shows "saved but shadowed by
  `<source>`".
- Secrets are references only and shown redacted (ADR-0042 C5).
- Config is per node (ADR-0041 §6); the inspector names the node it edits and
  offers no fleet-wide write.
- Only keys in the typed registry appear. The GUI has no defaults of its own.

**Reject "everything configurable" as a principle.** A setting exists when a
supported policy needs a knob. Structure that flk enforces belongs to
workflows (ADR-0049), judgement belongs to skills and role text, and knobs
belong to configuration, with no overlap.

**Rejected.**
- *A settings form of toggles that apply on click*: reports restart-required
  and shadowed edits as active, which ADR-0041 §6 forbids.
- *Everything configurable*: multiplies untested combinations and lets
  configuration stand in for decisions the product should make.
- *GUI-local preferences for server behaviour*: two clients would disagree
  about what the server does.

**Acceptance.**
- A8.1: setting a restart-required key shows saved and running values side by
  side and no "applied" state until the server reports the running value.
- A8.2: an env-shadowed key shows the winning env source and refuses to
  present the file edit as effective.
- A8.3: a principal with `read` only sees the inspector read-only with the
  authority required; no `config.set` enters the outbox.

### D9. Views are typed queries over server-owned properties (OD-48.6)

**Decided (OD-48.6, 2026-11-03, added at acceptance).** The owner: all objects
"carry their tags, properties, .. and grouping/views/... can be done as 'sql queries'
style", with a default "per machine>repo>agent>subagents/delegates" but room for "vastly
different style", and "adding any property to a workspace/agent/.. should directly allow
to configure it new"; and "since the flk itself has its api/mcp .. all this can be driven
by the agent itself".

- **Typed property schema.** Every object the GUI shows (Project, Work, Agent, Question,
  Artifact, Run, and the node facet) exposes a server-owned, typed property schema, sent
  on the wire and generated into client types. Built-in properties include node, `repo`
  identity, role (the D3 server role field), parent edge, state or phase, timestamps
  (created, last activity, state since), harness and model. User **tags** are typed
  properties too. Properties are facts the server already owns or records (D2: the
  client invents no state); a property the server lacks for an object is *unknown*, and
  a view sorts or groups it as unknown, never as a default value.
- **A view is a declarative, typed query.** A view record holds filter predicates
  (typed comparisons over properties, combined with and, or, not), an ordered group-by
  path, sort keys per level (a lexicographic key, as OD-48.2 requires; no weights) and a
  per-level cap. It is SQL-like in expressive power but it is **not raw SQL** and never
  reaches a database engine. The sidebar tree, the home sections and the attention
  ranking (D4) are all views.
- **Validated, then evaluated by the server.** The server validates a view against the
  property schema when it is saved and again when the schema changes: an unknown
  property refuses `unknown_property`, an operator applied to the wrong type refuses
  `property_type_mismatch`, and a view beyond the server's bounds (group depth, number of
  predicates, cap) refuses with the bound named. The server (the home server for a
  multi-node fleet, ADR-0043) evaluates the view over its projections and streams the
  result as ordered groups of object ids with counts on a `view/<view_id>` stream, a
  proposed server object like the others (D2). The client renders that result and
  re-groups or re-sorts nothing, so two clients on one view agree.
- **Open to new properties.** Adding a property to the server schema makes it available
  for filter, group-by and sort in every view with no GUI code change: the view editor
  and the renderer read the schema, not a hard-coded list.
- **Presets are saved views.** Two defaults ship:
  - **(a) repo > architect > delegates**: group by `repo`, then by architect (the
    project's resident owner on the section header, segment architects as the first
    level), then delegates sorted by state, then by last activity. This is the D3
    sidebar, which becomes this preset rather than a fixed structure, and the default.
  - **(b) machines > admin agents, plus repos**: a combined view with two sections:
    nodes, each listing its admin agents (agents whose role property is `admin`, the
    operator's root agents that look after a machine rather than a repository; this
    value needs the D3 role field), followed by preset (a)'s repo sections.
  - Presets keep the macOS HIG's two disclosure levels below a section header as their
    default. User views may go deeper, up to the server's group-depth bound
    (recommended 4); the GUI then indents further rather than refusing.
  - The OD-48.2 attention presets are saved views over the attention items (filter: the
    six D4 kinds; sort: blocking, then parked behind, then age; per-level cap 3, the D4
    per-agent cap). Protected kinds (D4) cannot be filtered out of the attention queue
    or `needs_you`: a view may group or sort them, and items outside a view's filter
    appear in an *Outside this view* group, still counted.
- **Cross-node identity by property.** Grouping by `repo` puts two checkouts of one
  repository on different machines in one group, while each stays its own Project
  object (D2 "Project identity migration").
- **No PTY rows.** The view source is the structured-only object set (D3): no view,
  group, count or preset counts or advertises PTY or terminal agents, and the schema has
  no property that would select them.
- **Views, presets, tags and properties are server objects, driven through the flk
  API.** They are created, read, updated and deleted through the same flk API that the
  CLI and MCP expose (`flk view` and `flk tag` subcommands and their generated MCP
  adapters), never through GUI-only state. An agent can therefore tag objects, define or
  adjust views and switch the operator's active view.
- **Authority.** Reading views, presets, tags and the schema needs `read` plus
  visibility of the objects involved. Writing tags or views, or switching the active
  view, on the operator's behalf is operator authority: the operator does it under
  `operate`; an agent does it only under an operator-issued **view grant** (`views.write`
  for tags and views, `views.activate` for switching the active view), held as binding
  state in ADR-0049 W5's authority model: operator-issued, scoped to projects, revocable
  and audited, shown in `flk authority show`, never conferred by an edge, role or
  repository file. Without the grant an agent's write refuses `not_authorized`. Every
  agent-made change is a revisioned record attributed to the agent and its grant,
  appears to the operator as a notice naming the agent with *Revert*, and is revertible
  to any earlier revision. No view or tag change alters the attention queue's
  membership or `needs_you` (the protected-kinds rule above), so an agent can never
  silently change what the operator sees as needing them.

- **Scope.** D9 decides views over properties only: the sidebar, the home sections and
  the attention ranking. Generalising views to every pane is deferred to #2916 (see
  Deferred) and nothing here pre-empts its backend choice.

**Rejected.**
- *Raw SQL over a server database*: an injection and denial-of-service surface, and it
  ties the GUI to a storage schema ADR-0045 keeps private.
- *Client-side grouping and sorting over a fixed set of fields*: two clients disagree,
  and every new property needs GUI code.
- *GUI-local saved views*: invisible to the CLI, MCP and agents, and lost with the
  device.

**Acceptance.**
- A9.1: a fixture server adds a new property to the schema; with the GUI build
  unchanged, a view filtering, grouping and sorting by it is accepted and renders the
  expected groups in the expected order.
- A9.2: saving a view that references an unknown property refuses `unknown_property`;
  a type-mismatched predicate refuses `property_type_mismatch`; a view deeper than the
  server bound refuses with the bound named.
- A9.3: with default presets, the A3.1 fixture renders exactly A3.1's tree under preset
  (a), and preset (b) renders node sections with their admin agents followed by the
  repo sections.
- A9.4: two checkouts of one repository on two nodes form one `repo` group under
  preset (a) and remain two Project objects with two routes.
- A9.5: a view created through the flk API by MCP renders in the GUI with no client
  change; an agent without `views.write` is refused `not_authorized`; an agent with it
  creates a view that is audited, attributed to the agent, shown as a notice and
  reverted to the previous revision.
- A9.6: a view whose filter excludes a protected kind leaves `needs_you` unchanged and
  shows the excluded items in *Outside this view*.

## Consequences, risks and pre-mortem

**Consequences.**
- The GUI's home depends on server projections that do not exist yet: the
  `inbox` stream (G5, #1597) with rank, `waiting`, `unblocks`, `addressee`
  and `needs_you`, the `work` stream (G8, #1600), `queue/<agent>`,
  `artifacts` and the run stream. These are proposed server objects; today's
  dispatcher refuses every stream but `fleet` and `agent/*`. GUI-P0 ships what
  the `fleet` stream supports, with the provisional badge (D1).
- Project rows gain a registered `project_id` beside today's per-node
  checkout key, through a snapshot reset with a committed mapping (D2).
- The inbox projects ADR-0049 W5's delegated items and `PermissionRerouted`
  records (D4) and defines no reroute of its own, so delegated items wait for
  ADR-0049 P2's resolver and delegation verbs.
- The `fleet` contract gains a structured-only guarantee (D3), a server-side
  change.
- ADR-0038 loses three sidebar trees, the `terminal` tile and the
  deferred-caption rule; the wireframes (`0038-gui/wireframes.html`) need a
  successor drawn from this ADR.
- The S6e inbox ordering gains a *parked behind* term, which the server must
  compute from edges and registered waits.
- Ephemeral agents disappear from navigation; finding a reviewer's transcript
  goes through the Work it reviewed.

**Risks and mitigations.**
- **Chat-everything.** Every feature becomes "ask the agent in chat". Mitigated
  by D6's rule that object changes are verbs on the object's view, and A6.3.
- **Canvas and dashboard sprawl.** The tile manager grows into a canvas, and
  tiles appear that no relation explains. Mitigated by D7's fixed tile kinds
  and subject-anchored arrangement.
- **Object-model drift.** Attractive cards invent state the server cannot
  restore. Mitigated by D2's "client invents no state" with A2.2's readonly
  types and reducer-only writes as a compile-time gate, and by refusing
  mutations on projection-phase rows (A2.5).
- **Definition sprawl.** Agent types, workflows, skills and config overlap.
  Mitigated by D8's one-line split, settled in ADR-0049.
- **Silent calm.** The home looks quiet while supervision is degraded.
  Mitigated by the supervision-gap card (D4) and *unknown* rendering (D2).
- **Stranded delegated prompt.** A delegate's authority ends while a prompt
  waits on it, and nobody sees it. Mitigated by ADR-0049 W5's reroute, which
  the inbox projects (D4), tested by A4.6.

**Pre-mortem: it is 2027 and the GUI failed. Why?**
1. *The work stream never shipped*, so the home stayed agent-first and D1 was
   never true. Guard: GUI-P1 starts with the read-only projection (#1600), which
   needs no new storage.
2. *Cards showed state the server could not restore after a restart.* Guard:
   A2.2 and the fake-server restart tests (G3).
3. *A permission was approved by a principal without `operate` or a live
   matching delegation* because the inbox merged question and permission
   handlers, or trusted a lapsed delegation. Guard: A4.3, A4.6 and ADR-0049
   W5's fence recheck; the handlers stay separate.
4. *The operator missed a red landing for hours* because the GUI was closed.
   Guard: D4 does not claim the guarantee; ADR-0045 D8's endpoint and
   reminders do.
5. *Forks quietly widened authority* through "merge back". Guard: D6's
   proposal rule and A6.3.
6. *A repository silenced its own permission prompts* through project policy.
   Guard: protected kinds and the operator-owned scope (D4, A4.4).

## Phased plan

| Phase | Scope | Issues | Decisions delivered |
| --- | --- | --- | --- |
| **GUI-P0** (now) | Design foundation: tokens, strict values, typed layout primitives; DOM-wide HIG conformance suite; exemplar baseline. Quick wins: hide deferred controls and drop *Terminal* and the phone *Agents* tab (the §2a amendments, item 2–4), unified toolbar with connection popover, plain-language refusal copy, desktop density with integrated composer and icon set, keyboard model, provisional needs-you badge in toolbar/title/sidebar (`waiting_on_human` until G5; D1), resizable and collapsible sidebar and inspector. Remove the phantom agent path; empty state. Server: structured-only fleet counts. | #2895, #2896, #2899, #2900, #2901, #2902, #2903, #2904, #2905, #2906, #2871, #2872; the structured-only fleet guarantee is to be filed on acceptance | D1 band 1–2 (fleet-backed), D3 (structured-only, node badge), D7 amendments 2–6, A1.3-GUI-P0, A1.4, A2.4, A3.3, A7.2–A7.4 |
| **GUI-P1** | Work stream home: the read-only projection of ADR-0045 first (G8, #1600), then records (#1603) with the work verbs. Typed inbox stream with rank, `waiting`, `unblocks` and the six kinds (G5, #1597). Evidence panel and completion approval. Artifacts on Work (G6/G7, `about` → Work). Forks as proposals (S12/S13, G14/G15, #2726 progress). Composer as queue (`queue/<agent>` stream). Context inspector. Project "what reaches me" policy (ranking preset, cap, non-blocking toggle). Registered `project_id` on project rows. Delegation tree from edges; the server role field on delegation rows (D3 prerequisite, to be filed on acceptance). Property schema, server-evaluated views and the default presets through the flk API, CLI and MCP (D9). | #1600, #1603, #1597, #2726; G6, G7, G8, G14, G15 (ADR-0038 §10) | D1 band 3 and A1.3, D2, D3 tree, D4 except delegated items (A4.1–A4.5, A4.7), D5, D6, D7 subject arrangement, D9 |
| **GUI-P2** | Delegated permission and `request-decision` items and the `PermissionRerouted` projection, after ADR-0049 P2 ships the W5 resolver and delegation verbs. Declarative workflows, agent types and the Run view (ADR-0049); configuration inspector writes; multi-node GUI through the home server (ADR-0043, #2293); mobile with remote push (#2733, G10). | ADR-0049 (proposed, #2908), #2293, #2733 | D4 delegated items and A4.6, D2 `Run` (projection ids from ADR-0049 P1, `RunId`s from ADR-0049 P2) and the workflow-definition facet, D8 writes |

The phases are named GUI-P0 to GUI-P2 to keep them apart from ADR-0049's P0 to P3 and
from ADR-0045 D7's projection and record phases; a cross-reference always names its ADR.
Each GUI-P1 and GUI-P2 issue carries the acceptance tests of the decisions it
delivers, written to fail first.

## Deferred (named here, not decided)

- **Waves and the landing queue as Work views.** An architect's waves and the
  landing order are views over Work and ADR-0049 runs (a `land` step's queue),
  not new objects. They wait for the work stream and ADR-0049's `land` runtime;
  until then they are not drawn. Reordering stays a verb (D2).
- **The fork branch rail.** A rail beside the transcript showing an Agent's
  fork lineage (ADR-0036 §9) as branches from message boundaries. It waits for
  #2726's fork progress and the lineage read; D6's fork sheet does not depend
  on it.
- **Panels as configuration.** The owner's direction is to generalise D9's views to every
  pane: a typed live query, rendered through a closed block vocabulary built from the
  #2895 primitives, placed in a layout slot, evaluated in the Rust server and pushed over
  the subscription wire. That generalisation is specified separately in the spike and ADR
  #2916 (planned ADR-0050). This ADR decides only views over properties (D9, OD-48.6) and
  does not pre-empt #2916's backend choice.

## Owner decisions (2026-11-03)

The owner decided six questions, each recorded where it applies, and accepted the ADR the
same day; OD-48.6 was added at acceptance. The `OD-48.n` ids keep them apart from ADR-0049's
owner questions.

1. **OD-48.1.** **Architects may answer structured delegates' permission prompts, only
   through ADR-0049's `DecideDelegation`** (operator-issued, scoped,
   revocable, audited). D4 "Permission authority", "addressee" and
   "Authority loss"; A4.6.
2. **OD-48.2.** **Attention ranking is configurable as a typed per-project policy with
   named presets**; the default preset is lexicographic: blocking >
   parked-behind > age. No free-form weights. D4 "Ranking"; A4.4.
3. **OD-48.3.** **Telemetry placement:** an inspector vitals strip, a collapsible
   bottom Diagnostics drawer, and a link out to vigil's output (ADR-0038 §4a and owner
   decision 5). No separate window and no Grafana dependency. D7.
4. **OD-48.4.** **Non-blocking questions count on the badge by default**, with a
   per-project toggle. D4; A4.4.
5. **OD-48.5.** **No node tree; the sidebar is project → delegation.** D2 rejected list,
   D3. Refined by OD-48.6: this is the default view, not a fixed structure.
6. **OD-48.6.** **Views are typed queries over server-owned properties**, and views,
   presets, tags and properties are server objects driven through the flk API, CLI and
   MCP, so an agent can drive them under the authority D9 names. Defaults: repo >
   architect > delegates by state then time; machines > admin agents, plus repos.
   Cross-node grouping is by the `repo` property. D9; D2 "Project identity
   migration"; D3; D4. Generalising views to every pane is deferred to #2916 (Deferred,
   "Panels as configuration").

## Open questions for the owner

1. **View query syntax (D9).** The stored and wire form is the typed structured view
   record. Should the CLI and MCP also accept a textual DSL that compiles to it, or only
   the structured form?
2. **Where saved views live (D9).** Views are server objects; at which scopes (operator
   global, node, project) are they stored, and which scope wins when two define the
   active view?
3. **Sharing views (D9).** May a saved view be shared with other principals or exported
   with a project, and if so under which authority?
