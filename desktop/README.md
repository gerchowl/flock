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
  against a fake unix-socket server (`just desktop-test` builds the client first,
  which `generate_context!` embeds). CI runs them on `ubuntu-latest` and
  `macos-15` and the required checks need both (#2477). `bun test` there checks
  the init script's `openBridge`; CI does not run it.

## Nix build (macOS)

Run `nix build .#flock-desktop` on an Apple Silicon or Intel Mac, then
`open result/Applications/flock.app`. To install it, copy that bundle into
`~/Applications/` (create the directory first if needed). Keep the Nix store
closure available: the copied app still uses its store dependencies.

The package embeds the flake's `flock-client` output and builds offline. Its
check phase runs `cargo test --release`, including the DevTools guard, and
the init script's `bun test`. The app is ad-hoc signed with hardened runtime
and the checked-in entitlements after Nix fixups, using `rcodesign`; no Apple
identity, keychain access or notarisation is involved (#1636).

`desktop/bun.lock` contains only Tauri CLI 2.12.2 and its native bindings. Nix
uses that CLI rather than nixpkgs' 2.11.0, alongside the 2.12.3 Tauri crate.
`nix/desktop-deps.json` pins a platform-independent fetch of both Darwin
bindings and records the Bun version and lock digest, following `flock-client`.
After changing the desktop Bun lock or Bun, run `just desktop-deps-hash` and
commit the refreshed pin. Cargo dependencies use `desktop/Cargo.lock` and
its registry checksums directly. Refresh the pin after same-version nixpkgs
Bun patches too, since those can reuse an existing fixed-output path.

The Nix output is Darwin-only. Linux packaging needs WebKitGTK wrapping and
desktop integration in a separate slice (#2790); the existing non-Nix binary build
remains available with `just desktop-build`.

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
  with 1008 and never forwarded, as is one with a duplicate top-level key. A
  binary frame from the server closes the session with 1003 (unsupported
  data): `/v1/ws` carries JSON text only. Frames come back on one Tauri channel
  per session; `bridge_send` and `bridge_close` carry the webview's side.
- **A session belongs to its page.** A reload, a navigation or the window
  closing ends the old page's sessions with 1001 at the server
  (`on_page_load`, `WindowEvent::Destroyed`), and so does a channel the page
  can no longer receive on. Each of these starts a new page generation for
  the webview's label, so an open the old page began that completes later is
  closed with 1001 as soon as it connects and never joins the table; the new
  page's opens are unaffected. Nothing lives on until the token's TTL. A
  channel that ends without a close tells the page 1006.
- **One budget per open.** The mint, its one retry and the upgrade, with its
  one retry, share 10 s, ADR-0038 §3's connect deadline (`mint::DEADLINE`,
  started in `Bridge::open`). Starting a server waits up to 5 s, `flk`'s own
  daemon timeout (`src/server/autodetect.rs`), inside that budget: the mint
  after it gets what is left. An open that runs out during the mint rejects;
  one that runs out during the upgrade resolves and closes with 1006, an
  outage. No lock is held across any of it, so a hung server stalls only the
  opens that need it.
- **One current bearer, one mint at a time.** The native side mints when it
  has none, when the open renews (`renew: true`, at ¾ of `expiresInMs`), when
  the bearer has run out on a clock that counts sleep, and after the server
  ends a session on it with 4401 or 4403. While a mint is in flight, every
  open that needs a bearer, renewing or not, waits for that mint and gets its
  bearer, or its refusal; an open that can use the current bearer does not
  wait. A refused bearer is never offered again, whichever way the 4401 or
  4403 arrives: on a live session, or while a gone page's session waits for
  the server's close reply. Any other close, such as
  1006 or 1001, keeps it, so a reconnect mints nothing. `fresh: true` says the
  open minted without the client causing it.

**Why no token reaches the webview.** The bearer is the operator's identity
while the app runs. It stays in the native process: no command returns it, the
open returns only `{ id, expiresInMs?, fresh? }`, and every message, frame and
close reason the webview gets has it cut out. It is never written to disk or
logged, and `Debug` shows `Token(<redacted>)`. The capability manifest grants
only the three bridge commands, so nothing in the webview can ask for it. The
server also accepts the bearer only from this process, over the unix socket.

The socket is where `flk` (a release build) puts it: `FLOCK_SOCKET_PATH`, else
`$XDG_CONFIG_HOME/flock/`, `~/.config/flock/` or, with neither set,
`/tmp/flock/`, under `sessions/<name>/` for `FLOCK_SESSION`. With no server there, the shell starts one as `flk server` in
its own process group, as `flk` itself does, and waits up to 5 s for the socket.
It runs `FLOCK_DESKTOP_FLK`, else `flk` on PATH; an app opened from Finder gets
launchd's short PATH, so set `FLOCK_DESKTOP_FLK` or start the server first.

## Refusals

A refusal rejects `openBridge`; the client shows the message in NeedsToken and
does not retry it. None falls back to anything silently. Each message says why
and ends by offering the remote path (#2293), added in one place
(`bridge::rejection`).

- **"…not running as the local operator (it was started from inside a flk
  pane…)"**: the upgrade's 403 `operator_only`. **The pane trap:** `open`,
  `bun x tauri dev` or the binary run from a flk pane, or from an agent,
  inherits the pane's ancestry and is refused by design. Open the app from the
  Dock, Finder or a launcher, or use the remote path (#2293).
- **"…started from inside a flk pane, or not from your own session"**: the
  mint's `operator_only`, the same trap.
- **"…could not bind this app's connection to its process, twice
  (peer_process_changed: …)"**, or
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
- **"…did not answer within 10 s"**: the server accepted but did not answer
  the mint within the open's budget.
- **"No flk server is running, and starting one failed: …"**: why `flk server`
  could not start, or that it did not listen within 5 s.
- **Linux, "…ptrace_scope is 0"**: any process of yours could read the token
  from the app, so the shell does not mint. Set Yama's `ptrace_scope` to 1 or
  more, or use the remote path.

Two ends are not refusals. An unreachable or silent `flock-ws.sock` (a restart
or a handoff) resolves the open and closes the socket with 1006; the server's 503
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
