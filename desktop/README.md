# flock desktop shell

A Tauri v2 window around the unchanged `web/client` bundle (ADR-0038 §6,
#1601). On the server's machine it carries the bundle's `/v1/ws` session
itself, over the server's unix socket, so the webview never holds a token. It
is a separate crate with its own `Cargo.lock`, `bun.lock` and `target/`, never
a member of flk's build and never linked into `flk`.

## Build and run

- `just desktop-dev`: the shell against the client's Vite dev server.
- `just desktop-build`: the client and a release shell. On macOS that is an
  ad-hoc signed `flock.app` in `target/release/bundle/` (`tauri.macos.conf.json`);
  identity signing and notarisation are G9c (#1636). Elsewhere bundling is off
  and the build is the binary in `target/release/`.
- `cd desktop && cargo test`: the bridge, mint, config and hardening tests,
  against a fake unix-socket server. `bun test` there checks the init script's
  `openBridge`. CI does not run them yet (#2477).

## Zero-touch, and the bridge

On the server's machine the shell needs no token input (ADR-0038 §6, "The
server on the same machine"). The init script sets the bundle's
`__FLOCK_SHELL_CONFIG__` (`web/client/src/config.ts`) to the contract's
`{ transport: "bridge", openBridge }`; `src/open-bridge.js` is that function.

- **The mint.** The shell's native side (`src/mint.rs`) opens `flock.sock` and
  sends `client.app.mint`. The server picks the scopes and binds the token to
  this process, by PID and birth, so it dies when the app exits.
- **The session.** Each `openBridge` call is one `bridge_open`
  (`src/bridge.rs`): a WebSocket over a fresh connection to `flock-ws.sock`, the
  sibling of `flock.sock` (`custom.sock` gives `custom-ws.sock`), with the same
  HTTP/1.1 upgrade and frames as the TCP `/v1/ws`. The client sends its `hello`
  without a token; the native side adds the bearer and forwards it, then passes
  every frame through unchanged both ways, and every close code and reason as
  the server sent it. A first frame that is not a token-free `hello` is closed
  with 1008 and never forwarded. Frames come back on one Tauri channel per
  session; `bridge_send` and `bridge_close` carry the webview's side.
- **One current bearer.** The native side mints when it has none, when the
  open renews (`renew: true`, at ¾ of `expiresInMs`), when the bearer has run
  out on a clock that counts sleep, and after the server ends a session on it
  with 4401 or 4403. A refused bearer is never offered again. Any other close,
  such as 1006 or 1001, keeps it, so a reconnect mints nothing. `fresh: true`
  says the open minted without the client causing it.

**Why no token reaches the webview.** The bearer is the operator's identity
while the app runs. It stays in the native process: no command returns it, the
open returns only `{ id, expiresInMs?, fresh? }`, and every message, frame and
close reason the webview gets has it cut out. It is never written to disk or
logged, and `Debug` shows `Token(<redacted>)`. The capability manifest grants
only the three bridge commands, so nothing in the webview can ask for it. The
server also accepts the bearer only from this process, over the unix socket.

The socket is where `flk` (a release build) puts it: `FLOCK_SOCKET_PATH`, else
`$XDG_CONFIG_HOME/flock/` or `~/.config/flock/`, under `sessions/<name>/` for
`FLOCK_SESSION`. With no server there, the shell starts one as `flk server` in
its own process group, as `flk` itself does, and waits up to 5 s for the socket.
It runs `FLOCK_DESKTOP_FLK`, else `flk` on PATH; an app opened from Finder gets
launchd's short PATH, so set `FLOCK_DESKTOP_FLK` or start the server first.

## Refusals

A refusal rejects `openBridge`; the client shows the message in NeedsToken and
does not retry it. None falls back to anything silently.

- **"…not running as the local operator (it was started from inside a flk
  pane…)"**: the upgrade's 403 `operator_only`. **The pane trap:** `open`,
  `bun x tauri dev` or the binary run from a flk pane, or from an agent,
  inherits the pane's ancestry and is refused by design. Open the app from the
  Dock, Finder or a launcher, or use the remote path (#2293).
- **"…started from inside a flk pane, or not from your own session"**: the
  mint's `operator_only`, the same trap.
- **"…refused this app's connection twice (peer_process_changed: …)"**, or
  the mint's **"…could not bind the token to this app's process"**: the server
  could not bind to this process. The shell has already retried once on a
  fresh connection.
- **"…holds too many app sessions or is busy"** (`session_busy`): at most 128
  live app tokens.
- **"…refused the mint request (…)"** (`invalid_params`): the app and the
  server disagree on the verb, usually different versions.
- **"…could not mint a token (code: message)"**: any other mint refusal.
- **"…refused this app's connection with HTTP <status> (<code>)…"**: an
  upgrade refusal the contract does not name. A defect: report it.
- **"No flk server is running, and starting one failed: …"**: why `flk server`
  could not start, or that it did not listen within 5 s.
- **Linux, "…ptrace_scope is 0"**: any process of yours could read the token
  from the app, so the shell does not mint. Set Yama's `ptrace_scope` to 1 or
  more, or use the remote path.

Two ends are not refusals. An unreachable `flock-ws.sock` (a restart or a
handoff) resolves the open and closes the socket with 1006; the server's 503
`preauth_capacity` closes it with 1013. The client reconnects under backoff.

## Hardening

Each rule has a test in `src/hardening.rs` or `src/config.rs`:

- **Capabilities:** `capabilities/main.json` grants `allow-bridge-open`,
  `allow-bridge-send` and `allow-bridge-close`, for the only app commands
  (`build.rs` declares them), so no `shell`, `fs`, `http` or `dialog`, no
  global Tauri API and no remote IPC. Starting `flk server` is the shell's own
  Rust code, not a webview permission.
- **CSP:** `default-src 'self'; connect-src 'none'; frame-src blob: 'self'`.
  The webview opens no connection of its own; the session runs in Rust.
- **Navigation:** only the bundle's origin, its `blob:` frames and, under
  `tauri dev`, the dev server. The origin is `tauri://localhost` on macOS and
  Linux and `http://tauri.localhost` on Windows only: elsewhere
  `tauri.localhost` is a loopback name any local process could serve.
- **DevTools:** Tauri's `devtools` feature stays off, so release has no inspector.
- **macOS:** hardened runtime (`tauri.macos.conf.json`); `entitlements.plist`
  grants nothing, so no `get-task-allow` and no same-UID attach.

A remote server, with a pasted one-time code and the keychain, is #2293.
