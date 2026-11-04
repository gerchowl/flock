# ADR-0050: A keyboard-first GUI: one command model, configurable bindings, hints and smart next

- Status: Proposed
- Date: 2026-11-04
- Issues: [#2978](https://github.com/gerchowl/flock/issues/2978);
  [#2904](https://github.com/gerchowl/flock/issues/2904),
  [#2972](https://github.com/gerchowl/flock/pull/2972),
  [#2967](https://github.com/gerchowl/flock/issues/2967).
- Decision owner: repository owner. Acceptance belongs to the owner; the
  approved platform defaults in #2978 are recorded below, not acceptance of
  this ADR or answers to its owner questions.
- Builds on: ADR-0038 §6 (one bundle and shell configuration), ADR-0041
  §1, §3–§6 and ADR-0042 §3–§5 (typed scoped configuration), and #2972
  (shortcut registry, modal layers, IME guard and runtime key guard).
- Amends, on acceptance: ADR-0048's GUI-P0 keyboard model and D7 navigation
  affordances. D4's server-owned attention membership and ranking remain
  authoritative; smart next ranks navigation candidates, not attention cards.
  Accepted ADRs are immutable; the amendment lives here.
- Implemented: none of this proposal. Inspected `origin/dev` at `ec911e781`;
  #2972 was still open on 2026-11-04, despite the issue describing it as
  landed. Its inspected head was `cf2d081b9024512f70da201aee92bc0fe35b1cbf`.
  The keyboard foundation is an implementation dependency, not shipped code
  claimed by this ADR.

## Context

The owner requires every GUI action and navigation target to be reachable
from the keyboard. Adding a shortcut to each component would leave several
inventories: click handlers, a palette, a help sheet and hints. They would
inevitably disagree about what exists and what can run.

At the inspected #2972 head, `web/client/src/workbench/keys.ts` owns
`SHORTCUTS`, `bindKeys`, `pushLayer` and the single branded document
listener. Its scopes distinguish text editing, focused controls and modal
layers. `web/client/e2e/key-guard.ts` rejects other listeners, including
aliases, delegated JSX handlers and listeners in frames. `web/DESIGN.md`
combines that runtime authority with the early `design/keys` lint. The
registry currently describes shortcuts, and only some are palette commands;
commands without shortcuts and controls without commands are not covered.

ADR-0048 makes Work, Agent, Question, Artifact, Project and Run the GUI's
subjects. Keyboard access must use those same objects and the same server
verbs. It must preserve native editing, browser standards, IME composition,
authorization, and the deferred-part rule: an unbuilt feature gets neither
a control nor a runnable command.

## Decisions

### 1. Commands own actions; controls and shortcuts are projections

Extend the keyboard foundation rather than install another listener.
Declare typed command descriptors in
`web/client/src/workbench/commands.ts`, with feature declarations imported
there as the inventory grows. `keys.ts` remains the sole keyboard dispatcher.
A descriptor has:

| Field | Contract |
| --- | --- |
| `id` | Stable semantic identifier, such as `agent.open` or `composer.send`; never a translated label, DOM selector or list index. |
| `label` | Human action label; a target instance adds its accessible target name. |
| `scope` | `global`, named `region`, or `focused-object`, plus explicit text-field eligibility and layer ownership. This compiles to the existing keyboard scopes; it does not bypass them. |
| `availability(context, target)` | Pure result: available, or unavailable with a human reason. Uses server capabilities and current projections; invocation rechecks it. |
| `defaults` | Optional ordered key sequences for each shell/platform profile. A command need not have a default binding. |
| `execute(context, target, intent)` | The shared semantic action path for pointer, palette, binding and hint activation, including decision 2's native adapters. Existing server authorization and confirmations still apply. |

A target instance is a descriptor plus validated object identity and its
current owning region/layer. For example, every rendered agent row uses
`agent.open` with a distinct AgentRef. Virtualized rows do not register new
command definitions. Native fields use focus/edit commands; ordinary typing,
selection and editing stay native rather than becoming one command per key.
A navigation command focuses or opens an object; it never silently performs
an approval or another destructive action.

The palette lists every command applicable to built surfaces, including
unbound commands and unavailable actions with their reason. Parameterized
commands expose a searchable target picker over the same authoritative
projection, so unmounted rows remain keyboard reachable. Layer-only commands
are listed in their layer context. Internal Tab/arrow mechanics remain
registry entries, grouped as widget navigation rather than thousands of
palette rows. The `?` sheet reads resolved bindings and scopes from this same
inventory. Which-key reads its sequence trie; link hints read its mounted
target instances. No surface keeps a separate action list.

Click and keyboard both resolve the active layer, target identity and
availability through the same dispatcher. Modal layers are exclusive. A
stale target refuses with a visible explanation; the dispatcher does not
retarget an action to the newly selected row. The existing IME guard
(`isComposing` and Safari's 229 signal) runs before all command matching.

### 2. Conformance detects controls independently of their registration

Extend the rendered conformance walk, not just a lint over command props.
The census uses element semantics and actual actions, independently of command
annotations. An interactive control is a native button, anchor with `href`,
non-hidden input, select, textarea, summary/disclosure, editable element, or
an element with an interactive ARIA role (menu item, tab, tree item, option,
slider or composite widget). Disabled controls count. A nonsemantic element
whose handler performs activation, navigation or a value change also counts
and separately fails the existing semantic/name checks. Focusability alone
is not evidence of an action.

Explicit exclusions from product command coverage are:

- Focusable scroll containers and focus sentinels that only support scrolling
  or focus management, without an activation/value-change action. If a scroll
  container also opens an object, that action still needs a command.
- Delegation roots whose handlers only route descendant events, and handlers
  for hover, measurement, pointer capture or drag tracking without an action.
  The actual descendant action and any drag result need commands; routing
  through a root never exempts them.
- Hidden, inert or unbuilt controls. Hidden/inert controls have no active
  instance; unbuilt features have no palette entries. Disabled visible
  controls remain in scope, and opening a hidden menu brings its controls
  into scope.
- Gallery-only demonstrations identified by the gallery build's fixture
  manifest. They still run native-widget, keyboard, name and other DESIGN
  checks, and adapter demonstrations use a fixture registry. No gallery
  exception or fixture command may enter the production bundle.

Source lint classifies action handlers versus routing/interaction handlers;
test-time event instrumentation follows indirect and delegated registrations
to the element whose semantic action they perform. The census reports an
unclassified handler as a finding, rather than assuming every pointer
listener is an action or ignoring nonsemantic actions. Neither lint nor
runtime census may derive its candidates from command annotations.

Every in-scope control maps to a descriptor, a valid target instance and a
typed adapter mode: `application`, `native-link`, `native-field` or
`native-submit`. Controls with several actions declare each intent. The
adapter resolves layer, identity and availability before activation; a
`data-command` attribute alone proves nothing. Native modes share semantic
command validation and outcomes, not identical synthetic event traces:

| Adapter | Native behavior and command contract |
| --- | --- |
| Application action | Pointer, palette, binding and hint invoke the same semantic command/target once. Native button Enter/Space activation uses its click path; the keyboard layer must not also execute it. |
| Native link | Descriptor includes the actual `href`, `target`, `rel` and download intent. Pointer activation and native Enter validate then allow the browser's default navigation, without cancelling or replacing it. Meta/Ctrl-click, Shift-click, middle-click and context-menu opening retain native disposition, target and security behavior; instrumentation observes them without turning them into ordinary `agent.open` calls. A link that must be unavailable has no navigable `href`, with a reason and disabled semantics. |
| Native field | Command focuses/edits the named field; browser typing, selection, IME, checkbox/radio toggles, select changes, validation and input/change events stay native. Adapters observe committed semantic changes; they do not intercept text keys or recreate input events. Application side effects consume that committed change once. |
| Native submit | Descriptor names the form and submitter. Native button/Enter submission reaches one submit adapter after browser constraint validation. Palette/binding/hint uses `requestSubmit(submitter)` through that same validation and submit path, never `form.submit()` or a second direct mutation. The submit adapter checks current command availability before the application effect. |

Palette/hint/link commands use the registered anchor's native activation for
ordinary navigation, preserving its attributes and browser security policy;
they do not fabricate a trusted modified event or promise a new window the
browser refuses. Native modified pointer activation is outside the hint
label input protocol. Browser context menus remain native. Navigable links
must therefore represent destinations safe to open natively, with no hidden
application mutation attached to navigation. Use an application button for
such a mutation. Plain, modified and auxiliary clicks cannot double-execute
an application side effect.

This adds command coverage to DESIGN conformance rules 8 (keyboard/name)
and 9 (toolbar/palette parity), without replacing focus, accessible name,
size, contrast or native-widget checks. Exercise disabled toolbar actions,
popovers, dialogs, below-fold content and newly mounted virtual rows in the
existing page/mode matrix. Artifact documents retain ADR-0038's sandbox
boundary: the enclosing workbench controls participate in this inventory.

Acceptance must prove both detection and exclusions:

- Seed a native button without a command, role-only and handler-only actions,
  an invalid target, disabled-state mismatch and late-mounted row. Each fails,
  including below the fold; a focusable scroll region and delegation-only
  root pass, but adding an action to either without a command fails.
- A native link's Enter, plain click, Meta/Ctrl-click, Shift-click,
  middle-click and context-menu open preserve URL, target, download and
  native disposition, without duplicate application effects. A disabled link
  cannot navigate. Palette/hint ordinary activation uses the same destination.
- Text editing and composition, checkbox/radio/select changes, valid/invalid
  form submission and Enter submission preserve native events and validation;
  each valid application submission executes once. Stale/disabled commands
  cannot submit an effect through any adapter.
- Hidden/inert menu controls enter coverage when revealed; unbuilt controls
  produce no command. Gallery demos pass their fixture/native checks without
  product commands; importing the gallery exemption into production fails.

Use the existing shrinking conformance exception mechanism with an issue;
there is no blanket exemption for legacy components. #2972's runtime key
guard and `design/keys` remain separate required gates.

### 3. Bindings depend on both shell and platform

The owner-approved defaults from #2978 are:

| Profile | Agent slots 1–9 | Next needs-you |
| --- | --- | --- |
| Tauri on macOS | `Meta+1` … `Meta+9` (⌘1–9) | `Meta+J` (⌘J) |
| Browser, any OS | `g 1` … `g 9` | `g n` |
| Tauri on Windows/Linux | Await owner answer Q2; do not translate ⌘ into Ctrl automatically. | Await Q2. |

Slots use the stable rendered order of the current agent navigation region;
missing slots are unavailable, and a pending sequence pins the region's
ordering revision. A changed revision cancels it. `next needs-you` remains
a dedicated command even when smart next has broader candidates.
Sequences and hint entry run outside text fields only. They cancel on
composition start, focus/region change, blur, modal change, Escape or an
invalid continuation. The invalid key is not redispatched as another app
command. A configurable bounded sequence deadline clears incomplete input;
the proposed default is 1000 ms (Q3). Repeats do not start or advance sequences.
A terminal sequence cannot also be a prefix of another active sequence.

Extend ADR-0038 §6's `__FLOCK_SHELL_CONFIG__` with a typed presentation
member `keyboard: { shell: "desktop", platform: "macos" | "windows" |
"linux" }`, injected by the native shell before the shared bundle runs.
This member is independent of transport selection and carries no authority.
Absent metadata means browser, even if a browser supplies transport config;
invalid metadata falls back to browser defaults. Browser OS detection is
only for modifier display and the reserved-key checks. Do not infer desktop
from a user agent, server node OS or transport URL. Test the same bundle in
each profile; the native shell must supply the actual client platform.

### 4. Keymap overrides use the existing configuration transaction

Proposed typed registry leaf: `gui.keymap`, a map from profile to command id
to an ordered list of sequences. Each sequence is a list of #2972-style
`Combo` objects (`key`, `mod`, `ctrl`, `alt`, `shift`); `[]` explicitly unbinds
a command. For example, this JSON-shaped schema value changes one browser
command (not a separate settings file):

```json
{"browser":{"navigation.nextNeedsYou":[[{"key":"g"},{"key":"n"}]]}}
```

Profiles are `browser`, `desktop-macos`, `desktop-windows` and
`desktop-linux`. Effective defaults fill commands absent from the resolved
override map. Omitted overrides inherit; reset removes the override rather
than writing defaults. File maps merge by profile and command id, replacing
one command's complete binding list; env/flag collection values replace
according to ADR-0042 C11. Reads show the winning source, shadowed entries,
candidate and running values. Writes use the revision-checked, authorized
config API, not direct GUI file writes or a second browser settings store.
Unknown persisted command ids survive round trips but remain inactive with
a diagnostic, following ADR-0028 §3a; new writes reject unknown ids.

Resolve source layers in ADR-0042 C7/C8 order:
`defaults < file < env < explicit flags < runtime`. Within an admitted
source, specificity is global < node (machine) < project (repo) < agent;
local file overlay wins over its base. A machine env value still beats an
agent file. Targets use registered IDs and agent generation, never cwd.

There is a real scope conflict: #2978 requests machine → repo → agent,
while ADR-0042 forbids repo content from supplying user preferences. Q1
must settle source admission before project overrides ship. Recommend the
ADR-0048 D4 precedent: operator-owned settings keyed by registered project id,
stored outside the checkout, with no repo-authored key installation. Agent
overrides fit ADR-0042's operator-owned storage and registered identity plus
generation checks; they are scoped preferences, not agent-authored grants.
Recommend owning these settings on the operator's client device, retaining
project/agent references when connecting to remote nodes, with no implicit
replication. The remote server's platform never selects the client profile.
Until the owner accepts that storage/source admission and the corresponding
ADR-0042 resolver is available, only global/node preferences activate;
project/agent activation waits rather than inventing another settings store.

Validate the complete resolved map atomically before activation. Reject
malformed chords, duplicate active sequences, terminal/prefix overlap,
reserved keys and conflicts across scopes that can be simultaneously active.
Disjoint focused objects may share a binding (composer send and focused
permission allow are existing examples). Region nesting and modal precedence
must make a winner explicit; declaration order is not conflict resolution.
Diagnostics name both command ids, profiles, scopes and winning sources.
Failed reload retains the last valid running map and shows the error.

The initial reservation policy protects these standards; it is a minimum,
not permission to intercept other OS or assistive-technology shortcuts:

| Platform/context | Reserved from arbitrary rebinding |
| --- | --- |
| Browser, all OSes | Browser tab/window navigation (`Mod+1–9`, `Mod+T/W/N`, `Mod+Shift+T/N`, `Ctrl+Tab`, `Ctrl+Shift+Tab`); location/search (`Mod+L`, `Mod+F`); reload (`Mod+R`, shifted reload, F5); history (`Alt+Left/Right`, macOS `Meta+[/]`); browser downloads/selection `Mod+J`; zoom (`Mod++/-/0`); print/save (`Mod+P/S`); developer tools (`F12`, platform developer-tools chords). |
| macOS, either shell | `Meta+Q/H/M`, `Meta+Space`, `Meta+Tab`, `Meta+Option+Escape`, window cycling, OS Mission Control chords; native text editing including `Meta+A/C/X/V/Z`, `Meta+Shift+Z`, `Meta+Backspace`, `Control+K` and movement/selection chords in editable controls. |
| Windows/Linux, either shell | `Alt+F4`, `Alt+Tab`, `Ctrl+Alt+Delete`, system/launcher `Super` chords; native editing including `Ctrl+A/C/X/V/Z/Y`, `Ctrl+Shift+Z`, word movement/deletion and selection chords in editable controls. Desktop-environment and accessibility reservations also constrain custom bindings. |
| Native widgets, all profiles | Tab/Shift+Tab, Space/Enter activation, Escape dismissal and widget arrows retain their standard meaning within their owning widget/layer. They may map only to that semantic command there. |

`Mod` is Meta on macOS and Ctrl elsewhere. Semantic native commands keep
their standard keys; arbitrary commands cannot take them. The existing
palette `Mod+K` remains an explicit application default, with macOS
`Control+K` preserved for editing. Desktop macOS ⌘J is allowed because the
browser reservation is shell-specific. Validation never promises to capture
keys intercepted by the OS; unsupported client reservations require tests
and owner review before a binding is offered.

Executable reservation fixtures enumerate concrete chords for every family
in the table, by shell, OS and focus context, with a forbidden override and
its expected diagnostic. Cover platform developer-tools, window cycling,
Mission Control and native editing expansions explicitly; a prose family is
not an executable test case. Dispatch fixtures also assert reserved native
keys are not cancelled by the app. Positive cases include native semantic
bindings, disjoint focused scopes and desktop macOS ⌘J; the same ⌘J is
rejected as a browser override. Unknown OS/accessibility reservations remain
unavailable for overrides until added to these fixtures.

### 5. Hints teach the same resolved command model

Which-key opens a nonmodal, non-focus-stealing continuation view after a
valid prefix. It displays only available continuations for the active
layer/context, with labels and resolved keys from the trie. Cancellation
follows decision 3. Proposed typed configuration defaults (Q3) are
`gui.keys.sequence_timeout_ms = 1000` (range 100–5000) and
`gui.hints.which_key_delay_ms = 300` (range 0–1000), with delay strictly less
than sequence timeout. The deadline starts at each accepted stroke;
which-key appears once its delay elapses without continuation, and a
continuation updates it without an automatic live announcement. It is not
another keyboard listener.

Link-hint entry is a command (`hints.open`; `f` is a proposed default pending
Q3). It pushes an exclusive layer. Snapshot eligible visible, enabled,
command-backed targets in the top modal or, without one, the workbench.
Visible means mounted and intersecting its clipping region and viewport,
not merely a virtual list's overscan. Sort by visual reading order with DOM
order as a stable tie-break. Assign deterministic prefix-free one- or
two-letter labels from the proposed configurable ordered alphabet
`gui.hints.alphabet = "asdfghjklqwertyuiopzxcvbnm"` (Q3), after excluding
active layer bindings. Validate 2–26 distinct lowercase ASCII letters and a
nonempty usable alphabet after exclusions. If more than the two-letter
capacity fit, show bounded pages with registry-owned paging commands; never duplicate a label.

While hints are open, ordinary application sequences are suspended; Escape
and paging keys are reserved in the hint layer and cannot be label tokens.
Do not consume browser/OS standards. A full label revalidates its pinned
target, closes hints, and invokes the command; dangerous actions still open
their normal confirmation. Scroll, resize, target removal or projection
revision invalidates the snapshot and cancels hints rather than relabeling
under partially typed input. Virtualized lists label only rendered visible
rows; scrolling and opening hints again labels the new rows. The palette
and normal list navigation reach every offscreen target.

Labels are visual annotations with `aria-hidden`, not additions to each
control's accessible name. On explicit entry announce one concise status
and Escape instructions; do not announce every label or every stream update.
No automatic live announcements accompany which-key updates. Focus stays at
the originating control until activation, then follows that command's normal
focus behavior. The palette, `?` sheet, normal Tab/widget traversal and
screen-reader names remain complete alternatives. Test keyboard-only use,
reduced motion, increased contrast and live-region output count.

### 6. Smart next starts with a complete deterministic rule

Stage (a) implements `navigation.smartNext`, separately from
`navigation.nextNeedsYou`. It only opens/focuses a target; it never answers,
submits, retries or changes a server record. The candidate universe is the
current navigation context's complete server-projected structured-agent
set, including offscreen rows, restricted to agents the operator can open.
A project region limits it to that project; the global region uses the
connected fleet. Exclude ended/gone agents and the current target.

For each authoritative projection revision, assign each candidate one band:

1. **Needs-you:** has an open item addressed to this operator, including the
   server's effective reroute addressee. Delegated-to-someone-else is not
   needs-you. In GUI-P0 only, use the existing non-automatic open-request
   signal and identify the reason as provisional, never claim G5 exists.
2. **Blocked:** server reports `blocked`, and the agent is not in band 1.
3. **Waiting:** server reports a waiting/idle state, and the agent is not
   in bands 1 or 2.

Order by band, then earliest server timestamp entering that band's current
condition (longest wait first), then stable AgentRef identity. For needs-you,
use the oldest still-open operator-addressed item; for blocked/waiting, use
the start of the current uninterrupted state. `navigation.nextNeedsYou` uses
this same rule restricted to band 1. Unknown ages
sort after known ages in the same band; never synthesize them from browser
arrival time. If even membership is unknown, omit the candidate and show
incomplete coverage. Running agents with none of these conditions are not
waiting candidates. No candidate means unavailable with “No next agent”.

Pin the revision and target before focus moves; recheck availability before
opening. A disappeared target causes one recomputation, then a visible
unavailable result, not an unbounded retry. Repeated invocations recompute
with the newly current target excluded; they do not mark attention handled.
Show a preview and reason, such as “Needs you: permission”, “Blocked: login”
or “Waiting since …”; unknown age is stated. For needs-you, opening may
focus its highest server-ranked item, preserving ADR-0048 D4 order. No
client rank is written back to the attention queue or its counts.

Stages (b) and (c) may rank only this same available candidate set; empty,
invalid, stale or low-confidence output falls back to (a). They cannot hide
needs-you items or change attention priority. Q4 must decide whether they
may ever suggest a lower band before a higher one; until answered, band
order is a hard constraint. Show why before activation, including whether
the reason is deterministic, local frequency or a model suggestion.

### 7. Learning stays local; System One is a gated spike

Stage (b) records only successful, operator-initiated navigation transitions.
Each observation is `(context, destination_token, command_id, target_kind,
time_bucket)`. Context is exactly region kind, source object kind, destination
state band and previous command id. For each candidate, derive
`destination_token = HMAC-SHA-256(device_profile_secret, canonical AgentRef)`;
the canonical reference includes node, agent id and generation. This makes two
agents opened by `agent.open` distinguishable without storing the raw id.
The secret is random per local operator profile/origin, never a shared or
public salt. Raw references exist only in the current in-memory projection;
never persist a token-to-id map. Pseudonyms are still personal data and require
Q4's privacy approval; hashing does not waive retention or opt-out.

Ranking is defined from exactly these fields: for each current candidate,
count retained observations matching its derived destination token, its
current context and resolved destination command `agent.open`. Palette, row
and direct-binding opens record that same semantic command id; the
`navigation.smartNext` wrapper is not a different learned destination. Rank
by descending count within each permitted band. Equal counts (including zero) use (a)'s
age/identity order. Expired observations contribute nothing; no opaque model
or unrecorded repo/title feature contributes to (b). The explanation states
“Opened this agent N times from this context” and the retention window;
never display the token. A new generation starts cold. Missing/corrupt
history or an invalid secret falls back to (a), with no partial guessed rank.

Do not store message content, titles, paths, credentials or raw agent/node
identifiers. Never train from programmatic focus changes or a ranker's own
suggestions, even when the operator accepts one. The initiating command marks
whether the choice was independent, so suggestion acceptance cannot reinforce
itself. Source identity is not needed in the recorded context.

Store observations and aggregates only in the client device's local profile:
IndexedDB for a browser origin, application-local profile storage for Tauri.
Do not sync, upload to the server, include in operational telemetry or share
across operators/origins. Both records and derived counts are subject to
expiry and bounded capacity. Q4 must set the retention duration, capacity
and default enablement before collection ships; until then collection is off
and retention is zero. A per-user off control stops collection and deletes
observations, aggregates, model evaluation records, derived caches and the
profile secret; a separate clear-history action does the same without enabling learning. Future explicit
collection creates a new secret. Expiry removes observations and recomputes
counts, dropping empty aggregates; when no retained history remains, discard
the secret. Users can inspect the coarse observation records and delete a
selected record or candidate's history; editing means deletion/retraction,
not inserting synthetic training records. Each deletion invalidates cached
rankings and recomputes counts before the next suggestion. The policy and
controls are exposed through the ADR-0041/0042 typed configuration surface;
local observations are not themselves configuration. Denied/unavailable
storage falls back to (a), with no server substitute.

Stage (c) is a **spike, not authorization to ship a model**. Its proposed
input is the same coarse context plus ephemeral candidate tokens and reason
bands, never raw content or durable target identities. Persist only the
minimum local evaluation record (model/version, candidate count, confidence,
accepted/rejected suggestion and deterministic fallback reason) under the
same retention and deletion policy as (b). No prompt/completion corpus is
retained. A model explanation must cite supplied candidate facts; generated
prose is not evidence. A response naming a missing candidate, lacking a
reason, arriving after a revision change, failing validation or falling
below the owner-set threshold is discarded in favor of (a). The model can
suggest navigation only, never execute a command.

“Local only” covers inference as well as storage. If System One needs a
remote provider, the spike stops at offline evaluation until the owner
explicitly changes that boundary. The spike must deliver a local-runtime
feasibility report, confidence calibration on held-out synthetic navigation
cases, latency/failure measurements, comparison with (a)/(b), and a proposed
threshold and bounded response deadline. These are engineering deliverables,
not questions for the owner to solve. No suitable local runtime or calibrated
improvement means a negative spike result and continued (a)/(b), not a remote
provider fallback. The owner decides permitted input context, acceptance of
the spike's proposed operating limits, live opt-in and any remote-inference
permission (Q5). The UI always offers deterministic mode independently of
learning availability.

## Alternatives considered

- **Per-component shortcuts:** smaller first edits, but duplicate labels,
  scope logic and availability; no exhaustive palette or conformance gate.
- **A Vim-modal UI:** introduces editing/navigation modes across native
  fields and existing modal layers. Optional hint entry gives reachability
  without making every operator learn a modal editing model.
- **DOM-only hints and arbitrary click activation:** find incidental nodes
  without semantic commands, authority checks or stable target identity.
- **A server-side learned ranker:** couples device navigation history to
  server storage and can contradict ADR-0048's shared attention order. Local
  ranking has a deterministic fallback and no cross-device history promise.
- **Learning from launch:** makes early behavior unexplained and hides
  whether the deterministic candidate set works. Stage (a) must ship first.

## Ordered implementation sub-issues

These are proposed slices, not filed issues. Every resulting implementation
PR, including follow-up and review-fix PRs, is capped at 400 changed handwritten
code lines: additions plus deletions in production code and tests, excluding
generated files and prose documentation. Every test/helper change counts.
The table budgets include tests; split any row into dependency-ordered PRs
before exceeding the cap. Estimates are not permission to combine rows. Each
row needs fail-first evidence through actual UI/config ingress, not only a
hand-constructed registry. This docs-only ADR is prose, not implementation
code counted against that cap.

| Order | Slice | Dependencies | Code + test lines | Acceptance evidence |
| --- | --- | --- | --- | --- |
| 1 | Typed descriptors, target instances and single dispatcher | #2972 landed | 250–350 | Click and registry key dispatch the same target; unavailable/stale target refuses. |
| 2 | Migrate existing controls and palette/sheet consumers | 1 | 250–400 | All currently built controls appear by command; unavailable toolbar actions remain listed. Split by surface if the cap requires it. |
| 3 | Independent control census and seeded command conformance gate | 2 | 250–400 | Decision 2's native/modified-link, field/submit, scroll/root, hidden and gallery acceptance cases run; missing actions fail. Existing key-guard seeds still fail. |
| 4 | Shell platform metadata and approved profile defaults | 1; Q2 for Windows/Linux defaults | 150–250 | Same bundle uses macOS desktop keys and browser sequences; absent/invalid metadata uses browser profile. |
| 5 | Sequence trie, cancellation and conflict/reservation validator | 4; Q3 timing | 250–400 | Executable per-profile reservation fixtures reject every listed shortcut family, allow semantic native editing and macOS desktop ⌘J, and reject browser Mod+J. Real input proves IME/text isolation, cancellation and prefix conflicts. |
| 6 | Typed keymap read/resolve schema and provenance | 5; ADR-0042 registry/read foundations; Q1 admission | 200–350 | Real config resolution proves source-before-scope and retained unknown persisted ids. |
| 7 | Keymap writes, reset and atomic reload | 6; ADR-0042 revision-checked writer | 200–350 | API write/read/reload shows winning bindings; invalid candidate preserves the running map. |
| 8 | Which-key continuation view | 5, 7; Q3 | 150–250 | Prefix shows actual effective bindings and cancels with the owning layer. |
| 9 | Hint target snapshot, label allocator and exclusive layer | 3, 5; Q3 entry key | 250–350 | Collision, capacity paging and removed/virtualized target cases cannot activate the wrong command. |
| 10 | Hint presentation and accessibility | 9 | 200–350 | Rendered hints preserve names/focus; one entry announcement; scrolling cancels. |
| 11 | Deterministic smart-next selector and explanation projection | 1; required authoritative membership/age fields | 200–300 | Real server frames establish band/age/tie order and expose incomplete data. |
| 12 | Smart-next command, preview and focus integration | 11 | 150–250 | UI invocation reaches an offscreen target, revalidates once and preserves server attention order. |
| 13 | Local history store, retention and inspect/delete/opt-out controls | 7, 12; Q4 | 200–350 | Two agents opened by the same command get distinct profile tokens. User record/candidate deletion, clear and opt-out remove matching counts/caches; expiry deletes retained data and unused secrets. |
| 14 | Local transition ranking and explanations | 13 | 200–300 | Same-context agents with different observed counts rank differently; ties/zero use (a). User history deletion changes the next rank; suggestions and programmatic focus cannot train it. |
| 15 | Offline System One feasibility/evaluation spike | 12; Q4–Q5 before live collection/inference | ≤300 | Deliver local-runtime feasibility, held-out calibration, latency/failure bounds and comparison with (a)/(b), including a negative feasibility result; no live rollout. |

## Owner questions

1. **Scope admission and device ownership:** approve the recommendation to
   follow ADR-0048 D4: operator-owned settings keyed by project id outside
   the checkout, never repo-authored preferences. Use ADR-0042's
   operator-owned, generation-checked agent overrides in the same client
   device's settings authority; remote node selection does not move that
   authority. Confirm this source admission/ownership before project/agent
   activation; the typed resolver/writer must also exist first.
2. **Desktop Windows/Linux defaults:** does the approved ⌘1–9 / ⌘J mean
   macOS only? What non-reserved bindings should the other desktop profiles
   receive? No automatic Ctrl translation is assumed.
3. **Hint and sequence policy:** approve the configurable proposed defaults:
   `f` for hints, alphabet `asdfghjklqwertyuiopzxcvbnm`, sequence timeout
   1000 ms, which-key delay 300 ms, with decision 5's validation. They are
   registry-backed preferences; no timing literal lives in a component.
4. **Local learning:** choose opt-in/default-off versus another default,
   retention duration and capacity, time bucket size, minimum evidence before
   ranking, and whether suggestions may cross deterministic bands. Approve
   the per-profile destination pseudonyms and their deletion/secret lifecycle,
   or require a different feature policy before collection. Collection stays
   off with zero retention until these are set; (a) works independently.
5. **System One rollout/privacy:** approve the permitted coarse input context,
   any live opt-in experiment, and the operating limits recommended by the
   spike. Local-runtime feasibility and confidence calibration are spike
   deliverables in decision 7. Remote inference still requires an explicit
   separate permission; this proposal grants none.

## Consequences

The GUI gains one auditable path from a visible action to keyboard access,
with configuration and hints explaining the same resolved bindings. The
migration costs are explicit: every existing control needs an adapter, the
conformance census must catch controls it did not register, and server
projection gaps can delay deterministic smart next. No new server authority,
attention ranking, Rust runtime behavior or released user documentation is
claimed by this docs-only proposal.
