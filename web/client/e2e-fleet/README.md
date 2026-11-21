# Real GUI fleet

Install Chromium with `just client-browsers`, then run `just gui-e2e-fleet`.
Bun and Chromium are required: their absence fails the test. The recipe builds
the production bundle before compiling the `web,ts` server and Rust driver.
CI runs the same recipe in the required `gui-e2e-fleet` Linux job. Ordinary
Rust shards on both platforms exclude this browser-dependent binary.

The Rust driver starts two isolated nodes with the fleet SSH shim. It scrubs
inherited `FLOCK_*` variables and uses short socket paths. Each node has its own
HOME, XDG directories, structured configuration and scripted Claude executable.
Each node receives a synthetic setup token, readable only by its owner.
The browser starts a structured agent on the home node through New agent and
checks its registration through that node’s CLI. After conversing, it stops and resumes the same session, sends a new prompt,
and stops the agent again. It then chooses the other node,
checks the plain-language refusal and verifies that node’s agent list is unchanged.
The driver creates one short, temporary working directory per node and records
its canonical path as `working_directory` in that node’s manifest entry. These
directories live under the driver’s owned tools root and are removed on teardown.
The browser first submits a nonexistent directory and checks its plain-language
refusal, then corrects the path in the same sheet. It checks the exact
cwd and structured runtime through `flk --json agent list` on the home socket.
It types the other node’s own path when testing the remote refusal.

The working-directory input defaults to the first project path reported by the
chosen node, if any. A project select offers those paths and **Other directory…**;
the operator can always type a path. An empty or relative path keeps Start
disabled. Changing nodes resets the directory to that node’s first project, or
an empty input. Directory existence is validated by the server.

The model script in `gui-fleet.json` checks three exact messages. The first
produces a final reply; the second requests one Bash permission, waits for the
browser's **Allow once**, then replies only after receiving `allow`. The browser
checks the durable user echo, both assistant replies, and the permission opening
and closing in the transcript. After a confirmed Stop and observed offline state,
**Restart agent** resumes the same session. The third prompt receives a distinct
reply; an independent history replay checks that reply. The home CLI checks the
same agent and session IDs across resume and confirms offline after the final
Stop. The GUI must also show offline; a stop acknowledgement alone is insufficient.
Partial-stream coverage arrives with #3131.

The browser loads the real served production bundle with its existing shell
configuration seam. The long-lived Bun driver mints an operator app-session
credential directly over the isolated home socket and supplies it before page
load. A test-owned loopback relay forwards HTTP assets unchanged from `flk` and
forwards `/v1/ws` bytes to the home's native Unix WebSocket socket. The same Bun
process mints and connects, preserving the kernel peer identity required by
app-session admission. Serving both paths at the relay origin also preserves the
bundle's normal content security policy; no browser security setting is disabled.
This models the app shell's authority: browser file tokens cannot stop or
resume structured agents (ADR-0036 §1a). No wire responses or application state
are mocked. The driver process remains alive for the credential's lifetime.

An independent wire client mints its own read/operate token through the home
socket, opens a separate connection, and replays durable `agent/<id>` history.
It checks both messages and replies and the permission's matching request id,
request sequence and recorded `allow_once` decision. It never uses the browser's
session or rendered state. The test unregisters the simulated DOM before
opening its real socket. This oracle moves to the CLI once #3132 supplies a
structured history read contract. Lifecycle and cwd still use `flk --json agent list`.

Remote start is attempted through the same picker. The normal test checks the
typed `unsupported_for_agent` refusal and unchanged node B inventory today.
A separate `test.failing` success assertion uses that observed outcome, so an
infrastructure failure still fails the normal test. When ADR-0043 permits the
start, node B must register the agent and the expected-failure assertion becomes
an unexpected pass; convert it to a normal test then.

The version 1 manifest contains the binary path, home node name, nodes (name, authenticated NodeId,
socket, working directory and isolated CLI environment), and artifact directory. The browser asks
`client open --print` for the home endpoint after launching Chromium. It records
the URL as `home_url` in the disposable manifest, then navigates to the relay origin
without the browser sign-in code: the app-session credential supplies authority.
It selects All nodes, waits for each authenticated node's online status in the DOM,
checks each node's workspace and the home's mesh route through `flk --json` on its own socket.

For a negative control, run `GUI_E2E_FLEET_REFUSE_EDGE=1 just gui-e2e-fleet`.
This refuses the home-to-peer edge before either node starts, using the same
browser assertion. It must fail. No DOM fixtures, browser timers, sleeps or
retries participate in the scenario. A 120-second driver deadline kills the
owned Bun process group. Detached Chromium groups are identified by the unique
manifest environment marker and killed as well. A separate test verifies that a
live, silent producer fails at the deadline.

Failures write `trace.zip`, `fleet.png`, Bun output and per-node logs under
`web/client/artifacts/gui-e2e-fleet/` (override with
`GUI_E2E_FLEET_ARTIFACTS`). Traces and screenshots are saved only on failure.
CI uploads the directory on failure. Sign-in codes and tokens belong only to
these disposable servers; treat traces as test credentials while they run.
