# Structured runtime fixtures

`claude.py` is an owned fake executable, not a live Claude invocation. Runtime
tests copy it and the S1 event catalogue into a private directory, select its
case there, and keep a FIFO lifeline open. The fake verifies the complete launch
argv, process group, environment keys, token placement and generated settings.
Python's Darwin startup adds `__CF_USER_TEXT_ENCODING`; the fixture excludes
only that interpreter-added marker when checking environment keys.

Review-round regressions cover a naturally exiting producer whose notifications
remain undrained beyond the I/O grace, repeated idle submissions without retry,
and generated-file cleanup when child pipes are missing before any workers exist.
Oversized and non-UTF-8 init cases keep the producer alive until refused. Runtime
refuses genuine lossy transport summaries until the first fully inspected clean
init. After that, loss is forwarded as data and leaves the process running:
ADR-0036 E1 keeps turns in the same process, and E16 binds its socket at startup.
Every inspectable init socket report, including a later init, still refuses.
Complete 1 MiB lines remain accepted. The durable record keeps the transport's
explicit loss evidence. Cancellation confirmed within the grace
permits cleanup and preserves the actual process outcome; notification delivery
is marked cancelled so the consumer can replay available durable records.

Loss summaries include `turn_end`: `result` when the preserved 256-byte prefix
structurally identifies the top-level result header, `not_result` for another
identified type, and `unknown` when no complete type can be read. Nested types
and truncated strings never identify a result. A lossy result ends the input
gate but remains an Unknown event with no claimed success/subtype. Indeterminate
loss during a turn keeps input closed and makes submission explicitly return
`structured_turn_end_unknown`, rather than ordinary busy. A subsequent complete
or identifiable lossy result clears that uncertainty; otherwise the consumer
can stop the session. These conditions never tear down an established session.

The two init JSON files are reconstructed examples derived from the committed
S1 catalogue (Claude Code 2.1.296) and ADR-0036 A.11. Raw probe captures are
unavailable; replacement is tracked in #1385. `init.diff` compares restricted
to unrestricted. Its only difference is the messaging socket key. The path is
a non-executable placeholder, not an operator path. Tests emit the restricted
example and insert real owned socket evidence for refusal cases.

| Field | Fixture comparison | Evidence and limits |
| --- | --- | --- |
| `tools` | Equal | S1's reduced three-tool example; A.11 reports equal tools. |
| `slash_commands` | Equal | Fictional one-command example; A.11 reports 55 equal commands, whose names were not preserved. |
| `capabilities` | Equal | S1's reduced list; A.11 reports equal capabilities. |
| `mcp_servers` | Both empty | Synthetic empty launch config; A.11 separately verifies a configured MCP server under restricted mode, but publishes no complete paired init lists. |
| `skills` | Both empty | Synthetic metadata; no paired measurement is available. |
| `permissionMode` | Both `default` | S1 example and launch settings; no paired A.11 measurement is claimed. |
| `messaging_socket_path` | Absent / present | A.11 documents this difference; runtime refuses presence even with null or malformed values. |

This diff records the reconstructed fixture contract, not a fresh measurement
of every effect of `--restricted`. No model calls or operator profiles are used.
