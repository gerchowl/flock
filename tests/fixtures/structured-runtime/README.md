# Structured runtime fixtures

`claude.py` is an owned fake executable, not a live Claude invocation. Runtime
tests copy it and the S1 event catalogue into a private directory, select its
case there, and keep a FIFO lifeline open. The fake verifies the complete launch
argv, process group, environment keys, token placement and generated settings.
Python's Darwin startup adds `__CF_USER_TEXT_ENCODING`; the fixture excludes
only that interpreter-added marker when checking environment keys.

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
