---
number: 109
title: "web terminal: xterm.js endpoint over tailscale to attach the phone to the fleet"
kind: issue
state: OPEN
author: gerchowl
labels: []
created: 2026-06-13T09:42:55Z
closed: 
url: https://github.com/gerchowl/herdr/issues/109
---

# web terminal: xterm.js endpoint over tailscale to attach the phone to the fleet

## Feature (user): xterm.js endpoint served over tailscale -> attach the phone to the fleet

Get a phone (browser) into the same herdr fleet via a web terminal, served over the tailnet (the 9ern/`tailscale serve` infra already exists on sage).

## Shape
- **herdr web-serve mode**: a small server that bridges a browser websocket <-> a herdr CLIENT (or directly to the session server), rendering into xterm.js. Reuses herdr's existing client (semantic frames -> the client paints) OR a raw-PTY bridge to xterm.js. The mobile single-column layout already exists for narrow widths -> the phone gets the mobile UI.
- **Transport**: `tailscale serve` / `funnel` fronts the websocket endpoint (HTTPS, tailnet-auth) like 9ern does -- no public exposure, fleet-auth via tailscale identity.
- **Attach target**: the phone endpoint attaches to the local server on whichever host runs it (sage, always-on) and -- via federation -- can switch to any fleet server (the spoke gossip, once #102/slots settle).

## Open questions (spike-worthy)
- xterm.js consuming herdr SEMANTIC frames vs raw PTY: the semantic-frame client paints locally (mosh-like, #responsiveness); a browser can't run libghostty-vt, so the web bridge likely streams a RENDERED cell grid (server-side render -> xterm.js writes ANSI), accepting the heavier-bytes tradeoff for the phone case. Decide.
- Input: touch/soft-keyboard -> websocket -> PTY; the mobile UI's existing touch routing applies.
- Auth: tailscale-serve identity header -> map to the herdr user; no separate auth.
- Multi-client: the phone is just another client on the session server (herdr already supports multiple clients per session) -- the desktop and phone view the same session live.

## References
9ern + `tailscale serve` (g-fleet), the mobile single-column layout, herdr client/server semantic-frame protocol, multi-client support, #65 (slots/federation).

---

## Comments

### gerchowl — 2026-06-13T17:55:21Z

## Spike verdict: DECISIVE-YES (~weekend MVP, lives in g-fleet)

The render model is ALREADY solved: wire.rs has a **TerminalAnsi** encoding (TerminalFrame { bytes }) -- server-side diffed ANSI via render_ansi.rs BlitEncoder. With HERDR_RENDER_ENCODING=terminal-ansi the client pipes frame.bytes straight to stdout (client/mod.rs:879). xterm.js eats exactly that byte stream -- no JS painting, no server rerender for the web case. Multi-client exists (headless.rs clients map + foreground_client_id). Mobile UI is width-driven (compute_mobile_view) so a narrow browser auto-gets it.

**v1 architecture**: a standalone bridge (Rust ~200-400 LoC, tokio + axum/tungstenite + portable-pty): WS connect -> spawn herdr in a PTY with HERDR_RENDER_ENCODING=terminal-ansi + COLUMNS/LINES from xterm; PTY stdout -> WS -> term.write; term.onData -> PTY stdin; onResize -> pty.resize. Front with tailscale serve; auth via the Tailscale-User-Login identity header. Kitty graphics is the known v1 cut (cell_width_px=0 disables it).

**Repo: g-fleet** (modules/herdr-web.nix + the bridge bin + a static xterm.js page), next to 9ern -- it's a transport/ops concern, keeps hyper/axum out of the herdr crate. Landing per the session goal.

### gerchowl — 2026-06-14T12:29:24Z

Web-view follow-ups split out (all root-caused to: the bridge faithfully renders a full herdr session but only forwards plain keystrokes):

- #128 — bug: "connected" badge overlaps the top-right TUI (server/repo picker)
- #129 — CLI-friendly on-screen key bar (Esc/Tab/Ctrl/arrows/prefix) → unlocks keyboard-driven features (worktrees etc.) on a phone
- #130 — forward wheel/touch as mouse events → scroll the conversation + right-click/selection parity

Feature parity is inherent (it's the same herdr binary); #129 + #130 are the input-forwarding gaps that gate it.

### gerchowl — 2026-06-14T12:41:42Z

Graduation issue: https://github.com/gerchowl/herdr/issues/131 — port the bridge out of g-fleet into herdr as a first-class `herdr web` option, leaving g-fleet to own only the tailscale-serve exposure. The web-view fixes (#128/#129/#130) should land there.

