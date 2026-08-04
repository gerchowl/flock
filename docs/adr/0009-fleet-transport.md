# ADR 0009 — Fleet transport: one held SSH connection per peer, not a replicated log

- Status: Proposed
- Date: 2026-08-04
- Issues: #224 (the transport spike); PRs #225, #226, #227, #228, #229.
  Supersedes the *transport* half of ADR-0008 — its delivery model (messages
  ride the tool surface) stands unchanged. Constrained by ADR-0001 (fleet
  gossip is pull; no push between servers) and ADR-0005 (the durable event log
  is the audit substrate).
- Decision owner: operator; design from four independent expert reviews on
  #224 plus measurements taken on the live tailnet.

## Context

ADR-0008 shipped agent-to-agent messaging but left its transport dependent on
a **direct SSH edge per sender/recipient pair**. The fleet does not have that
shape: it is hub-and-spoke, the Macs hold the outbound edges, and a spoke has
no edge back. A spoke-to-spoke message had nowhere to go.

The proposal on the table was to **replicate the durable event log** between
nodes and let messages ride it — appealing because the log already exists and
is already the audit substrate.

## What decided it

**Four independent reviews said don't ship it.** The decisive objection was in
flock's own source: `persisted_events_after` carries the docstring *"COLD PATH
— re-reads every log file per call — fine for one-shot queries, wrong for
polling"*, and it was the exact function the design leaned on, at every poll
interval, per peer (~160 MB re-parsed per call). Replication also required
origin-scoped cursors, per-origin dedupe, replica-log integrity, and would put
every message body on every spoke's disk permanently.

**Measurement decided the alternative.** Cold SSH handshake from `sage`,
`ControlMaster=no ControlPath=none`, exit status verified:

| peer | handshake |
| --- | --- |
| anvil | 0.13s |
| anvil-dev | 0.16s |
| ethz-heimdall (Tailscale-remote) | 0.97s |

Five requests: **1.93s** spawning per call versus **0.38s** over one held
connection — linear versus constant.

There is no cheap peer. The floor is ~130ms per call, so per-call spawning is
wasteful fleet-wide rather than only for remote peers.

## Decision

**Hold one SSH connection per peer, carrying API requests to `flk peers
relay`.** Spoke-to-spoke is one pull hop plus one push hop, so gossip stays
pull and ADR-0001 holds.

1. **The relay is a multiplexer, not a byte pump.** The API server is
   one-request-per-connection, so each line gets its own short-lived local
   socket. Those are free; the SSH connection is what gets amortized.
2. **Liveness is not silence.** On an idle fleet nothing flows for long
   stretches, so a no-traffic timer cannot distinguish healthy from dead.
   Death is *observed*: ssh's own `ServerAliveInterval=5 ServerAliveCountMax=2`
   drops a dead or half-open link and exits, arriving as EOF. The 15s timeout
   covers only what ssh cannot see — a wedged relay behind a healthy
   connection.
3. **The one-shot spawn remains, as a working path.** Primarily a
   *compatibility* path: a peer whose `flk` predates the relay can never hold a
   stream, so during any rollout it is the steady state, not an error. This is
   what makes the change never-worse-than-today.
4. **State pushes as a coalesced summary, not raw events.** The hub already has
   exactly one path that applies a summary, so a push needs no second path to
   drift from it — and a snapshot coalesces by nature, so a 1s debounce drops
   nothing the next push does not already carry.
5. **Throttling is structural, not configurable.** State coalesces because it
   is a snapshot; messages pass through because each is discrete. A per-kind
   cadence knob was reviewed and rejected as a footgun.

## Alternatives considered

**Replicate the event log.** Rejected above. Its one lasting contribution: the
review found a real sequence-regression bug (#225) that the design would have
made load-bearing.

**Tune the poll interval.** Rejected on measurement. A 2s cadence is a 6.5%
duty cycle against the nearest peer and ~50% against the furthest; the spread
is 7.5×, so no single value is right and a per-peer knob relocates the problem
onto the operator.

**A message broker (NATS/JetStream, Iggy).** Correct instinct — do not
hand-roll a distributed log — but wrong conclusion for this fleet. It means a
daemon on every host, a second trust domain alongside the SSH host CA, and an
inbound-reachable port, which *is* the topology problem. The right conclusion
was not to build the distributed log at all.

**SSH `ControlMaster`.** Amortizes handshakes for free, and is already in the
operator's `~/.ssh/config`. Not a substitute: its sockets go stale across
exactly what a roaming laptop does — sleep, wifi↔LTE, Tailscale DERP↔direct —
and then hang rather than reconnect. An app-managed connection with an
explicit fallback handles that; borrowing ssh's multiplexing does not.

## Consequences

- Connection lifecycle is new live state: reconnect, backoff, sleep/wake. The
  one-shot fallback keeps every failure benign.
- Failures back off 60s so an asleep peer cannot become a reconnect storm.
- A customized `summary_command` keeps the one-shot path — it is a shell string
  where the connection carries an API request, and the two coincide only at the
  shipped default.
- Cross-host sends now emit `MessageRelayed` (#228), closing a hole where a
  message that left the machine left no audit record at all.
- **Still open:** a relayed message's outcome lives on the receiving node, so
  `msg status` reports `outcome_known: false` for it. Querying the far side is
  now cheap over the held connection and is the obvious follow-up.

## Measurement note

An earlier round of the numbers above included a "0.01s LAN" figure that was
`exit=255` — a DNS failure timed as though it were a handshake — and it reached
a merged commit message before being caught. `/usr/bin/time -p ssh … | grep
real` times a failure just as happily as a success. Check exit status when
timing network calls.
