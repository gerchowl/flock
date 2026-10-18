# Public structured routing dogfood (#1843)

Run inside the development shell after `cargo build --locked --bin flk`:

```sh
python3 -I tests/probes/structured_public.py --flk target/debug/flk
```

The same probe runs under nextest's `structured_public` integration binary.
It starts a real headless server under a newly allocated short `/tmp` directory,
with a sandbox HOME, config and state. Its environment is explicit; inherited
`FLOCK_*` variables and live credentials are excluded. The only token is fixture
data in a mode-0600 file. The fake harness uses a generated private MCP config
to start a real `flk mcp serve --structured` child, and asserts that no lease
credential reached the model environment.

The default dedicated-user start must refuse. In a fresh server with explicit operator opt-in,
the public CLI starts a pane-free parent. Through its real MCP child the probe
checks raw-method exclusion, unexposed argv, disabled spawn, lineage depth,
capacity, fanout, tracked checkout admission, and the child's framed opening
turn. It checks additive list/get/status contracts by id and name, PTY contract
preservation, pane-only refusal, and parent refusal to stop its child. The
operator stops the child, and the fake parent then invokes the real CLI against
both that offline child and itself: both resumes must refuse `operator_only`.
The operator resumes the child with the same native session identity; another
MCP spawn must still observe the original parent's fanout/capacity accounting.
The restart scenario then starts a new server against the same sandbox snapshot,
resumes both identities through the public CLI, checks the saved parent/depth,
and proves that the new live lease still charges the child to its original
parent's fanout limit. The operator stops the owned sessions and server.
A success prints a JSON receipt.

Each policy scenario starts a fresh server with its own loaded config.
Live reload does not apply `[structured]`/`[fleet]` yet (#2043); the `[sessions]`
case is tracked by #1975.
The probe observes a session-sourced idle turn, persisted result and Offline
after stop; a result does not invent missing background evidence.

Every subprocess, listener and profile belongs to this probe. Readiness uses
pipes and the server's readiness output; calls use bounded reads. There are no
sleeps or retry loops. On failure the probe terminates and joins only its own
server, whose worker cleanup owns its model tree. All fixture directories are
removed on exit. Never substitute the live API socket or a live Claude profile.

Limits: the fake proves routing and admission, not provider behavior. Default
agent-user launching remains gated on #1208. Explicit operator execution retains
#1177's cooperative same-UID and #1629's detachment limits. Stdio permission and
question tools remain S6 (#1307). CLI start creates an idle session; narrowed MCP
spawn additionally submits its opening turn. Pane attach, terminal keys, pane
history/readiness and settled waits are not structured operations.
