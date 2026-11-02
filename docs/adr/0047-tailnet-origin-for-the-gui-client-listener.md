# ADR-0047: An opt-in tailnet origin for the GUI client listener, fronted by `tailscale serve`

- Status: Proposed
- Date: 2026-11-02
- Issues: [#2794](https://github.com/gerchowl/flock/issues/2794); related
  [#2476](https://github.com/gerchowl/flock/issues/2476) (startup without
  Tailscale) and [#2293](https://github.com/gerchowl/flock/issues/2293)
  (remote code exchange, multi-node GUI).
- Decision owner: repository owner. The owner asked for this ADR on 2026-11-02.
  The four recommendations (R1–R4) and the open questions at the end are
  pending the owner's decision.
- Builds on: ADR-0036 §5 (client wire, tokens, one-time codes), ADR-0038 §6
  (browser shell, shell origins, identity), ADR-0043 §3 and §8 (remote ingress
  and its threat model).
- Amends, if accepted: ADR-0043 §3's sentence "Do not expose the current
  loopback-only one-time-code exchange remotely as an unreviewed shortcut".
  This ADR is that review, and it covers only the `tailscale serve` front end
  defined here. It does not change ADR-0043's own TLS listener (T1–T3).
- Implemented: none. The baseline below describes the code at `origin/dev`
  `5f7f4f0ac144`. All line numbers refer to that commit.

## Context

The owner wants to use the browser GUI from another Mac on the same tailnet
(2026-10-30). The structured-client listener refuses this on purpose. A probe
against `flock-web` 3.1.0 set Host and Origin to `https://<name>.ts.net` and got
403 for `GET /`, for the `/v1/ws` upgrade and for `POST /v1/client/exchange`.
The same build signs in and reaches Live over loopback.

ADR-0043 settled remote GUI access as **TLS straight to a home server**: the flk
server terminates TLS with operator-supplied certificates. That design is
accepted but not built, and it needs T1–T3 plus the gateway slices. A single
tailnet Mac reaching its own server needs none of the mesh work. `tailscale
serve` already gives that Mac a valid HTTPS origin (`https://<name>.ts.net`), and
tailscaled terminates TLS with a ts.net certificate and proxies to a loopback
port. The question is whether flk can admit that one proxied origin without
weakening the loopback rules every other path relies on.

### Inspected baseline

**Listener bind.** `Listener::prepare_with_funnel` in
`src/web/client_listener.rs:178-184` refuses any bind other than
`127.0.0.1` (`require_loopback`, `src/platform/client_listener.rs:105-113`,
and a second explicit `Ipv4Addr::LOCALHOST` check). The same check runs again on
the actual address (`:189-199`), and also when a live handoff passes in the
socket. The GUI router refuses a non-loopback bind before it builds any route
(`src/web/gui.rs:41-46`).

**Guard.** `guard` (`src/web/client_listener.rs:267-291`) returns 403 unless
the TCP peer is `127.0.0.1`, and either `gui::origin_allowed` passes or the path
is `/v1/ws` and `gui::shell_origin_allowed` passes. Any `/v1/` path without an
`Origin` header is also refused (`:278`). Every response that is not the entry
document gets `RESOURCE_POLICY` (`:286-289`, `src/web/gui.rs:139-140`).

**Origin rule.** `origin_allowed` (`src/web/gui.rs:65-89`) requires exactly one
`Host` equal to the bound numeric authority (with `:80` normalized),
`Sec-Fetch-Site` either absent or `none`/`same-origin` (`:76-80`), at most one
`Origin`, and that header, if present, equal to `http://` plus the authority.
Forwarding headers play no part. `shell_origin_allowed` (`:92-108`) admits only
`Origin: tauri://localhost` with `Sec-Fetch-Site: cross-site`, on `/v1/ws`.

**Exchange.** `client_exchange::router` (`src/web/client_exchange.rs:16-27`)
derives its one origin from `client_open::endpoint`, which accepts only
`127.0.0.1` with a nonzero port (`src/client_open.rs:15-18`). `exchange`
(`src/web/client_exchange.rs:28-82`) repeats the loopback-peer check and
requires exactly one Host and exactly one Origin, both exact. `Sec-Fetch-Site`,
if present, must be `same-origin` (`:36-52`). The reply is `no-store`
(`:81`).

**CSP.** The entry document's policy is
`src/web/gui/document-policy.txt`. Its `connect-src` is
`'self' ws://LISTENER/v1/ws`, and `document_policy`
(`src/web/gui.rs:149-157`) fills in the bound address once, at router
construction, and never from request headers. It is set only on `index.html`
(`:237-239`).

**Startup forwarding check.** `check_funnel_with`
(`src/web/client_listener.rs:317-341`) runs `tailscale serve status --json`
(bounded at 5 s and 64 KiB, `:38-41`, `:306-316`). Its three outcomes (#2476):
`NotInstalled` lets startup continue and logs one INFO record (`:334-337`),
`Status` is validated, and `Unavailable` refuses startup. `validate_status`
(`:357-370`) refuses startup with `PermissionDenied` if any `AllowFunnel` entry
for the port is true, or if `targets_port` (`:388-402`) finds any `Proxy`,
`TCPForward` or `TCP` entry targeting the port anywhere in the document,
foreground and service configurations included. An unparsable target counts as
a match (`is_none_or`). So today, **any** serve rule pointing at the client port
aborts startup. The check runs once. `docs/next/structured-client-transport.md`
(lines 221-222) says later changes are not monitored.

**Configuration.** `ClientConfig` (`src/config/model.rs:497-503`) has one
field, `bind`, with `deny_unknown_fields`.

**Code issuance.** `flk client open [--print]` (`src/cli/client.rs:14-58`) sends
`Method::ClientOpen` over the unix socket. That method is operator-only: a
caller that is not `SpawnCaller::Operator` gets `OperatorOnly`
(`src/app/api.rs:1481-1496`). The handler
(`src/app/api/client_tokens.rs:81-117`) builds the endpoint from
`client_open::endpoint` and loads `store.operator_token(&authority)`. It mints 32
random bytes as the code, stores them in `Codes` (`src/client_open.rs:39-48`;
128 live codes at most, a 60 s lifetime, SHA-256-keyed, single use at `:53-62`),
and returns `http://127.0.0.1:<port>/#code=<code>`. `operator_token`
(`src/structured/permissions/auth/tokens.rs:302-318`) reuses **one** durable
record named `operator` with `operate` scopes. Every browser that exchanges a
code receives that same bearer.

**Identity in `flk web`.** The terminal bridge's `ws_handler`
(`src/web/mod.rs:266-292`) reads `Tailscale-User-Login` straight from the
request headers and checks it against `allowed_users` with `identity_allowed`
(`:623-628`). It does not check where the request came from. `flk web` can bind
off loopback with `--allow-non-loopback` (`:126`), and then any tailnet peer can
send that header itself. ADR-0038 §6 has already **superseded** this header as
the GUI's principal: on `/v1/ws` the principal is the client token, on any
network. It keeps `tailscale serve` only as optional defence in depth.

### What tailscaled does to a proxied request

From tailscaled's serve proxy (`ipn/ipnlocal/serve.go`, `tailscale/tailscale`
main, read 2026-11-02). R1's acceptance tests re-check this against the pinned
Tailscale version, because the design depends on it.

- `addTailscaleIdentityHeaders` **deletes** any client-supplied
  `Tailscale-User-Login`, `Tailscale-User-Name`, `Tailscale-User-Profile-Pic`,
  `Tailscale-Funnel-Request` and `Tailscale-Headers-Info` before it sets its own.
- A Funnel (public internet) request gets `Tailscale-Funnel-Request: ?1` and no
  identity.
- The identity comes from `WhoIs("tcp", SrcAddr)` on the tailnet source address.
  A tagged node gets **no** identity headers. If WhoIs fails, which happens for
  Funnel or local-machine traffic, no identity is set.
- For a TCP backend, `Out.Host = In.Host`, so the browser's Host
  (`<name>.ts.net`) reaches flk. tailscaled also sets `X-Forwarded-Host`, sets
  `X-Forwarded-Proto: https` only when the inbound leg was TLS (`serve.go`
  1146-1148), and sets `X-Forwarded-For` to the tailnet source address.
- tailscaled itself never deletes a client-supplied `X-Forwarded-Proto`. Go's
  `httputil.ReverseProxy` does: when `Rewrite` is set, it removes `Forwarded`,
  `X-Forwarded-For`, `X-Forwarded-Host` and `X-Forwarded-Proto` from the
  outbound request before `Rewrite` runs (`net/http/httputil/reverseproxy.go`
  128-131 for the contract, 521-524 for the deletion). On a plain `--http`
  handler, `X-Forwarded-Proto` is therefore absent and not client-controlled.
  This depends on Go's proxy behavior, not on tailscaled's own code, so A18
  pins it.
- For a `unix:` backend, `Out.Host` is rewritten to the backend URL's host.
- Raw TCP forwarding (`serve --tcp`, `--tls-terminated-tcp`) is not HTTP, so
  none of the above happens. The remote client controls every byte, including
  any `Tailscale-User-Login` line.

These headers are therefore trustworthy only on a request that **tailscaled's
HTTP proxy itself** delivered. flk's TCP peer will always be `127.0.0.1`, and
that does not prove the proxy sent the request: any local process can open the
same loopback socket and write any header. `tailscale whois` does not help.
flk would need the tailnet source address for it, and that address arrives only
in `X-Forwarded-For`, which is as forgeable from loopback as the identity header.

## Threat model

Assets, in order: the `operate` bearer (ADR-0036 §5: it is the operator's
identity for ADR-0014's operator-only verbs, from any address), live one-time
codes, session content (transcripts, questions, artifacts, which the browser
also keeps in IndexedDB, ADR-0038 §3), and availability of the listener.

Trust boundary, as ADR-0036 §5 and ADR-0043 §8 state it:

- **Inside:** the operator's uid on the server host, and tailscaled (root, or
  the system extension), which already terminates the tailnet's TLS and could
  rewrite any proxied byte.
- **Outside, and not trusted:**
  - other local uids, including `flk-agent` (ADR-0035 §6.3) and pane-started
    processes. They can reach `127.0.0.1:<port>` and forge Host, Origin and the
    Tailscale headers;
  - `pty` agents that still run as the operator's uid (#1177). The socket
    classifies an operator-uid caller as the operator when its process ancestry
    reaches no agent pane: `spawn_caller`, `src/app/api/spawn.rs:473-522`. A
    re-parented or daemonized caller leaves no ancestry evidence
    (`src/app/ids.rs:175-178`). So such an agent can detach a child and call
    operator-only verbs, including `server.live_handoff` and `client.open`.
    That is an existing residual, and this ADR does not widen it: such a process
    can already mint codes directly;
  - web pages in any browser, on any machine;
  - other tailnet nodes and users, including shared-in nodes, tagged nodes and
    other people in a multi-user tailnet;
  - the public internet through Funnel;
  - whoever sees a printed URL.

The adversaries come from ADR-0043 §8, plus four this front end adds:

- a tailnet peer that can reach `<name>.ts.net:443` under the tailnet ACLs;
- a raw TCP forward or Funnel rule that the operator turns on later, which
  silently replaces the proxy's header stripping;
- any relay to `127.0.0.1:<port>` that is not Tailscale's (`socat`, Caddy,
  `cloudflared`, `ssh -R`, or a process behind another serve port that forwards
  here). No probe can see one, and each makes every header client-controlled
  (R2);
- another backend mounted under the same ts.net origin.

Invariants this ADR must keep:

1. **The listener stays loopback-only.** No non-loopback bind, no
   `--allow-non-loopback` on this listener (ADR-0043 §3: never a plaintext
   non-loopback listener).
2. **The bearer is the only principal.** Origin and identity checks gate
   *admission*. They never authenticate or grant scopes (ADR-0038 §6).
3. **Code issuance stays LocalOperator-only, over the unix socket.** No network
   path mints a code.
4. **Loopback admission is unchanged** for every request whose Host is the
   numeric loopback authority, and so are all existing tests.
5. **Fail closed** whenever flk cannot prove the `tailscale serve` shape this
   ADR requires.

## Decision

### R1. Opt-in `[client] public_origin`, admitted only behind a verified HTTPS `serve` proxy

**Recommendation.** Add two fields to `ClientConfig`:

```toml
[client]
bind          = "127.0.0.1:7699"            # required, nonzero port
public_origin = "https://<name>.ts.net"      # opt-in; absent = today's behavior
public_users  = ["owner@example.com"]        # required when public_origin is set (R2)
```

**Configuration validation** happens at load, and an invalid configuration
refuses startup:

- `public_origin` must be scheme `https`, and its host must be an
  ASCII-lowercase DNS name ending in `.ts.net`, with no trailing dot, userinfo,
  path other than empty or `/`, query or fragment.
- The port is optional. Port 443 is normalized away, and any other port is kept
  as given.
- `bind` must be set, with a nonzero port (`client_open::endpoint` already
  refuses port 0).
- The `public_origin` port, 443 when omitted, must differ from the `bind`
  port. If they were equal, a page on any origin that resolves
  `<name>.ts.net` to `127.0.0.1` (for example a split-DNS or hosts-file entry on
  the server host, or a rebinding resolver) would reach the listener directly
  over plain HTTP with `Host: <name>.ts.net:<port>`. That is the public
  authority, without tailscaled in the path. With different ports, that Host
  can only mean the serve listener.
- Both fields are restart-only, like `bind`. A config reload that changes them
  is refused with a diagnostic that names the restart.

**Two authorities, selected by exact equality.** The guard computes a request's
authority class from its single `Host` header. It compares the header with two
configured strings, never parsing it into anything looser:

- `Host == <bind>`, or the `:80`-normalized form: **loopback class**. Every
  current rule applies unchanged (`origin_allowed`, `shell_origin_allowed`,
  `exchange`).
- `Host == <public authority>` (`<name>.ts.net`, or `<name>.ts.net:<port>`
  when the port is not 443): **public class**. Only the rules below apply.
- Anything else: 403, as today.

**Public-class rules.** All of these must hold, in addition to the existing
loopback-peer check (`peer == 127.0.0.1`, which stays):

| Header | Rule |
|---|---|
| `Host` | exactly one, equal to the public authority |
| `Origin` | exactly one on every `/v1/` path, equal to `https://<public authority>`. On a non-`/v1/` GET (the document and assets), absent or equal to that value. `http://`, `null`, a `tauri://` shell origin and any other value are refused. Shell origins are never admitted on the public class. |
| `Sec-Fetch-Site` | `/v1/client/exchange`: exactly one, `same-origin`. Non-`/v1/` GET: absent, `none` or `same-origin`. `/v1/ws`: absent or `same-origin`. `same-site`, `cross-site` and duplicates are always refused. |
| `X-Forwarded-Proto` | exactly one, `https`. Behind tailscaled the client cannot supply it, because Go's `ReverseProxy` deletes it before tailscaled's `Rewrite` runs, and tailscaled sets it only for a TLS inbound leg (see "What tailscaled does", above). So it separates the TLS serve listener from a plain `--http` handler. Like every header here, it is forgeable by any other relay (R2). |
| `Tailscale-Funnel-Request` | must be absent |
| `Tailscale-User-Login` | exactly one, in `public_users` (R2) |

**CSP.** The listener precomputes two entry-document policies at router
construction, both from configuration and neither from request headers. The
public one replaces `ws://LISTENER/v1/ws` with
`wss://<public authority>/v1/ws`. It never also lists the loopback `ws://`
source, and the loopback policy never lists the `wss://` source. `serve` picks
the policy by the authority class the guard computed. `RESOURCE_POLICY` stays
on every other response. The browser client already derives `wss:` from an
`https:` page (`web/client/src/config.ts:13`), so it needs no change.

**`Sec-Fetch-Site` on `/v1/ws`** stays "absent or `same-origin`" until the
capture test (A13) records what Safari, Chrome and Firefox actually send on a
`wss:` upgrade from the ts.net page, in a table like ADR-0038 §6's. If every
target browser sends it, a follow-up tightens the rule to "exactly one".

**Startup check: serve yes, funnel never, nothing else.** With `public_origin`
set, the forwarding probe changes from "no rule may target the port" to "exactly
one rule targets the port, in exactly this shape". It refuses startup unless
**all** of the following hold:

1. The probe outcome is `Status`. `NotInstalled` and `Unavailable` both refuse:
   flk cannot verify a front end that it cannot see (see R4).
2. `Web["<name>.ts.net:<port>"]` exists. Its `Handlers` map has exactly one
   entry, path `/`, whose only field is `Proxy`, and `Proxy` is exactly
   `http://127.0.0.1:<bind port>` (the bare `127.0.0.1:<port>` spelling is also
   accepted). `localhost`, `[::1]`, `https+insecure://`, `unix:`, `Text` and
   `Path` handlers are all refused.
3. `TCP["<port>"]` for that serve port has `HTTPS: true`, with no `TCPForward`,
   `TerminateTLS` or `HTTP`.
4. `AllowFunnel` is false or absent for that host:port, **and** for every other
   host:port whose handlers target the client port.
5. No other entry anywhere in the document targets the client port: not a
   second host:port, not a foreground configuration, not a service, not a raw
   TCP forward, not an HTTP-only handler. This is today's `targets_port`
   traversal, minus the single allowed path. An unparsable target still counts
   as a match.

Without `public_origin`, today's refusal of every rule is unchanged.

**The check is repeated while running.** `serve` configuration can change
underneath a running server, and a later `serve --tcp` or `funnel` on the same
port would replace header stripping with attacker-controlled bytes. With
`public_origin` set, the same bounded probe runs:

- off the executor, every 60 s;
- immediately before every public-class code exchange.

A positive bad-configuration finding (defined below) **latches public
admission closed** for the life of the process. Public-class requests then get 403
`public_origin_unverified`, and public-class `/v1/ws` sessions already upgraded
are closed with 4403. The server logs one WARN naming the failed condition
without the configuration body. The listener's `Health`
(`src/api/client_listener.rs:12`, which holds a `Status` from `:6-9`, today
`Listening` or `Failed`) gains a public-class state so that server status
reports the latch. Loopback admission continues. Only a restart re-opens a
latched class. A live handoff counts as a restart (below).

**What latches, and what only closes.** Only a **positive bad-configuration
finding** latches: a probe that ran, parsed, and shows a `serve` configuration
failing conditions 2–5. Every other non-passing outcome only **closes** the
class until a probe passes again, through the freshness rule below:

- a timeout (the 5 s deadline);
- `Unavailable` (the CLI ran and failed, or its output did not parse);
- `NotInstalled` at runtime (the CLI vanished, as it can for a moment during a
  Tailscale upgrade).

A tailscaled restart or upgrade shows up as one of these, and recovery then
needs no flk restart. This is still fail closed: no public request is admitted
without a passing probe younger than 90 s. Latching only on positive evidence
also means that anyone who can slow the host or disturb tailscaled cannot turn
one bad `tailscale` call into a permanent outage. At **startup**, all three
outcomes still refuse (condition 1). A process that has never seen a passing
probe has nothing to re-open.

**Freshness: the guard fails closed when the monitor dies.** The guard admits a
public-class request only while the last *passing* probe is younger than 90 s,
on the monotonic clock. That is one 60 s period plus the 5 s probe deadline plus
margin. The guard compares that timestamp itself and does not trust the
monitor's liveness. A monitor task that panics, is aborted, or stalls behind a
blocked `spawn_blocking` pool stops refreshing the timestamp, so the public
class closes within 90 s with 403 `public_origin_unverified`. Upgraded public
sockets are closed with 4403 on the same edge. This is a close, not a latch: a
later passing probe re-opens the class. A monitor that exits restarts once,
with a WARN, and the freshness rule covers the gap.

**Exchange probes cannot be used for denial of service.**
- The per-exchange probe runs only **after** every header check has passed:
  public class, Origin, `Sec-Fetch-Site`, `X-Forwarded-Proto`, no Funnel header,
  an allow-listed `Tailscale-User-Login` and the body limit. A request that
  fails any of these never spawns a `tailscale` process.
- At most **one** probe is in flight. The 60 s monitor shares that slot.
- **An exchange joins only a probe that started after the exchange arrived.**
  If the probe in flight started earlier, which might be the monitor's, the
  exchange does not use its result: that probe may predate a `funnel` or
  `--tcp` change. The exchange waits for it to finish, and then joins the next
  probe, which starts as soon as the slot is free. All exchanges that arrive
  during one probe share the same next probe. So N concurrent exchanges cost at
  most two subprocesses, and the latency an exchange can add is bounded by two
  probe deadlines (10 s).
- Every redeemed code was therefore checked against a `serve` configuration
  read **after** the request arrived. A change made between that read and the
  code consumption, a window of milliseconds, is not seen. Every check-then-use
  has that window.
- A timeout refuses that exchange with a retryable 503 and does not consume the
  code. It does not latch (above).

**A live handoff counts as a restart, and resets the latch on purpose.** The
importer always runs the full startup probe on the inherited socket
(`client_listener.rs:200`, which runs for adopted and freshly bound sockets
alike) with every R1 condition. It starts with a fresh passing timestamp, or
with a refused import, and the old process then keeps serving with its class
still latched. The latch state does not cross the handoff. That is intended:

- a restart is the only documented way to re-open the class;
- the evidence it needs is exactly one fresh, full, passing probe, which the
  importer produces;
- carrying the latch would add a handoff field whose only effect is to make a
  handoff stricter than the restart it stands in for.

The latch exists so that a running process never re-opens on its own. It is not
meant to outlive a process change that has the operator's authority.

**Who can trigger a handoff.** The CLI `flk server live-handoff`
(`src/cli/server.rs:14`, request at `:103`) and `flk update`
(`src/update.rs:1394-1410`) both send `ServerLiveHandoff` over the unix socket.
The headless server sends it through `peer_uid_refusal` with the request's
peer credentials (`src/server/headless.rs:3051-3073`). That function returns
`OperatorOnly` unless `spawn_caller` is `SpawnCaller::Operator`
(`src/app/api.rs:1481-1496`).

- Refused: `flk-agent`-uid callers, structured-lease agents, and callers whose
  ancestry reaches an agent pane.
- Admitted: the operator, and an operator-uid `pty` agent's detached child (the
  #1177 residual in the threat model).

Such a process gains nothing from the reset that it does not already have.

- A still-bad configuration fails the importer's probe, so the reset cannot
  re-open an unsafe class.
- A good configuration re-opens it on fresh evidence, which is correct.
- The same process can already call `client.open` and mint a code without any
  handoff.

An operator who has fixed the serve configuration can re-open the class with a
handoff or a restart.

**Availability cost: a bad serve configuration blocks every handoff.** With
`public_origin` set, an importer whose startup probe fails is refused. Every
live handoff and every `flk update` then fails while the serve configuration
is bad, or while tailscaled is down long enough for the importer's probe to
time out. The old process keeps serving, loopback included, so nothing goes
dark. Upgrades are coupled to Tailscale health, and that coupling is the price
of never starting an unverified public class. To recover, do one of the
following and retry the handoff or update:

- fix the `serve` rule (`tailscale serve status` shows it), and wait for
  tailscaled to answer;
- remove `public_origin` from `[client]`. Without it, the importer runs
  today's probe, under which no rule may target the port. So if a rule still
  targets the port, remove that rule too.

**Rejected alternatives**

- *Trust `X-Forwarded-Host` instead of Host.* Rejected because tailscaled
  already preserves Host for TCP backends. A second header is one more forgeable
  input, and the guard would need a precedence rule.
- *A configurable list of public origins, or a wildcard `*.ts.net`.* Rejected
  because each extra origin is another backend whose same-origin script could
  read this origin's `sessionStorage`. One exact origin is enough for the stated
  need.
- *Accept any `serve` rule and rely on the bearer.* Rejected because it lets a
  raw TCP forward or Funnel through, which makes R2's identity check forgeable,
  and it gives up invariant 5.
- *A startup-only check, as today.* Rejected because the documented gap
  ("forwarding changes after startup are not monitored") is exactly how a later
  `funnel` exposes the public class.
- *flk terminates TLS itself on the tailnet address.* That is ADR-0043 T1–T3,
  already accepted and still the long-term path. This ADR does not replace it.
  It covers the single-node case until T1–T3 ship, and it can be withdrawn once
  they do.
- *Run `tailscale serve` to a `unix:` socket, so flk could check the kernel
  peer credential of tailscaled.* That would prove the request came from
  tailscaled, which loopback TCP cannot. Rejected for now: `unix:` targets are
  recent and version-dependent, macOS tailscaled variants run under different
  identities (system extension, App Store sandbox), and the guard would need a
  second socket type in the handoff path. It is recorded as the upgrade path if
  local header forgery ever needs to be closed (open question 3).

### R2. A tailnet identity allow-list is **required**, as an admission precondition, never as a principal

**Recommendation.** When `public_origin` is set, `public_users` must be a
non-empty list of tailnet logins. Configuration validation refuses an empty
list. Each entry must be ASCII, contain `@`, contain no `=?` (tailscaled
RFC 2047-encodes non-ASCII values) and no whitespace, and is compared
ASCII-case-insensitively, as in `flk web`. Every public-class request, including
the document `GET`, must carry **exactly one** `Tailscale-User-Login` in the
list. A missing header refuses the request, which also refuses tagged nodes and
Funnel traffic. Loopback-class requests ignore the header entirely. The
principal is still the bearer.

**Where the header is trustworthy, stated strictly:**

- It is **unforgeable by a remote tailnet peer** only when all of these hold:
  - the listener is bound to `127.0.0.1` (invariant 1), so a remote machine's
    bytes reach flk only through a local relay;
  - the only relay to `127.0.0.1:<port>` is tailscaled's HTTP proxy, which
    deletes client-supplied `Tailscale-*` headers before it sets its own;
  - no raw TCP forward exists.

  **flk cannot verify the second condition.** R1's probe sees only Tailscale's
  own rules. Any other relay to `127.0.0.1:<port>` gives a remote client control
  of Host, `X-Forwarded-Proto` and `Tailscale-User-Login`, and nothing in
  `serve status` shows it. Examples: `socat`, Caddy or another reverse proxy,
  `cloudflared`, an `ssh -R` from another machine, or a `serve` rule that targets
  a different local port whose process forwards here. The identity check is
  therefore exactly as strong as the operator's assurance that tailscaled is the
  only relay. It narrows who can try. It is not a proof. **The bearer remains
  the real gate**, as it already is for the loopback class today: a code or the
  bearer is still needed, and neither can be obtained over the network.
- It is **forgeable by every local process**, at all times, because flk cannot
  tell tailscaled's loopback connection from anyone else's. That includes
  agents, and the operator's own SSH `-L` forwards from other machines. This
  ADR does not try to close that gap. Local processes are already outside
  loopback's trust and are kept out by the bearer, not by headers (ADR-0036 §5,
  "local processes can forge Host/Origin"). A forged identity gets a local
  process no further than a real one: it still needs a live code or the bearer.
- `tailscale whois` is not used. flk only ever sees `127.0.0.1` as the peer,
  and the tailnet address exists only in `X-Forwarded-For`, which is exactly as
  trustworthy as the header itself.

**Why required and not optional.** In this topology the allow-list is what
turns "anyone the tailnet ACLs let reach port 443 on this node" into "the
operator's own logins". That matters in four ways:

- **Codes are bearer secrets.** A code that leaks in its 60 seconds (pre-mortem
  P6) is useless to a *remote* tailnet peer outside `public_users`, while
  tailscaled is the only relay. It is **not** useless to a local process (P6).
- **Shared tailnets.** Shared-in nodes, family or organization tailnets and
  tagged service nodes become refused by default and not merely unauthenticated.
- **Exposure.** An unauthenticated tailnet peer cannot load the bundle, so it
  cannot fingerprint the version or run the pre-auth budget against the
  operator's own tab (ADR-0038 §6 admission policy).
- **Consistency.** It is the same defence in depth that ADR-0038 §6 left
  available, applied as a precondition and not as a principal, so it does not
  reopen that supersession.

**Rejected alternatives**

- *Bearer only, no allow-list.* Rejected because a leaked code or bearer is then
  usable by any tailnet peer the ACLs admit, and the operator does not control
  every ACL in a shared tailnet.
- *An optional allow-list, empty by default, as in `flk web`.* Rejected because
  the default would be the weak configuration, and `flk web`'s own reading of
  the header shows the cost: it has no provenance check, so with
  `--allow-non-loopback` any peer forges it. That is a separate defect, filed as
  a follow-up and not changed here.
- *Use the identity as the principal and skip the code.* Rejected because it
  reverses ADR-0038 §6's supersession and makes a forgeable-locally header an
  authority.
- *Bind each code to one login (`--for <login>`).* Rejected for v1: with a
  required allow-list of the operator's own logins, per-code binding adds a flag
  for little gain. It is open question 2.

### R3. A remote browser gets a code from `flk client open --print --origin public`

**Recommendation.**

- **Issuance stays local.** `flk client open --print --origin public` sends
  `ClientOpen` with a new `origin: "public"` parameter over the unix socket. The
  same `OperatorOnly` gate applies (`src/app/api.rs:1481-1496`). The server
  refuses with `invalid_params` when `public_origin` is not configured, or when
  the public class is latched closed (R1). There is no network route to mint
  codes, and `/v1/client/exchange` stays the only network surface.
- **The code is bound to an audience.** `Codes` stores
  `(expiry, token, audience)`, where audience is `Loopback` or `Public`. A
  public-class exchange consumes only a `Public` code, and a loopback-class
  exchange consumes only a `Loopback` code. A mismatch is the same uniform 401
  as unknown or expired, and it **consumes** the code, so a code offered on the
  wrong audience cannot be retried on the right one. Lifetime stays 60 s,
  single use, at most 128 live codes in total, SHA-256-keyed, as today.
- **A separate, separately revocable bearer.** A public code is bound to a
  durable token record named `operator-public` with `operator_scopes()`. It is
  issued on first use and reused afterwards, with the same reuse discipline as
  `operator_token` (`tokens.rs:302-318`). `flk client token rm
  operator-public` cuts off every remote browser without touching the local
  operator's `operator` bearer. The remote tab keeps it in `sessionStorage`
  under ADR-0038 §3's rules.
- **The two names are reserved.** Today `TokenStore::issue`
  (`tokens.rs:283-300`) accepts any name, and `operator_token` reuses whatever
  record named `operator` has the operator scopes. So a record that `flk
  client token add operator-public --scopes read,send,answer,operate` created
  earlier would be handed to every remote browser, and its bearer was already
  printed to whoever ran `token add`. W3 therefore:
  - makes `issue`, the `token add` path, refuse `operator` and `operator-public`
    with `invalid_params`;
  - mints them only through a private reserved-name path used by
    `operator_token` and its public twin;
  - has that path write a `reserved: true` marker into the record, and makes the
    public twin refuse to reuse an `operator-public` record without the marker
    (`TokenError::Unsafe`, with a diagnostic naming `flk client token rm
    operator-public`).

  Existing `operator` records predate the marker, so they stay reusable, and
  the reservation applies to them from now on.
- **`--origin public` requires `--print`.** Opening a browser on the server's
  own host at the public URL is not a remote use, and it would put the code in
  the opener's argv (ADR-0036 §5's argv exception) for no benefit.
- **The output goes to a terminal only.** As with `token add`, the command
  refuses when stdout is not a terminal, unless `--json` is given. `ssh -t
  <server-host> flk client open --print --origin public` therefore works, and
  `ssh <server-host> flk … > file` does not write a code to disk by accident. It
  prints `https://<public authority>/#code=<64 hex>` and nothing else to
  stdout. The code stays in the fragment, and so never reaches tailscaled's
  request line, a `Referer` or a log.
- **Exchange checks on the public class.** These are R1's public-class rules,
  plus the R2 identity, plus the R1 pre-exchange probe, plus the audience check
  above. The response is still `no-store` with one uniform 401.

**Rejected alternatives**

- *Reuse the `operator` bearer for remote tabs.* That is what today's handler
  does. Rejected because the only way to revoke a lost Mac's tab would also
  revoke the operator's local browser, and nothing would tell the two
  exposures apart.
- *An in-memory, TTL-bound token, like app-session tokens.* Rejected:
  app-session bearers are bound to a minting PID and refused on TCP by owner
  decision 18 (ADR-0038 §6), and every restart or live handoff would push the
  remote tab back to NeedsToken. A follow-up may add a TTL to durable tokens
  (open question 4).
- *Paste a long-lived token into the remote browser.* Rejected because it puts
  the bearer itself on the clipboard and in scrollback, where a code expires in
  60 s.
- *A remote "request a code" endpoint that the local operator approves.*
  Rejected because it adds a pre-auth network verb and an approval UI. That is
  #2293's remote-exchange design space, and this ADR does not open it.
- *A longer TTL for remote codes.* Rejected, since copying a URL between two
  Macs fits in 60 s. Open question 1 asks whether 60 s is too short in practice.

### R4. VM-guest servers behind a host's tailscaled: **unsupported**

**Recommendation.** A server whose tailscaled runs on a VM host, while flk runs
in the guest, cannot use `public_origin`.

- A non-loopback bind stays refused, so invariant 1 holds.
- With `public_origin` set, a guest whose probe returns `NotInstalled` or
  `Unavailable` refuses startup, as R1 condition 1 requires.

The supported options are:

- run tailscaled **in the guest**, which makes it its own tailnet node with its
  own `<guest>.ts.net`, and use R1 there;
- use the SSH local forward (Interim, below).

**Why.**

- R1's checks and R2's trust argument both need flk to observe the serve
  configuration that actually fronts its port. From the guest it cannot: the
  host's `serve status` is on another kernel.
- A non-loopback bind on the guest bridge would admit every process on the VM
  host and every other guest on that network as a peer. Each of them could
  forge `Tailscale-User-Login`, and none of them would show in any probe.
- A host-side hop (`socat`, or `ssh -L` from the VM host into the guest) is
  invisible to the guest too. It keeps the guest bound to loopback, but flk
  would admit a public class whose front end it has never verified.
- Supporting either option would mean trusting operator assertions in place of
  evidence, which is what invariant 5 rules out.

**Rejected alternatives**

- *`bind = "<guest bridge address>"` with an `allow_non_loopback` switch.*
  Rejected for the reasons above, and because it contradicts ADR-0043 §3's "never
  a plaintext non-loopback listener, even on a private network".
- *A `public_origin_unverified = true` escape hatch for host-side hops.*
  Rejected because it turns R1 and R2 into configuration claims. If the owner
  wants VM guests, the right shape is a probe that runs on the VM host and
  reports a signed or socket-attested result to the guest. That is out of scope
  here (open question 5).

### Interim, with no code change

The SSH local forward keeps Host and Origin at the numeric loopback authority,
so every current check passes unchanged:

```sh
ssh -L 7699:127.0.0.1:7699 <server-host>
# on the server host, in that session or another:
flk client open --print
# open the printed http://127.0.0.1:7699/#code=… in the local browser within 60 s
```

It works once the server is web-enabled with `[client] bind =
"127.0.0.1:7699"` and #2476 is fixed (it is, in v3.2.0). This path stays
documented and supported after R1 ships. It is also the only path for R4's VM
guests that do not run their own tailscaled. Note that `ssh -L` makes remote
bytes arrive as a loopback peer. That is acceptable only because the SSH
session itself is operator-authenticated. It is also why invariant 2 never lets
a loopback peer stand in for authority.

## Pre-mortem

Assume R1–R3 shipped and something went wrong. Each entry gives the failure and
the control that answers it.

- **P1. DNS rebinding.**
  - *Attack:* a page on `evil.example` rebinds to `127.0.0.1` or to the node's
    `100.x` address and drives the listener.
  - *Control:*
    - Against `127.0.0.1`, the browser sends `Host: evil.example`, which matches
      neither authority class, so 403.
    - Against `100.x:443`, the browser either fails TLS because the ts.net
      certificate does not match `evil.example`, or reaches a plain-HTTP serve
      handler. R1 condition 3 and the `X-Forwarded-Proto` rule refuse the
      latter.
    - The Host comparison is exact equality with configured strings, never a
      suffix match.
    - If `<name>.ts.net` resolves to `127.0.0.1` on the server host (split DNS,
      a hosts-file entry or a rebinding resolver), a browser there would reach
      the listener directly on the bind port. Refusing a `public_origin` port
      equal to the `bind` port (R1) means that a Host carrying the public
      authority can never arrive on the bind port without tailscaled in the
      path.
  - *Test:* A3, A8, A22.
- **P2. Header spoofing from a tailnet peer.**
  - *Attack:* the operator later runs `tailscale serve --tcp 443
    tcp://127.0.0.1:7699`, or `funnel`, on the port, and a peer then sends its
    own `Tailscale-User-Login` and `X-Forwarded-Proto`.
  - *Control:* the running check (60 s, and before every exchange) latches the
    public class closed and closes public sockets with 4403. The window is at
    most 60 s for already-upgraded sockets. For code exchange there is no
    window relative to the request: an exchange is redeemed only on a probe
    that started after it arrived (R1). The only gap is the milliseconds
    between that probe's read and the code consumption. If the
    monitor itself has died, the 90 s freshness rule closes the class anyway.
    A relay that is not Tailscale's (`socat`, Caddy, `cloudflared`, `ssh -R`)
    is invisible to every probe. Against that, only the bearer gate holds
    (R2).
  - *Residual:* within that window a forged identity still needs a code or the
    `operator-public` bearer.
  - *Test:* A10, A11.
- **P3. Header spoofing from a local process.**
  - *Attack:* an agent forges every public-class header over loopback.
  - *Control:* none at the header layer, by design (R2). The agent gets the
    bundle and a 401 at exchange, and nothing more than it gets from the
    loopback class today. Codes are minted only for the LocalOperator, over the
    unix socket.
  - *Residual:* documented, including local redemption of a seen code (P6). The `unix:` backend upgrade path is open question 3.
- **P4. CSRF or CSWSH from another tailnet origin.**
  - *Attack:* a page served at `<other>.<tailnet>.ts.net` issues `fetch` or
    `WebSocket` calls to `<name>.<tailnet>.ts.net`. `ts.net` is on the Public
    Suffix List, so the registrable domain is `<tailnet>.ts.net`, and sibling
    nodes in one tailnet are **same-site** with this one. Same-site is exactly
    the case where a browser would attach `SameSite` cookies and send
    `Sec-Fetch-Site: same-site`.
  - *Control:*
    - `Origin` must equal the one public origin on every `/v1/` path.
    - R1 refuses `Sec-Fetch-Site: same-site` on every path. Because siblings
      are same-site, that rule is what stops them, not a side effect.
    - There is no cookie or other ambient credential (ADR-0038 §6). A forged
      upgrade still needs the bearer in `hello`.
  - *Test:* A4, A5.
- **P5. A second backend under the same origin.**
  - *Attack:* the operator adds `serve --set-path /grafana …` on
    `<name>.ts.net:443`. That backend's script is same-origin, so it can read
    the GUI tab's `sessionStorage` bearer and IndexedDB transcripts.
  - *Control:* R1 condition 2 requires exactly one `/` handler. The running
    check latches the public class closed when a second handler appears.
  - *Test:* A9, A11.
- **P6. Code replay or leakage through clipboard, scrollback or history.**
  - *Attack:* the printed URL sits in tmux or terminal scrollback, in Universal
    Clipboard (synced to other Apple devices), in browser omnibox history or in
    a synced history service, and someone replays it.
  - *Control:*
    - The code is single use, consumed even on an audience mismatch, and lives
      60 s.
    - The client removes the fragment before any request (ADR-0038 §3), so the
      only history entry with the code is the one the browser recorded before
      `replaceState`.
    - A replay needs an allow-listed tailnet identity (R2).
    - Output goes to a terminal only (R3).
  - *Residual:*
    - A same-identity device that replays within 60 s, before the operator
      does, wins the code. The operator then sees "expired or already used" and
      knows.
    - **A local process that sees the code can redeem it.** Any local uid that
      reads the printed URL (from a shared tmux scrollback, a pane capture, a
      terminal log or the clipboard) can forge every public-class header over
      loopback and exchange it within 60 s. R2's allow-list does not stop it.
      This is no worse than a loopback code today, which a local process can
      redeem the same way. The controls are the same as for loopback codes:
      single use, 60 s, fragment only, terminal-only output. The `unix:`
      backend (open question 3) is the only change that would close it.
  - *Test:* A14, A15, A16.
- **P7. Funnel misconfiguration.**
  - *Attack:* `tailscale funnel 443 on` is enabled for `<name>.ts.net`.
  - *Control:* startup refuses it (R1 condition 4). At runtime the check latches
    the public class closed. Even before the next probe, every Funnel request
    carries `Tailscale-Funnel-Request` and no identity, so it is refused per
    request. The control works without the probe.
  - *Test:* A7, A10.
- **P8. The bearer is lost with a remote Mac.**
  - *Attack:* the remote Mac is stolen or shared while a tab still holds the
    bearer.
  - *Control:* `flk client token rm operator-public` revokes every remote tab.
    Existing revocation closes live sessions with 4403 (ADR-0036 §5a). The local
    `operator` bearer is unaffected.
  - *Residual:* transcripts cached in that browser's IndexedDB stay on that
    machine (open question 6).
  - *Test:* A17.
- **P9. CSP regression.**
  - *Attack:* the public document's policy lists the loopback `ws://` source,
    or a request-derived host, so a script injection could exfiltrate over a
    second socket.
  - *Control:* both policies are precomputed from configuration, and each
    names only its own authority.
  - *Test:* A12.
- **P10. Breakage, not exploitation.** A probe timeout, an `Unavailable` result
  or a runtime `NotInstalled` (a tailscaled restart or upgrade) closes the
  public class until the next passing probe, with no flk restart. A parsed
  bad-configuration finding latches it. Either way the remote tab drops. That
  is the intended fail-closed cost. While the serve configuration is bad, live
  handoffs and `flk update` are refused (R1, "Availability cost"). It
  shows in health status with the failed condition, and the SSH interim path
  still works. A Tailscale release that renames serve JSON fields breaks the
  shape check. A6 runs against the pinned Tailscale version's real
  `serve status --json` output.

## Acceptance tests

Each test must fail on `origin/dev` `5f7f4f0ac144`, or on the slice's base, and
pass after the change. The base refuses every public-class request, so a
refusal test alone would pass there. Each refusal test therefore runs as a pair:
a positive control, the A1 request (or the A6 configuration) that must be
admitted, and the same request with exactly one input changed, which must be
refused. The positive control is what fails on the base. The tests use isolated
servers, injected probe output and injected clocks, with no sleeps, as ADR-0037
requires. The live tests are marked as such. No CI job runs tailscaled today, so
they run by hand on an isolated instance, with the result recorded on the PR,
until one exists.

1. **A1.** With `public_origin` set, a document `GET` with
   `Host: <name>.ts.net`, `Sec-Fetch-Site: none`, `X-Forwarded-Proto: https`
   and an allow-listed login returns 200, with the public CSP. On the base, the
   same request returns 403.
2. **A2.** A `ClientConfig` with `public_origin` and `public_users` loads.
   Configurations with a non-`https` scheme, a host outside `.ts.net`, a path,
   a trailing dot, or no `bind` are refused. On the base, the field is unknown
   and `deny_unknown_fields` refuses even the valid configuration.
3. **A3.** The public class refuses Host `<name>.ts.net.`, `<NAME>.ts.net`,
   `<name>.ts.net:443`, a lookalike suffix and a numeric `100.x` Host. Every
   existing loopback-class test passes unchanged.
4. **A4.** On `/v1/ws` and on exchange, the public class refuses `Origin` of
   `http://<name>.ts.net`, `null`, `tauri://localhost`, `https://<other>.ts.net`,
   duplicates and absence.
5. **A5.** The public class refuses `Sec-Fetch-Site: same-site`, `cross-site`
   and duplicates on every path, and refuses an absent `Sec-Fetch-Site` on
   exchange.
6. **A6.** Startup with `public_origin` accepts real `serve status --json`
   output from the pinned Tailscale version that has exactly one HTTPS `/` proxy
   to `127.0.0.1:<port>`. It refuses the same output when the proxy is
   `localhost:<port>`, `https+insecure://`, `unix:` or `Text`, or when the port
   is wrong. On the base, every one of these refuses startup.
7. **A7.** Startup with `public_origin` refuses when `AllowFunnel` is true for
   the host:port, when there is a TCP forward or TLS-terminated TCP on the
   serve port, when there is an HTTP-only handler, or when a second host:port,
   foreground or service entry targets the client port.
8. **A8.** Startup with `public_origin` refuses on `NotInstalled` and on
   `Unavailable`. With `public_origin` unset, `NotInstalled` still starts, as
   #2476 decided.
9. **A9.** Startup refuses a second handler path (for example `/grafana`) on the
   public host:port.
10. **A10.** At runtime, an injected parsed probe result that turns into
    funnel, a TCP forward or a second handler latches the public class closed:
    public `GET`s and exchanges get 403 `public_origin_unverified`, an upgraded
    public socket is closed with 4403, the loopback class is unaffected, and a
    later passing probe does **not** re-open it. An injected `Unavailable`, a
    runtime `NotInstalled` or a timeout instead closes the class, and the next
    passing probe re-opens it without a restart.
11. **A11.** A public exchange that passes every header check is redeemed only
    on a probe that started after it arrived. With a monitor probe in flight
    that started before the exchange and whose injected result passes, while
    the configuration injected for the next probe is a funnel, the exchange is
    refused and latches; it never uses the earlier passing result. A
    bad-configuration result refuses that exchange and latches. A timeout or
    `Unavailable` refuses it with a retryable 503, does not latch, and **does
    not** consume the code. Only an audience mismatch consumes it.
12. **A12.** The public document's CSP is exactly the template with
    `wss://<name>.ts.net/v1/ws`, and contains no `ws://127.0.0.1`. The loopback
    document's CSP contains no `wss://`. A Chromium CSP test (like `csp.test.ts`)
    loads the production bundle under the public policy with zero violations.
13. **A13 (live).** Through a real `tailscale serve` to an isolated instance,
    Safari, Chrome and Firefox load the GUI, exchange a code and reach Live.
    The test records each browser's `Origin` and `Sec-Fetch-Site` on the
    document, the exchange and the `wss` upgrade.
14. **A14.** Public-class requests with no `Tailscale-User-Login`, with an
    unlisted one, with two, or with one that carries `Tailscale-Funnel-Request`
    are refused. A listed login in different ASCII case is admitted. A
    configuration with an empty `public_users` refuses to load.
15. **A15.** `flk client open --print --origin public` prints the
    `https://<name>.ts.net/#code=…` URL. It is refused for a non-operator
    caller, when `public_origin` is unset, when the public class is latched,
    without `--print`, and on non-terminal stdout without `--json`.
16. **A16.** A public code offered on the loopback class gets 401 and is then
    also refused on the public class, and the same holds the other way round. A
    public code exchanged twice gets 401 the second time. A public code at
    60 s gets 401.
17. **A17.** A public exchange returns the `operator-public` bearer and not the
    `operator` one. `flk client token rm operator-public` closes a live public
    session with 4403 and leaves a loopback session live.
18. **A18 (live).** Through real tailscaled, a request carrying a client-supplied
    `Tailscale-User-Login: attacker@example.com` arrives at flk with the
    proxied identity, or with none from a tagged node. A client-supplied
    `X-Forwarded-Proto: https` sent to a plain `--http` serve handler arrives
    with **no** `X-Forwarded-Proto`, and one sent to the HTTPS handler arrives
    as exactly one `https`. This pins the header-stripping behavior of
    tailscaled and of Go's `ReverseProxy` that R1 and R2 depend on, for the
    pinned Tailscale version.
19. **A19.** Freshness: with the monitor task aborted, or panicking on an
    injected fault, and an injected clock, public requests are admitted at
    89 s after the last passing probe and refused at 90 s, with 4403 on an
    upgraded public socket. A subsequent passing probe re-opens the class.
    Repeated probe timeouts never latch.
20. **A20.** Exchange probe cost. 64 concurrent public exchanges that pass all
    header checks and arrive while a probe is in flight spawn at most two
    probes: the one in flight and one shared next probe. Never more than one
    runs at a time, and every exchange takes the result of a probe that started
    after it arrived. 64 public exchanges that fail any header check (no
    login, wrong Origin, same-site) spawn none. The probe runner counts spawns
    and their overlap.
21. **A21.** Live handoff: a latched process hands off to an importer whose
    probe passes, and the importer serves the public class. With the serve
    configuration still bad, the import is refused, and the old process stays
    latched and keeps serving loopback. With `public_origin` then removed and no
    rule targeting the port, the retried handoff succeeds. A handoff request
    from a `flk-agent`-uid caller is refused with `operator_only`, as today.
22. **A22.** Configuration with a `public_origin` port equal to the `bind`
    port refuses to load. This covers both `https://<name>.ts.net:7699` with
    `bind = "127.0.0.1:7699"`, and an omitted port (443) with
    `bind = "127.0.0.1:443"`.
23. **A23.** `flk client token add operator-public ...` and `token add operator
    ...` are refused with `invalid_params`. A pre-existing `operator-public`
    record without the reserved marker makes `client open --origin public` fail
    with a diagnostic and never hands out its bearer. On the base, `token add
    operator-public` succeeds.

Regression guards, which pass before and after and must keep passing: with
`public_origin` unset, the A1 request still returns 403; every existing
loopback-class, shell-origin and #2476 startup test passes unchanged.

## Implementation slices

Each slice stays at or below 400 own production lines (ADR-0043 §9's cap).

- **W1. Configuration and pure checks.** `ClientConfig` fields and validation;
  the authority classifier; public-class header rules; the serve-shape validator
  over parsed JSON, and the port-collision rule. Tests A2–A9, A14 and A22
  (unit level). About 250 lines.
- **W2. Listener wiring.** Two document policies, the guard's class dispatch,
  exchange audience, the running probe, the latch, 4403 on latch, the 90 s
  freshness gate, single-flight exchange probes, and the latch reset on handoff.
  Tests A1, A10–A12, A16 and A19–A21. About 350 lines; split at the
  probe/latch boundary if it runs over.
- **W3. Issuance.** The `ClientOpen` origin parameter, the `operator-public`
  record, reserved token names with their marker, the CLI flag and the
  terminal-output rule. Tests A15, A17 and A23. About 180 lines.
- **W4. Live verification and documentation.** A13 and A18, a next-docs page
  for the setup, and the interim SSH path. Documentation and tests only.

## Consequences

- One machine on the tailnet can use the GUI without the ADR-0043 TLS stack, at
  the cost of a second admission class in the guard and a running probe.
- The public class depends on tailscaled's header behavior. A Tailscale release
  that changes it is caught by A18, not in production.
- The trust gap for local processes stays exactly as wide as it is today, and is
  now written down for the identity header too.
- `flk web`'s unchecked `Tailscale-User-Login` (`src/web/mod.rs:287-292`) is
  recorded as a separate follow-up. It should require a loopback peer, and
  refuse the header when `--allow-non-loopback` is on.
- When ADR-0043 T1–T3 ship, the owner can keep this front end for tailnet-only
  nodes or withdraw it. Nothing here blocks or replaces them.

## Open questions for the owner

Each question carries the review's recommendation. These are recommendations
only, not decisions. Questions 3, 4 and 6 affect safety. Questions 1, 2 and 5
do not.

1. Is a 60 s code lifetime enough for copying a URL between two Macs, or should
   public codes get a longer one, for example 120 s?
   *Recommendation:* keep 60 s. This does not affect safety.
2. Should a code be bound to one login (`--for <login>`) when `public_users` has
   more than one entry?
   *Recommendation:* no `--for` binding. This does not affect safety: a local
   redeemer forges the login anyway (P6).
3. Should the public front end move to a `tailscale serve` `unix:` backend with
   peer-credential checks, once the pinned Tailscale version supports it, to
   close local header forgery (P3)?
   *Recommendation:* yes, as the planned upgrade path. It is the only change
   that closes both the unverifiable-relay gap (R2, P2) and local redemption of
   a seen code (P6), because only a kernel peer credential can prove that
   tailscaled is the sender.
4. Should `operator-public` carry a TTL, so that a forgotten remote tab expires
   without a manual `token rm`?
   *Recommendation:* yes. A remote bearer that never expires is the largest
   residual of P8.
5. Are VM-guest servers worth a host-attested probe later, or is "run tailscaled
   in the guest" the permanent answer?
   *Recommendation:* keep VM guests unsupported. This does not affect safety
   beyond R4.
6. Should a public-class tab keep its transcript cache in IndexedDB at all
   (ADR-0038 §3), or run memory-only, so a lost Mac holds no session content?
   *Recommendation:* yes, memory-only storage by default for the public class,
   so a lost or shared remote Mac holds no session content at rest.
