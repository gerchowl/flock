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
| `execute(context, target)` | The single action path used by click, palette, binding and hint activation. Existing server authorization and confirmations still apply. |

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
An interactive control is any native button, linked anchor, form control,
summary/disclosure, editable element, element with a nonnegative tab index,
or element with an interactive ARIA role (including menu items, tabs, tree
items, options, sliders and composite widgets). Disabled controls count.
The walk also finds pointer/click handlers, including delegated framework
handlers, so an unsemantic clickable `div` cannot escape by lacking a role.
Source lint rejects direct application action handlers outside command
adapters; test-time event instrumentation catches indirect registrations.
Neither check may derive its candidate set from command annotations.

Every candidate must map to a registered descriptor and valid target
instance. Native field adapters map focus/edit intent, and composite widgets
map both container navigation and actionable items. Controls with several
actions declare each intent. Merely adding `data-command` is insufficient:
the test resolves it, compares availability with the rendered enabled state,
and exercises pointer and keyboard dispatch against the same command/target.

This adds command coverage to DESIGN conformance rules 8 (keyboard/name)
and 9 (toolbar/palette parity), without replacing focus, accessible name,
size, contrast or native-widget checks. Coverage includes disabled toolbar
actions, popovers, dialogs, below-fold content and newly mounted virtual rows
in every existing page/mode matrix. Hidden/inert controls have no active
instance; hidden or unbuilt features do not acquire palette actions. The
walk must still inspect inactive/disabled states in their owning fixtures.
Artifact documents retain ADR-0038's sandbox boundary: only the enclosing
workbench controls participate in the application's command inventory.

Seeded failures cover a native button without a command, a role-only and a
handler-only control, an invalid target, a disabled-state mismatch and a
late-mounted row. The test must find each seed, including below the fold.
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
its initial duration remains Q3. Repeats do not start or advance sequences.
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
must settle source admission before project overrides ship. Until then the
proposal supports operator-owned global/node preferences only; project and
agent override activation is blocked, rather than silently granting a repo
permission to install keys. A remote server's platform never selects the
client profile. Which node owns client preferences in a multi-node session
is also Q1; no implicit replication is introduced.

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

### 5. Hints teach the same resolved command model

Which-key opens a nonmodal, non-focus-stealing continuation view after a
valid prefix. It displays only available continuations for the active
layer/context, with labels and resolved keys from the trie. Cancellation
follows decision 3; timing is Q3. It is not another keyboard listener.

Link-hint entry is a command (`hints.open`; `f` is a proposed default pending
Q3). It pushes an exclusive layer. Snapshot eligible visible, enabled,
command-backed targets in the top modal or, without one, the workbench.
Visible means mounted and intersecting its clipping region and viewport,
not merely a virtual list's overscan. Sort by visual reading order with DOM
order as a stable tie-break. Assign deterministic prefix-free one- or
two-letter labels from a fixed ordered alphabet after excluding active
layer bindings. If more than the two-letter capacity fit, show bounded
pages with registry-owned paging commands; never duplicate a label.

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

Stage (b) records only successful, operator-initiated navigation transitions:
coarse context (region kind, object kind, state band, previous command id),
chosen command id/target kind and a coarse timestamp bucket. Do not store
message content, titles, paths, credentials or persistent agent identifiers.
Aggregate frequency or first-order Markov counts for those transitions;
never train from programmatic focus changes or a ranker's own suggestions.
An explanation exposes the contributing transition and sample count. No
samples, ties or invalid statistics use (a)'s order.

Store observations and aggregates only in the client device's local profile:
IndexedDB for a browser origin, application-local profile storage for Tauri.
Do not sync, upload to the server, include in operational telemetry or share
across operators/origins. Both records and derived counts are subject to
expiry and bounded capacity. Q4 must set the retention duration, capacity
and default enablement before collection ships; until then collection is off
and retention is zero. A per-user off control stops collection and deletes
observations, aggregates and derived caches; a separate clear-history action
has the same deletion effect without enabling learning. The policy and
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
explicitly changes that boundary. Q5 must decide context, local runtime,
confidence calibration, latency bound, evaluation criteria and opt-in before
any live model path. The UI always offers deterministic mode independently
of learning availability.

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

These are proposed slices, not filed issues. Each first PR is capped at
400 production code lines (tests and documentation excluded). Estimates are
budgets, not permission to combine rows. Each row needs a fail-first test
through actual UI/config ingress, not only a hand-constructed registry.

| Order | Slice | Dependencies | Production lines | Acceptance evidence |
| --- | --- | --- | --- | --- |
| 1 | Typed descriptors, target instances and single dispatcher | #2972 landed | 250–350 | Click and registry key dispatch the same target; unavailable/stale target refuses. |
| 2 | Migrate existing controls and palette/sheet consumers | 1 | 250–400 | All currently built controls appear by command; unavailable toolbar actions remain listed. Split by surface if the cap requires it. |
| 3 | Independent control census and seeded command conformance gate | 2 | 250–400 | Native, role-only, handler-only and late-mounted missing commands fail the real rendered walk. Existing key-guard seeds still fail. |
| 4 | Shell platform metadata and approved profile defaults | 1; Q2 for Windows/Linux defaults | 150–250 | Same bundle uses macOS desktop keys and browser sequences; absent/invalid metadata uses browser profile. |
| 5 | Sequence trie, cancellation and conflict/reservation validator | 4; Q3 timing | 250–400 | Real input proves composition/text isolation, stale-region cancellation, prefix conflicts and reserved-key refusal. |
| 6 | Typed keymap read/resolve schema and provenance | 5; ADR-0042 registry/read foundations; Q1 admission | 200–350 | Real config resolution proves source-before-scope and retained unknown persisted ids. |
| 7 | Keymap writes, reset and atomic reload | 6; ADR-0042 revision-checked writer | 200–350 | API write/read/reload shows winning bindings; invalid candidate preserves the running map. |
| 8 | Which-key continuation view | 5, 7; Q3 | 150–250 | Prefix shows actual effective bindings and cancels with the owning layer. |
| 9 | Hint target snapshot, label allocator and exclusive layer | 3, 5; Q3 entry key | 250–350 | Collision, capacity paging and removed/virtualized target cases cannot activate the wrong command. |
| 10 | Hint presentation and accessibility | 9 | 200–350 | Rendered hints preserve names/focus; one entry announcement; scrolling cancels. |
| 11 | Deterministic smart-next selector and explanation projection | 1; required authoritative membership/age fields | 200–300 | Real server frames establish band/age/tie order and expose incomplete data. |
| 12 | Smart-next command, preview and focus integration | 11 | 150–250 | UI invocation reaches an offscreen target, revalidates once and preserves server attention order. |
| 13 | Local history store, retention and opt-out/clear controls | 7, 12; Q4 | 200–350 | Navigation ingress records permitted coarse fields; expiry and opt-out delete raw and derived data. |
| 14 | Local transition ranking and explanations | 13 | 200–300 | Known transitions explain counts; absent data uses (a); no feedback from programmatic focus. |
| 15 | Offline System One feasibility/evaluation spike | 12; Q4–Q5 before live collection/inference | ≤300 | Report local-runtime feasibility, calibration, latency and fallback against deterministic cases; no live rollout. |

## Owner questions

1. **Scope admission and device ownership:** #2978 asks for machine → repo →
   agent overrides, but ADR-0042 disallows repo-authored user preferences.
   Should project bindings be operator-owned references outside the repo,
   or should this explicitly amend that restriction? Which client-local node
   owns preferences when the GUI connects through a home gateway to several
   nodes? Project/agent keymap activation waits for this answer.
2. **Desktop Windows/Linux defaults:** does the approved ⌘1–9 / ⌘J mean
   macOS only? What non-reserved bindings should the other desktop profiles
   receive? No automatic Ctrl translation is assumed.
3. **Hint and sequence policy:** approve `f` for hints, the hint alphabet,
   sequence timeout and which-key display delay. Existing ADRs and #2978 do
   not settle those values; they must be registry-backed configuration,
   with no timing literal hidden in a component.
4. **Local learning:** choose opt-in/default-off versus another default,
   retention duration and capacity, time bucket size, the minimum evidence
   before ranking, and whether a learned suggestion may cross deterministic
   bands. Collection stays off until these are set; (a) works independently.
5. **System One spike:** what coarse context is sufficient, is a local-only
   runtime available, what confidence threshold is calibrated well enough,
   what latency is acceptable, and what offline success metric warrants a
   live opt-in experiment? A remote model requires a separate privacy
   decision; this proposal grants none.

## Consequences

The GUI gains one auditable path from a visible action to keyboard access,
with configuration and hints explaining the same resolved bindings. The
migration costs are explicit: every existing control needs an adapter, the
conformance census must catch controls it did not register, and server
projection gaps can delay deterministic smart next. No new server authority,
attention ranking, Rust runtime behavior or released user documentation is
claimed by this docs-only proposal.
