# Structured-wire scripts (fixture version 1)

These scripts are the S7a contract for G3 (#1595), governed by ADR-0036 §4,
§5 and §5a and ADR-0038 §3. They describe a fake server, not a runnable
endpoint. No network, token store, model or wall-clock sleep is needed.
Rust defines every frame and script shape and exports them through `just gen-ts`.
`structured_wire::tests::every_script_parses_and_round_trips_all_frame_kinds`
reads every `.jsonl` here, so adding a script automatically adds validation.

Each nonempty line is one JSON object with `v:1`, independent of the initial
`WIRE_JSON_VERSION:1`. Unknown fixture versions, steps, malformed typed frames
and invalid selected fields fail visibly. Additive fields in transport frames
are ignored; unknown transport `t` tags are protocol errors. Unknown session
payloads instead use `{type:"unknown", raw, shape}` inside `ev.ev`: raw is the
exact original line, shape is the server-supplied harness/version identity.
The adapter's bare raw Unknown serialization is not the wire wrapper. S8a's
`SessionEvent` TS union currently includes `unknown`; narrowing fixes belong
to #1656. Non-agent payloads and cmd args/results stay opaque until S8b #1677.

## Operations

| Step | Fields | Runner effect |
| --- | --- | --- |
| `connect` | none | Start one connection attempt and its 10 s virtual ceiling. |
| `open` | none | Complete upgrade, cancel connect ceiling, start 5 s hello/welcome ceiling. Decline permessage-deflate. |
| `send` | `frame: ServerFrame` | Deliver one server frame to the current connection. Welcome cancels handshake ceiling and starts liveness. |
| `expect` | `frame: ClientFrame`, `fields: string[]` | Receive the next client frame. Match `t` and deep JSON equality for the listed top-level fields only. All other fields are ignored. The example remains a complete typed frame even when only some fields matter. Duplicate implicit `t` or nonexistent selections fail. |
| `deadline` | `stage: {kind:"connect"\|"handshake"\|"subscription"\|"command_ack", sub_id? , cmd_id?}` | Advance only the supplied virtual clock to that stage's failure ceiling. Subscription requires sub_id, command_ack requires cmd_id (30 s each). No duration override exists. |
| `heartbeat_missed` | none | After welcome, withhold valid server traffic and advance virtual time to 45 s since its last valid frame. Observe Stale then Reconnecting; this never marks a stream current. |
| `close` | `code`, `reason` | Deliver an explicit WS close. A protocol close is visible and reconnects under capped backoff. |
| `drop` | none | Lose the socket without a close; preserve applied cursors, resync markers and bounded outbox. |
| `token_refused` | `code:4401` or `4403` | Deliver authentication refusal or revocation; enter NeedsToken, holding outbox without a reconnect loop. |

Process operations in file order. A runner rejects an invalid stage, unexpected
client output, mismatch or premature exit immediately. Missing expected output
fails at that stage's supplied-clock ceiling, never hangs. `expect` does not
introduce a sleep, reset a deadline or manufacture success. No socket callback
from an abandoned attempt may deliver frames, renew liveness or advance cursors.
G3 owns execution and lifecycle assertions; S7a only parses the script/types.

Application ping is server to client every 15 s; pong echoes its bounded nonce
immediately. Server liveness requires a valid outstanding pong within 45 s;
client liveness requires any valid server frame within 45 s. Heartbeats and
unrelated traffic never reset a subscription's 30 s progress ceiling.
Command-ack expiry marks just that command uncertain; resend only after reconnect
with the same principal/id/args and `resume_only:true` inside the advertised
window. `session_unconfirmed` keeps `retry:later` and the CLI UNCONFIRMED status;
it does not authorize a new first send. Error details belong under `error.data`.

## Scenarios

- `clean`: hello/welcome and a persisted agent event, ending at synced.
- `connect-deadline`, `handshake-deadline`: no open or no welcome respectively.
- `gap-snapshot`: applied cursor 2, missing 3..5, snapshot 6..7, then synced 7.
- `empty-snapshot`: nonzero high-water 5 remains 5; clear entities immediately.
- `empty-bootstrap`: initial `from:1,to:0,snapshot_to:0`, then synced 0.
- `lagged`: diagnostic skipped_to 9 does not advance applied cursor 2; re-sub after 2, then unsub and close after replay.
- `heartbeat-missed`: authenticated empty stream, ping/pong, then explicit virtual stale fault.
- `protocol-refusal`: ping before welcome is a protocol failure, not auth refusal.
- `version-gap`: newer client gets welcome with read-only capabilities; reading and stale detection continue. This differs from malformed-handshake refusal.
- `token-refused`, `token-revoked`: 4401, or error ack before 4403.
- `uncertain-command`: missing ack, drop, reconnect, resume-only lookup, uncertain outcome.
- `subfail-*`: all four subscription refusal codes, scoped to one subscription.

Snapshot streams use `snapshot_to`; equal to `to` means immediate empty completion.
Bootstrap can have an empty range. History gaps (`agent/<id>`, `events`) omit
snapshot_to. Cursors advance only after reducer application; synced is a captured
head barrier, not snapshot completion. Every seq stream has one barrier per sub;
`agent/<id>/live` omits seq, after and synced. Terminal bootstrap/recovery carries
a full frame, never an isolated delta. S7/G3 enforce these stateful rules; the
fixture format does not claim to implement authorization, reducers or replay.
