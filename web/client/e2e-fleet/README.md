# Real GUI fleet

Install Chromium with `just client-browsers`, then run `just gui-e2e-fleet`.
Bun and Chromium are required: their absence fails the test. The recipe builds
the production bundle before compiling the `web,ts` server and Rust driver.
CI runs the same recipe in the required `gui-e2e-fleet` Linux job. Ordinary
Rust shards on both platforms exclude this browser-dependent binary.

The Rust driver starts two isolated nodes with the fleet SSH shim. It scrubs
inherited `FLOCK_*` variables and uses short socket paths. Each node has its own
HOME, XDG directories, structured configuration and scripted Claude executable.
No agent is started by this slice. Future lifecycle tests can seed
`gui-fleet.json` in each home, then use the same manifest and CLI helper.

The version 1 manifest contains the binary path, home node name, nodes (name, authenticated NodeId,
socket and isolated CLI environment), and artifact directory. The browser asks
`client open --print` for the home URL immediately before navigation, after launching
Chromium, so an operator code cannot expire during browser startup. It then adds that
URL as `home_url` in the disposable manifest immediately before navigation.
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
