# ADR 0017 — Handed-over files are MCP resources with a durable identity; tools stay for parameterised calls

- Status: Proposed
- Date: 2026-08-28
- Issues: #286 (this design), #79 / #80 (the P0 file transport this replaces the
  read side of), #276 / #379 (`flock_agent_history` — the other consumer the
  issue asked to design against), ADR-0005 (durable event log — the substrate,
  and the P5 that forbids a second one), ADR-0014 (the narrowed-verb precedent),
  ADR-0016 (the notification log this shares a substrate with), ADR-0009 (fleet
  transport — why cross-host content is pulled, not replicated).
- Decision owner: operator. Decision 5 — whether the paste survives now that the
  resource exists — is deliberately left as a recommendation, not resolved here.

## Context

A file handed to flock is staged server-side and its path is typed into the
agent's input surface. Verified rather than assumed, on the code as it stands:

- `HeadlessServer::write_client_clipboard_image` staged the bytes and pushed the
  `PathBuf` onto `ClientConnection::staged_clipboard_files`.
- `remove_client` deleted every file in that vector, and so did the shutdown
  path and `Drop`. **The file a human handed to an agent stopped existing when
  the operator detached.**
- Nothing enumerated it. There was no verb, on any surface, that answered "what
  have I been handed" — the path existed in exactly one place, the scrollback of
  the pane it was pasted into.
- `ClientMessage::ClipboardImage` carries `extension` and `data`. It does not
  carry the file's name, so nothing downstream can know it.

So the record was the paste, and the paste is a keystroke: it survives as long
as the terminal buffer does, it reaches one agent, and re-reading it means
scrolling back to find the path again.

## Decision

### 1. Files are exposed as MCP **resources**, not as a fifteenth tool

MCP separates the two primitives, and the line falls exactly where flock's two
candidates fall.

A **tool** is a parameterised call. `flock_agent_history` takes a detail level,
a cursor and a limit; the answer is computed per call and no URI stands for it.
A **resource** is content the server already holds, that it can enumerate and
hand back by a stable identifier.

The issue asked whether #276's history belongs on the resource surface too. It
does not, and this is the reason: a transcript page is an answer to a question,
a handed-over file is an object. `flock_agent_history` stays a tool, the tool
table keeps its fourteen entries and its order, and this ADR adds no rows to it.

### 2. The record shares ADR-0016's substrate; the bytes do not go in it

`EventKind::FileHandedOver` is a new kind on the **existing** durable event log,
and `HandoffLog` is a fold over it, rebuilt at boot in the same
`persisted_events_after(0)` pass that already rebuilds the agent mailboxes and
the notification list.

The question the issue raised — should the notification log and the file
registry share one store — has two halves, and they get different answers:

- **The substrate: shared.** ADR-0005's design round ruled out a second event
  system, and ADR-0016 already declined to build one. A third would inherit the
  same four solved problems (format, rotation, corruption, restart) to re-solve.
- **The list: not shared.** A notification is prose addressed to the *operator*
  with an unread bit; a handoff is bytes addressed to an *agent* with no such
  thing. Folding them into one list would mean one of them acquires a field
  that means nothing for it. They are two projections over one log, which is
  what the log is for.

And the bytes stay on disk. A JSONL audit log is not a blob store: 16 MB of
base64 on one line would be held hostage by ADR-0005's rotation, and shredded by
it. So the filesystem owns the bytes and the log owns the record — which makes
the seed a **reconciliation**: a record whose staged file is gone (a reboot
cleared the OS temp dir) is dropped rather than listed as a resource that cannot
be read. Every URI `resources/list` advertises is one `resources/read` can serve.

### 3. The lifetime is the file's, not the connection's

`remove_client`, the shutdown drain and `Drop` no longer delete staged files.
The file was handed to an agent, not lent to a connection.

Retention is therefore two bounds, both of which have to hold on their own:

- **Count** — 128 records, oldest evicted first, and eviction deletes the bytes
  with the record. Far below ADR-0016's 512: a notification is a line of prose,
  a handoff pins a file on disk that only eviction frees.
- **Age** — the staging directory's existing 24-hour sweep, which now also runs
  at boot. It used to run only when a *new* file was staged, so a server that
  never saw another drop never swept; that was survivable when the connection
  deleted everything anyway, and is not now.

The floor under both is the same one ADR-0016 named: a record older than the
event log's own rotation cannot be rebuilt after a restart.

### 4. The list is not filtered by who is asking

`handoff.list` returns every handed-over file this server holds, each row
carrying the `pane_id` and `agent_id` it was handed to. `target` narrows it on
request; the default does not narrow at all.

Filtering by the caller's identity would be the tidier default and is the wrong
one. An MCP server normally runs as a child of the agent's own pane, so the
caller *usually* resolves by process ancestry — but when it does not (a desktop
MCP client, a re-parented process), the honest failure mode of a filtered list
is an **empty** one, and "nothing was handed to you" is indistinguishable from
"nothing was handed over at all". That is the one answer that must never be a
guess. `total` is reported alongside the filtered rows for the same reason.

This grants no authority that was not already granted: any agent can already
read any pane on this server with `flock_agent_read`.

### 5. The paste stays — and whether it should is the operator's call

`resources/list` is only reachable by an agent that has flock's MCP wired up.
flock cannot know from the outside whether the pane it is about to paste into
has one, so suppressing the paste on the assumption that it does would silently
lose the file for every agent that does not. The paste therefore stays exactly
as #80 shipped it.

The alternative — suppress the injected `read this file:` line when the target
pane's agent is known to have the flock MCP registered — is buildable (flock
already detects other agents' MCP status) and is a behaviour change to a working
path. It is named here rather than taken.

### 6. Cross-host is out of scope, with a reason

The issue notes that nothing else in the fleet can see a handed-over file.
Same-host is what ships. ADR-0009 measured and rejected replication, and
pushing file bytes to every peer is replication of the worst-shaped data for it.
`origin_host` is on the record so the pull has somewhere to point; a fleet-wide
listing is a follow-up with its own relay and staleness story.

## Consequences

- The MCP server advertises a `resources` capability. Clients that previously
  got `-32601` for `resources/list` now get a list. No `subscribe` or
  `listChanged` sub-capability is claimed, because flock pushes no resource
  notifications.
- `handoff.read` inlines at most 4 MiB and otherwise refuses with the staged
  path. The bytes and the agent share a machine by construction, so the refusal
  is a redirection rather than a denial.
- A resource is named by the *staged* file's name, not the name it had on the
  machine it was dropped from — `ClientMessage::ClipboardImage` never carried
  that, and adding it is a bincode wire change with its own protocol bump.
- Two read-only verbs are added to the socket API and neither is in
  `request_changes_ui`, so an agent may poll them the way it polls
  `agent.history`.
