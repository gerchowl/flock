# Scripted GUI fleet model

This offline Python fixture implements the Claude stream-json subset used by
GUI lifecycle tests. It runs no tools and makes no model or network calls.
Install `claude.py` as `claude` on the isolated server's PATH, with a Python 3
shebang. Use a disposable HOME and structured configuration directory, as in
`tests/client_tokens/gui_fleet.rs`. Never install it in a live profile.

Write a JSON array to `$HOME/gui-fleet.json` before starting the server:

```json
[
  {"expect": "message", "text": "hello"},
  {"emit": "stream", "chunks": ["hello", " world"]},
  {"expect": "gate", "name": "partial-observed"},
  {"emit": "permission", "request_id": "example-request", "input": {"command": "true"}},
  {"expect": "decision", "behavior": "allow"},
  {"emit": "reply", "text": "finished"},
  {"expect": "message", "text": "continue"},
  {"emit": "reply", "text": "resumed"}
]
```

Each step has exactly the documented fields. `message` checks the exact user
text, extracting the body when the runtime supplies its nonce-delimited queue frame.
`stream` emits and flushes each partial separately within an active turn.
`permission` emits a Bash permission request but never executes its input.
`decision` blocks on the matching request's response, with behavior `allow` or
`deny`. `reply` emits an assistant message and successful result, completing the
turn. Permission decisions provide an input barrier for observing partials before
completion, without sleeps or timing assumptions.

The optional `gate` step waits for the driver to connect to the Unix socket
`$HOME/g-UUID` and send its name as a JSON string followed by a newline. The
driver must release `partial-observed` after observing the partials in the
example above. Omit the gate for an uninterrupted scripted turn. This keeps
observation deterministic when durable permission records can overtake live
partials in a consumer. Gate messages must fit in 4096 bytes. The socket exists
only for scripts containing gates, and the disposable HOME must keep its path
under the platform's Unix socket length limit.

`--session-id UUID` creates a session. `--resume UUID` requires that session and
continues after its last completed reply using the same id. Checkpoints live in
`$CLAUDE_CONFIG_DIR/gui-fleet/UUID.json`, using synced atomic replacement. Resume
requires the script to remain identical. Restart only after a completed turn;
interrupted turns replay from the previous checkpoint and are outside this
fixture's lifecycle proof. `$HOME/gui-fleet-launches.json` records session ids and
whether each launch used resume, so tests can distinguish resume from a new
session with an accidentally reused id.

The fixture accepts additional Claude launch arguments, and answers `--version`
for harness discovery. Missing/duplicate identity flags, invalid UUIDs, malformed
JSON, unknown steps/fields, unexpected input/EOF and mismatched decisions exit 1
with a `gui-fleet fixture:` diagnostic on stderr. EOF after the final completed
step exits successfully. An idle process waiting for scripted input has no
internal deadline; test drivers must bound waits and own process cleanup,
including a producer that stays silent or waits at a gate.

Run the browser-free Rust lifecycle proof with the `web` feature:

```sh
cargo nextest run --locked --features web --test client_tokens --test gui_fleet_fixture \
  -E 'test(gui_fleet) | binary(gui_fleet_fixture)' --test-threads 2
```

The lifecycle test uses a real server, public `flk --json` start/send/status/stop
commands, the native rendered view for live partials, and authenticated structured
wire subscriptions and permission decision.
Restart uses the existing operator stop/start path. It checks both the reported
session id and the actual resumed launch. A separate fixture test checks a
continued scripted reply after the completed-turn checkpoint. The GUI restart command is separate
work; this fixture does not change production lifecycle behavior.
