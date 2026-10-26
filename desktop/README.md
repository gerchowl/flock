# flock desktop shell

A Tauri v2 window around the unchanged `web/client` bundle (ADR-0038 §6,
#1601). It connects to a `flk` server over `/v1/ws`, like the browser. It is a
separate crate with its own `Cargo.lock`, `bun.lock` and `target/`, never a
member of flk's build and never linked into `flk`.

## Build and run

- `just desktop-dev`: the shell against the client's Vite dev server.
- `just desktop-build`: the client and a release shell. On macOS that is an
  ad-hoc signed `flock.app` in `target/release/bundle/` (`tauri.macos.conf.json`);
  identity signing and notarisation are G9c (#1636). Elsewhere bundling is off
  and the build is the binary in `target/release/`.
- `cd desktop && cargo test`: the config, mint and hardening tests. CI does not run
  them yet (#2477).

## Configuration

The server URL is `FLOCK_DESKTOP_SERVER_URL`, else `server_url` in
`dev.flock.desktop/config.toml` under the platform config dir (macOS:
`~/Library/Application Support/`), else `ws://127.0.0.1:7682`, the
`[client] bind` the flk docs suggest (flk's listener is off until it is set).

```toml
server_url = "ws://127.0.0.1:7682"
```

It must be an origin on a loopback address; remote servers take the
pasted-code path (#2293). An initialization script hands it to the bundle as
`__FLOCK_SHELL_CONFIG__` (`web/client/src/config.ts`). A malformed config never
falls back: the shell injects the contract's `{ transport: "error", message }`
(ADR-0038 §6), and the client shows the message in its ConfigError state.

## Zero-touch token

On the server's machine the shell needs no token input (ADR-0038 §6, "The
server on the same machine"). The init script gives the bundle a
`tokenProvider` that calls the shell's one command, `mint_token`. That command
opens `flock.sock` from this process and sends `client.app.mint`; the server
picks the scopes and binds the token to this process, so it dies when the app
exits. The token stays in memory: never written to disk, never logged, and
redacted from `Debug`. The client asks again at a reload and after the server
closes the socket with 4403 (expiry, revocation, restart), so a revoked token is
one reconnect. Renewal before the 12 h expiry waits for the mint result to carry
the expiry (a server change); until then the shell re-mints on the 4403.

The socket is where `flk` (a release build) puts it: `FLOCK_SOCKET_PATH`, else
`$XDG_CONFIG_HOME/flock/` or `~/.config/flock/`, under `sessions/<name>/` for
`FLOCK_SESSION`. With no server there, the shell starts one as `flk server` in
its own process group, as `flk` itself does, and waits up to 5 s for the socket.
It runs `FLOCK_DESKTOP_FLK`, else `flk` on PATH; an app opened from Finder gets
launchd's short PATH, so set `FLOCK_DESKTOP_FLK` or start the server first.

What the status line says in NeedsToken when the mint fails:

- **"…started from inside a flk pane, or not from your own session"**
  (`operator_only`): only the operator's own console may mint. **The pane
  trap:** `open`, `bun x tauri dev` or the binary run from a flk pane, or from
  an agent, inherits the pane's ancestry and is refused by design. Open the
  app from the Dock, Finder or a launcher, or use the remote path (#2293).
- **"…could not bind the token to this app's process"**
  (`peer_process_changed`): the server could not watch this process. The
  shell has already retried once on a fresh connection.
- **"…holds too many app sessions or is busy"** (`session_busy`): at most 128
  live app tokens. Not retried behind your back.
- **"…refused the mint request (…)"** (`invalid_params`): the app and the
  server disagree on the verb, usually different versions.
- **"…could not mint a token (code: message)"**: any other refusal, such as
  `internal_error`, with the server's words.
- **"No flk server is running, and starting one failed: …"**: why `flk server`
  could not start, or that it did not listen within 5 s.
- **Linux, "…ptrace_scope is 0"**: any process of yours could read the token
  from the app, so the shell does not mint. Set Yama's `ptrace_scope` to 1 or
  more, or use the remote path.

None of these falls back to anything silently. Without a provider (the browser
shell) the client keeps its token prompt.

## Hardening

Each rule has a test in `src/hardening.rs` or `src/config.rs`:

- **Capabilities:** `capabilities/main.json` grants one permission,
  `allow-mint-token`, for the one app command (`build.rs` declares it), so no
  `shell`, `fs`, `http` or `dialog`, no global Tauri API and no remote IPC.
  Starting `flk server` is the shell's own Rust code, not a webview permission.
- **CSP:** `default-src 'self'; connect-src <server>; frame-src blob: 'self'`.
  The shell writes the configured server's `ws:`/`http:` (or `wss:`/`https:`)
  origin in at startup; the checked-in policy has `connect-src 'none'`.
- **Navigation:** only the bundle's origin, its `blob:` frames and, under
  `tauri dev`, the dev server. The origin is `tauri://localhost` on macOS and
  Linux and `http://tauri.localhost` on Windows only: elsewhere
  `tauri.localhost` is a loopback name any local process could serve.
- **DevTools:** Tauri's `devtools` feature stays off, so release has no inspector.
- **macOS:** hardened runtime (`tauri.macos.conf.json`); `entitlements.plist`
  grants nothing, so no `get-task-allow` and no same-UID attach.

macOS and Linux send `Origin: tauri://localhost` (ADR-0038 §6 has the captured
table), so the bundle is served from the Tauri protocol, not a loopback port.

## Next

The webview's `/v1/ws` connection from `tauri://localhost` is admitted once the
server's shell-origin allow-list lands (#2442's second PR); until then the mint
works and the socket is refused at the origin guard.
