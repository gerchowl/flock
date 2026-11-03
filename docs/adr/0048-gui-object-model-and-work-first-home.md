# ADR-0048: The GUI is mission control for delegated work: object model and work-first home

- Status: Proposed
- Date: 2026-11-03
- Issues: P0 [#2895](https://github.com/gerchowl/flock/issues/2895),
  [#2896](https://github.com/gerchowl/flock/issues/2896),
  [#2899](https://github.com/gerchowl/flock/issues/2899),
  [#2900](https://github.com/gerchowl/flock/issues/2900)–[#2906](https://github.com/gerchowl/flock/issues/2906),
  [#2871](https://github.com/gerchowl/flock/issues/2871),
  [#2872](https://github.com/gerchowl/flock/issues/2872); P1 depends on
  [#1597](https://github.com/gerchowl/flock/issues/1597) (G5 inbox stream),
  [#1600](https://github.com/gerchowl/flock/issues/1600) (work projection),
  [#1603](https://github.com/gerchowl/flock/issues/1603) (work records),
  [#2726](https://github.com/gerchowl/flock/issues/2726) (fork progress); P2
  depends on [#2293](https://github.com/gerchowl/flock/issues/2293) (multi-node)
  and [#2733](https://github.com/gerchowl/flock/issues/2733) (mobile delivery).
- Decision owner: repository owner. The owner asked for this ADR on 2026-11-03
  ("go for all"), after two independent GUI-vision reviews (a product review
  and a systems review, 2026-11-02) agreed on its thesis. D1–D8 are
  recommendations awaiting the owner's decision.
- Builds on: ADR-0027 §1–§2 (delegation edges, `delegate.list`), ADR-0028 §1
  and §8a (state axes, open questions), ADR-0030 §1–§3 and ADR-0031 (goal
  records), ADR-0032 §1 (five context layers), ADR-0033 §1 and §3 (AX forks),
  ADR-0034 §2 and §6 (question policy, permission routing), ADR-0035 §1a
  (owner → segment architect → builder), ADR-0036 §2a, §3, §9, §10b, S6e and
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
- Forward reference: ADR-0049 (workflow definitions, agent types and the Run
  runtime) is not yet written. This ADR names `Run` as an object and reserves
  its place; it decides nothing about its runtime.
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
| The wire serves two stream families: `fleet` and `agent/<id>`. There is no `inbox`, `work`, `queue/<agent>` or `artifacts` stream on the GUI wire. | `src/web/wire/subscriptions.rs:114-117` |
| The fleet-lite row and counts cover local structured agents only; `FleetAgentCounts.total` is the sum of disjoint phase buckets. | `src/structured/fleet_lite/wire.rs:100-101` |
| The palette can open an agent by id and the composer stays open for an agent missing from the list. | `web/client/src/workbench/Palette.tsx`, `web/client/src/workbench/Composer.tsx` (#2871) |
| Deferred parts render as captions that name ADR slices. | `web/client/src/workbench/Workbench.tsx:138` (`waitsFor="the harness picker (ADR-0036 S17)"`), `web/client/src/workbench/parts.tsx` |
| Permission decisions reuse a typed refusal map in one place only. | `web/client/src/workbench/decide.tsx` |
| Work records (ADR-0045 D1), completion proposal/approval (D2), work verbs (D7) and escalation (D8) are accepted design, not code. | ADR-0045 "Implemented: none" |
| Question records with blocking reasons, threads and proposals are accepted design (S6e), not an endpoint. | ADR-0036 S6e "design only, post-MVP" |

## Decision

Eight decisions follow. Each states the recommendation, the rejected
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
keeps ADR-0038 §2a's inbox-first layout; its *Inbox* tab is band 1 and an
*Activity*-equivalent tab carries bands 2 and 3.

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

### D2. Object model: six navigable objects, the rest are facets; the client invents no state

**Recommendation.** Six kinds are first-class: each has a stable server id,
a route, a detail view and a row in search (⌘K).

| Object | Identity and source of truth | Relations (all server-owned edges) |
| --- | --- | --- |
| **Project** | Registered repository identity (ADR-0042 C2 project scope; ADR-0043 §5 `FleetProjectRow`) | contains → Work; owned_by → the resident owner Agent (ADR-0035); spans nodes |
| **Work** | `WorkId` (ADR-0045 D1). A **goal is the root Work** of a DAG, not a separate entity. Phase 1 rows carry projection view ids (ADR-0045 D7), which are not mutation targets | parent/child and blocks (DAG); assigned_to → Agent (assignment generation); supervised_by → Agent; evidence → typed descriptors (D5); about ← Question, Artifact; implements → issue; delivered_by → PR |
| **Agent** | `AgentId` (ADR-0008); structured only (D3) | delegated_by → Agent (ADR-0027 edge, carrying role); works_on → Work; forked_from → Agent (lineage, ADR-0036 §9) |
| **Question** | `question_id` + revision (ADR-0036 S6e `QuestionRecord`) | asked_by → Agent; addressed_to → Agent or operator; about → Work or Artifact; threads → fork Agents; proposals |
| **Artifact** | `artifact_id` + version (ADR-0038 §7) | published_by → Agent; about → Work; annotated_by → comments, Questions |
| **Run** | Workflow instance id, defined by ADR-0049 (forward) | instantiates → workflow definition (pinned hash); drives → Work. Not rendered until ADR-0049 is accepted and its stream exists |

Everything else is an **attribute or facet** of one of those, shown inside
its owner's view and never as a top-level navigation root:

| Facet | Of | Shown as |
| --- | --- | --- |
| Node | Agent, Project | a badge (name, health dot, freshness) with a popover; ADR-0043 §5 classifications rendered as sent |
| Role and tier | the delegation edge | an edge label: owner, segment architect, implementer, reviewer, judge (ADR-0034 §4: the role comes from the server) |
| Transcript and input queue | Agent | the Agent's conversation view (D6) |
| Evidence | Work | the evidence panel (D5) |
| Rules and context | Project, Agent | a context inspector with ADR-0032 §1's five layers and each layer's provenance |
| Configuration | Node, Project, Agent | the settings inspector (D8) |
| Permission prompt | Agent | an attention card (D4) and the Agent's state; not an object of its own |
| Notification | any | a toast or attention card referring to an object; ADR-0016's log is not navigable as a list of truth |

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
  "one Node→agents tree"): nodes are where work runs, not what the person is
  supervising. A node outage still surfaces, as a supervision-gap card (D4)
  and as stale badges.
- *Client-side optimistic work state* (move a card, reconcile later): the
  first restart shows a board the server cannot restore. This is the
  "object-model drift" failure in the pre-mortem.
- *Workflow definitions as objects now*: they need ADR-0049's runtime;
  drawing them earlier produces diagrams with no executor.

**Acceptance.**
- A2.1: a type-level test: the client's entity store has exactly the six
  object kinds plus facet records; adding a seventh kind fails a
  generated-types drift test (ADR-0036 §6).
- A2.2: a static check fails if any component computes `status`, `rank`,
  `blocking` or `phase` from other fields (a lint rule over the store's
  selectors, mirroring ADR-0038 §3's "never derive" list).
- A2.3: the fake server sends a Work without `coverage`; the Work view shows
  *unknown*, not *armed*.
- A2.4: no element in the production build carries a drag handler that
  issues a `cmd` (Playwright scan of `draggable` and pointer-drag listeners
  against the outbox).

### D3. Sidebar: project → delegation tree, structured agents only, node as a badge

**Recommendation.** The sidebar is **one tree per Project**: the project's
resident owner, its segment architects, their implementers (ADR-0035 §1a;
ADR-0014 §5's two working levels), built from `delegate.list` edges
(ADR-0027 §2) through the `fleet` and (later) `work` streams. Each row shows
the agent's name, the Work it is assigned to, its phase mark, its node badge,
and its count of open attention items. Above the trees sits a *Needs you* row
with the D4 count. Unattached root agents (started by the operator with no
Project binding) group under an *Unassigned* row.

- **Ephemeral agents are not sidebar rows.** Reviewers, falsifiers, AX forks
  (ADR-0033 §3: in place, reaped on report), System One judges (ADR-0030 §3)
  and question-thread forks (ADR-0036 S6e) appear as **evidence rows on the
  Work** or as thread chips on the Question they serve, with a link to their
  retained transcript. This amends ADR-0038 §11 item 13's consequence: ended
  threads fold into the Question's history, not the sidebar.
- **Node is a badge, not a group.** A toolbar health dot opens a fleet
  popover listing nodes with ADR-0043 §5's server classifications. A scope
  control filters the tree to this node or all nodes.
- **Never PTY agents.** The GUI shows structured agents only (owner
  direction, 2026-11-02). The filter is the server's: fleet rows, counts and
  `FleetAgentCounts.total` must be structured-only by contract, so no client
  filter is needed and no PTY agent leaks into a count. PTY agents remain the
  TUI's.

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
  renders a three-level tree; an architect's builders are its children, not
  siblings.
- A3.2: an in-place AX fork and a thread fork in the fixture produce zero
  sidebar rows and one evidence row (or thread chip) each.
- A3.3: a server-side test: with one PTY and one structured agent running,
  the `fleet` snapshot contains one agent row and `total == 1`.
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
| Supervision gap | ADR-0045 coverage `unarmed`/`degraded`, delivery-degraded endpoint, unreachable owner node | open the Work or node; the remedy the server offers |

Non-blocking questions addressed to the operator are listed below the
blocking items, as ADR-0036 S6e orders them, and do not count toward the
toolbar badge unless the project policy says so (below).

- **Ranking is server-side.** Order is a lexicographic key: blocking items
  first; then the number of agents parked behind the item (the asker plus
  every agent whose registered wait, ADR-0028 §5, or delegation edge resolves
  to it); then age. This refines ADR-0038 §1 and S6e's "blocking, urgency,
  age" by inserting *parked behind*; the server computes it and ships it as a
  field. The client never re-ranks.
- **Every card says who waits and what answering unblocks.** The server
  sends `waiting: [AgentRef]` and `unblocks: [WorkRef]` with the item, plus
  the asker and recipient as attested identities, separate from the untrusted
  body (S6e "GUI, TUI and CLI contracts").
- **Per-project "what reaches me" policy.** An operator-owned setting keyed
  by project id (ADR-0042 C2 scope, stored outside the checkout, never in a
  committed `.flk.toml`): for each kind, *badge and notify*, *badge only* or
  *list only*. It defaults the question policy of new root agents in that
  project (ADR-0034 §2) and the D8 notification level. Protected kinds cannot
  be lowered below *badge and notify*: permissions, blocking questions,
  operator-stage escalations and root completion approvals. Repository content
  cannot change what reaches the operator.
- **Permission authority is unchanged.** A permission card calls
  `agent.decide` and works only under an `operate`-scoped principal (ADR-0036
  §10b). A question card calls `question.answer` under `answer` or `operate`
  (S6e "Authorization and proposals"). The two handlers stay separate even when
  rendered side by side; a principal without the authority sees the action
  disabled with the authority it needs. The GUI shows only items addressed to
  the operator: a delegate's permission that ADR-0034 §6 routes to its
  architect is an evidence row on the Work, not a card.
- **The inbox is not the escalation guarantee.** It projects ADR-0045 D8's
  attention record. Reading or dismissing a card does not acknowledge an
  escalation or suppress reminders; only `work.escalation.ack` does, and only
  for a bounded lease.

**Rejected.**
- *Separate inboxes per kind* (permissions here, questions there, failures in
  a log): the person polls three places, and none can rank against the others.
- *Client-side ranking or a weighted score in the browser*: two clients would
  disagree, and the client would derive state ADR-0038 §3 forbids.
- *A weighted product (blocking × parked × age) as one opaque score*: harder
  to explain on the card and to test; the lexicographic key keeps the same
  intent with a stated order. (Open question 2.)
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
  by the server with a typed error; setting *non-blocking question* to *list
  only* removes it from the badge count.
- A4.5: opening and closing an escalation card leaves the server's reminder
  schedule unchanged (scripted server asserts no `work.escalation.ack`).

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
  ADR-0038 §4a). Telemetry remains reachable from a separate Diagnostics
  window or route for operators, not as tiles beside the work. The transcript
  shows turns and tool runs, never raw event type names; an unknown event
  shows ADR-0036 §8's generic card.
- **No free canvas.** No infinite canvas, no user-assembled dashboards, no
  pinboard of arbitrary tiles. Splits remain an advanced action.

**Amendments to ADR-0038 §2 and §2a (on acceptance):**

1. §2's tile kinds admitted in the product workbench are `transcript`,
   `artifact`, `queue`, `inbox`, `work` (the Work view, replacing
   `goal_tree`/`work_graph` as separate kinds), `diff` and `context`.
   `terminal` is removed; `trace`, `logs`, `audit` and `node` move to
   Diagnostics.
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
4. §2a's phone segmented control drops *Terminal*.
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
- A7.2: the production bundle contains no element whose text matches
  `ADR-\d{4}`, `S\d+[a-z]?\b` slice ids or `#\d{3,}` in user copy (DOM-wide
  Playwright scan, part of #2896).
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

## Consequences, risks and pre-mortem

**Consequences.**
- The GUI's home depends on server projections that do not exist yet: the
  `inbox` stream (G5, #1597) with rank, `waiting` and `unblocks`, and the
  `work` stream (G8, #1600). P0 ships what the `fleet` stream supports.
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
  restore. Mitigated by D2's "client invents no state" with A2.2 as a lint
  gate, and by refusing mutations on phase-1 projection rows.
- **Definition sprawl.** Agent types, workflows, skills and config overlap.
  Mitigated by D8's one-line split, settled in ADR-0049.
- **Silent calm.** The home looks quiet while supervision is degraded.
  Mitigated by the supervision-gap card (D4) and *unknown* rendering (D2).

**Pre-mortem: it is 2027 and the GUI failed. Why?**
1. *The work stream never shipped*, so the home stayed agent-first and D1 was
   never true. Guard: P1 starts with the read-only projection (#1600), which
   needs no new storage.
2. *Cards showed state the server could not restore after a restart.* Guard:
   A2.2 and the fake-server restart tests (G3).
3. *A permission was approved by a principal without `operate`* because the
   inbox merged question and permission handlers. Guard: A4.3; the handlers
   stay separate.
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
| **P0** (now) | Design foundation: tokens, strict values, typed layout primitives; DOM-wide HIG conformance suite; exemplar baseline. Quick wins: hide deferred controls and drop *Terminal* (the §2a amendments, item 2–4), unified toolbar with connection popover, plain-language refusal copy, desktop density with integrated composer and icon set, keyboard model, needs-you count in toolbar/title/sidebar (from fleet phases until G5), resizable and collapsible sidebar and inspector. Remove the phantom agent path; empty state. Server: structured-only fleet counts. | #2895, #2896, #2899, #2900, #2901, #2902, #2903, #2904, #2905, #2906, #2871, #2872; the structured-only fleet guarantee is to be filed on acceptance | D1 band 1–2 (fleet-backed), D3 (structured-only, node badge), D7 amendments 2–6, A1.3, A3.3, A7.2–A7.4 |
| **P1** | Work stream home: the read-only projection of ADR-0045 first (G8, #1600), then records (#1603) with the work verbs. Typed inbox stream with rank, `waiting`, `unblocks` and the six kinds (G5, #1597). Evidence panel and completion approval. Artifacts on Work (G6/G7, `about` → Work). Forks as proposals (S12/S13, G14/G15, #2726 progress). Composer as queue (`queue/<agent>` stream). Context inspector. Project "what reaches me" policy. Delegation tree from edges. | #1600, #1603, #1597, #2726; G6, G7, G8, G14, G15 (ADR-0038 §10) | D1 band 3, D2, D3 tree, D4, D5, D6, D7 subject arrangement |
| **P2** | Declarative workflows, agent types and the Run view (ADR-0049); configuration inspector writes; multi-node GUI through the home server (ADR-0043, #2293); mobile with remote push (#2733, G10). | ADR-0049 (to be written), #2293, #2733 | D2 `Run`, D8 writes |

Each P1 and P2 issue carries the acceptance tests of the decisions it
delivers, written to fail first.

## Open questions for the owner

1. **Permission routing for structured delegates.** ADR-0034 §6 lets an
   architect answer a delegate's in-worktree permission; ADR-0036 §10b admits
   `agent.decide` only from an `operate`-scoped principal. This ADR keeps
   §10b for everything the GUI shows and treats architect-routed decisions as
   evidence rows. Should a follow-up reconcile the two for structured
   delegates (an attested-architect decision path), or must every structured
   permission reach the operator?
2. **Ranking.** Accept the lexicographic key (blocking, parked behind, age),
   or do you want a weighted score?
3. **Diagnostics.** Telemetry and raw events in a separate GUI window or route
   (recommended), or TUI and CLI only?
4. **Policy default.** Should non-blocking questions count toward the badge
   by default (*badge and notify*) or start as *list only*?
5. **Nodes.** Confirm dropping a node → agents tree (the UX review's
   suggestion) in favour of node badges and a fleet popover.
