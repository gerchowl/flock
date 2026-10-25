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
- `cd desktop && cargo test`: the config and hardening tests. CI does not run
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
(ADR-0038 §6), and the client shows the message in its ConfigError state. With
no token yet, the client waits in NeedsToken.

## Hardening

Each rule has a test in `src/hardening.rs` or `src/config.rs`:

- **Capabilities:** `capabilities/main.json` grants no permission, so no
  `shell`, `fs`, `http` or `dialog`, no global Tauri API and no remote IPC.
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

PR 2 adds the zero-touch token: the native side mints an app-session token over
`flock.sock` (server slice #2442) and hands it to the webview through one Tauri
command, the only permission the capability gains.
