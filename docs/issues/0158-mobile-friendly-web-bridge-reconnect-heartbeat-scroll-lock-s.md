---
number: 158
title: "Mobile-friendly web bridge: reconnect+heartbeat, scroll-lock+scrollback, loud TLS guidance"
kind: issue
state: OPEN
author: gerchowl
labels: ["enhancement"]
created: 2026-06-15T19:47:02Z
closed: 
url: https://github.com/gerchowl/herdr/issues/158
---

# Mobile-friendly web bridge: reconnect+heartbeat, scroll-lock+scrollback, loud TLS guidance

## Motivation / why

The `herdr web` bridge is the only non-terminal way into the fleet, and in practice it's used from a phone over the tailnet (ADR-0001's whole premise: "a phone (browser) to view the herdr fleet"). Today that phone experience has three concrete failure modes that make it unreliable enough to avoid:

1. **It disconnects constantly.** iOS Safari suspends a backgrounded tab and tears down the WebSocket on sleep/app-switch. The client has **no reconnect path** — `ws.onclose` just paints a red "disconnected" badge (`assets/web/index.html:140`) and the user has to manually reload. There's also no heartbeat, so half-dead connections aren't detected.
2. **Scrollback is semi-functional and the whole page drags around.** Touch-drag rubber-bands the entire document instead of scrolling the pane, because the body has no scroll-lock and the term touch handlers are `passive` (can't `preventDefault`) and only fire when herdr mouse reporting is on (`index.html:237-264`). xterm's own `scrollback` is never configured (`index.html:99-116`), so there's no local buffer fallback when server-side mouse-wheel forwarding is off.
3. **Safari warns "unsafe connection."** herdr does no TLS by design — loopback bind, fronted by `tailscale serve` (ADR-0001 decision 6). The warning means the user reached `http://host:7681` directly instead of through the TLS-terminating proxy. Nothing in the binary makes that mistake loud or self-correcting, and the docs don't spell out the `tailscale serve` path prominently.

**This is explicitly NOT a Tauri/native rewrite.** A webview wraps the same xterm page over the same WS to the same server — it fixes none of these root causes (a backgrounded webview drops the WS too), adds app-store/build friction, and contradicts herdr's stated philosophy ("not a gui window, not electron", README:18). All three problems live in `assets/web/index.html` plus small bits of `src/web/mod.rs`.

## Decision / proposed approach

Stay in the web bridge. Three P0 fixes:

- **P0-A — Reconnect + heartbeat.** Restructure the one-shot `const ws = new WebSocket(...)` (`index.html:125`) into a `connect()` function. Auto-reconnect on `ws.onclose` with exponential backoff (cap ~10s, jittered) and immediately on `document.visibilitychange → visible`. Add a client-side heartbeat: a periodic WS `ping`/app-level keepalive; if no traffic within a window, force-close and reconnect. The persistent `herdr server` (ADR decision 2) means a fresh client attach repaints live state — acceptable. Surface "reconnecting…" in the existing status badge.
- **P0-B — Body scroll-lock + real scrollback.** Lock the document: `html, body { overflow: hidden; overscroll-behavior: none; }`, `#term { touch-action: none; }`, and make the touch handlers non-passive where needed so the page can't rubber-band. Set xterm `scrollback` to a real value (e.g. 5000) as a local fallback. Verify drag-to-scroll still forwards SGR 1006 wheel reports when mouse reporting is on, and degrades to xterm-local scroll when it's off.
- **P0-C — Loud TLS guidance.** When `herdr web` is reached over plain HTTP (no `tailscale serve` in front), make it obvious: a hard, repeated warning on the listen line (`src/web/mod.rs:189-190`) and/or a banner injected into the served page, plus a documented `tailscale serve` setup. Goal: a user who sees Safari's warning is told exactly why and how to fix it.

## What already exists

- Frontend: single hand-written `assets/web/index.html` (288 lines), vendored xterm.js in `assets/web/vendor/`, no build step.
- Server: `src/web/mod.rs` — axum WS bridge, spawns a `herdr` client per WS in a PTY. Passively handles Ping/Pong frames (`src/web/mod.rs:335,449`) — so a client heartbeat needs no server change. Opt-in idle timeout (`--idle-timeout`, default off, `src/web/mod.rs:415-426,665-668`).
- Loopback/funnel guards + CSWSH same-origin check (`src/web/mod.rs:111-147,243-256,550-576`).
- Existing mobile work already in the page: status badge auto-hide, on-screen key bar (Esc/Tab/Ctrl/arrows/prefix), touch→mouse SGR mapping, visualViewport resize (`index.html:39-50,55-72,212-280`).
- ADR-0001 (`docs/adr/0001-web-bridge-hosting-and-transport.md`) — establishes the loopback + `tailscale serve` security boundary and the PTY-per-WS transport.

## Scope

**P0 (this issue):**
- [ ] Reconnect with backoff + reconnect-on-visibilitychange
- [ ] Client heartbeat / dead-connection detection
- [ ] "reconnecting…" status surfaced in the badge
- [ ] Body/document scroll-lock (no whole-page drag)
- [ ] xterm `scrollback` configured + verified touch-scroll both modes
- [ ] Loud plain-HTTP warning (listen line + in-page banner)
- [ ] `tailscale serve` setup documented

**P1 (follow-ups, likely separate issues):**
- [ ] PWA manifest + service worker (offline shell, add-to-homescreen)
- [ ] safe-area-inset / notch handling for keybar + status
- [ ] font-size / pinch-zoom control (hard-coded `fontSize: 13`)
- [ ] swipe-to-switch-tab gesture

## Pitfalls

- **Reconnect ≠ session resume.** Each WS spawns a *fresh* `herdr` client in a new PTY. Reconnect re-attaches to the persistent server and repaints — but any in-browser scrollback/local term state is lost unless we keep the `term` object and only reopen the socket. Decide: reuse `term` (repaint over existing buffer, possible visual artifacts) vs `term.reset()` on reconnect.
- **Reconnect storms.** Backoff must be jittered and capped, and visibilitychange + onclose can both fire — guard against double-dialing and against hammering a server that's down (respect `--max-sessions`, default 16).
- **Heartbeat vs idle-timeout interaction.** If the operator sets `--idle-timeout`, a client keepalive that sends real frames would defeat it; a pure WS-ping might not reset the server's idle counter. Confirm which frames the idle timer counts (`src/web/mod.rs:415-426`).
- **`touch-action: none` can kill the soft keyboard / tap-to-focus.** Tap-to-focus (`index.html:283`) and keybar usability must survive the scroll-lock.
- **Non-passive listeners cost scroll latency** — only go non-passive where preventDefault is actually needed.
- **In-page HTTP banner needs the server to know it's behind a proxy** — detecting "plain HTTP vs fronted by tailscale serve" from inside a loopback-bound process is non-trivial (X-Forwarded-Proto? Host header?). May only be reliably detectable client-side (`location.protocol`).
- **CSWSH/origin check** could reject reconnects if origin handling differs on retry (`src/web/mod.rs:550-576`).

## Acceptance criteria

- [ ] Backgrounding Safari for 60s then returning reconnects automatically with no manual reload.
- [ ] A dropped connection recovers within a few seconds without user action.
- [ ] Touch-dragging the terminal scrolls the pane (or xterm buffer), and never drags/rubber-bands the whole page.
- [ ] Scrollback is usable on mobile in both mouse-reporting-on and -off states.
- [ ] A user on plain `http://` gets a clear, actionable warning pointing at `tailscale serve`.
- [ ] No regression to keybar, tap-to-focus, status badge, or desktop behavior.

## References

- ADR-0001 `docs/adr/0001-web-bridge-hosting-and-transport.md` (security boundary, transport)
- Frontend `assets/web/index.html` (esp. :99-141 ws/term setup, :212-284 touch/resize)
- Server `src/web/mod.rs` (Ping/Pong :335,449; idle timeout :415-426; listen :189-190; CSWSH :550-576)
- Parent web-bridge issues: #131, #109

---

## Comments

### gerchowl — 2026-06-15T19:49:50Z

## Review round 1 — consolidated (4 fresh-eyes panels: iOS-Safari-lifecycle, WS-transport, xterm-touch-UX, security/TLS)

### Where all reviewers agree (clean signal — bake into the plan)

- **Browsers cannot send WS ping control frames.** `WebSocket.send()` only emits data frames. The heartbeat in P0-A as written ("WS ping") is impossible — it MUST be an **app-level JSON control message** (`{type:"ping"}` / `{type:"pong"}`), reusing the existing `{type:"init"|"resize"}` channel.
- **`visibilitychange` alone is insufficient on iOS.** Must also handle `pageshow` (especially `event.persisted === true` for BFCache) and `pagehide`. BFCache restores a socket whose `readyState` is OPEN but whose underlying connection is dead — never trust readyState on resume.
- **On resume, do an explicit liveness probe**, don't wait for TCP RST (30–60s on cellular): send a ping, 2–3s timeout, force-close + reconnect if no pong.
- **Reuse the `term` object on reconnect; do not recreate.** Recreating flashes, drops focus, and dismisses the iOS soft keyboard. Force a full repaint by sending a synthetic resize so the fresh PTY client redraws.
- **Heartbeat trigger = "no pong within N of last ping", NOT "no traffic in N seconds."** Idle herdr panes legitimately emit zero bytes for minutes.
- **TLS detection must be client-side** (`location.protocol !== 'https:'`), not server-side header sniffing — `tailscale serve` doesn't set a dependable `X-Forwarded-Proto` and rewrites Host to `127.0.0.1:7681`, indistinguishable from a direct hit. Banner belongs in `index.html` (server-side HTML injection would break the `<base href>` path-mount logic at index.html:24-29). Add a localhost carve-out so dev/loopback doesn't nag.
- **P0-B scroll-lock recipe is sound:** `html,body { overflow:hidden; overscroll-behavior:none; position:fixed; inset:0 }`, `#term { touch-action:none }`, `#keybar { touch-action:pan-x }`, and flip the touch listeners to `{passive:false}` + `preventDefault()` in `touchmove`. This kills the rubber-band without breaking tap-to-focus, keyboard, or the keybar.

### The real decision point: this is NOT client-only

The issue said "client-side only (no server change intended)." The transport review refutes that — **~4 small server changes in `src/web/mod.rs` are required for correctness**, not polish:

1. **Handle `{type:"ping"}` as a no-op + reply `{type:"pong"}`.** Otherwise an unparseable text keepalive falls through to `stdin_tx.send(t.into_bytes())` (mod.rs:444) and **types into the shell** — a hard bug.
2. **Don't reset `--idle-timeout` on the keepalive frame.** Track last *user-input* frame for the idle timer; pings must not defeat an operator's idle timeout.
3. **Server-side active ping** (axum `Message::Ping` ~every 25s) to detect half-open sockets from a backgrounded Safari tab whose JS timers are frozen — the client heartbeat physically cannot run while backgrounded, so only the server ping reaps those.
4. **Explicit `child.kill()` on WS-close cleanup.** A flapping phone can churn reconnects faster than orphaned PTY children die, exhausting `--max-sessions=16`.

### New pitfalls surfaced (beyond the filed list)

- Unparseable text keepalive → typed into the shell (transport).
- Orphan PTY children → max-sessions exhaustion under mobile flapping (transport).
- BFCache returns a dead-but-OPEN socket (iOS).
- Off-mode scrollback currently **no-ops** (handlers early-return when `!mouseReportingOn()`), it doesn't degrade — must add a `term.scrollLines(-steps)` branch so drag scrolls the local xterm buffer when mouse reporting is off. Set `scrollback: 5000` (dead weight when mouse-reporting-on, since xterm forwards wheel to PTY instead of scrolling locally — that's expected, not a conflict). (xterm)
- **Backfire risk:** a loud "insecure connection" warning could push power users to `--allow-non-loopback` (binds 0.0.0.0, strictly worse). Wording must steer ONLY to `tailscale serve`; the `--allow-non-loopback` `--help` must state "does NOT add TLS." (security)
- `navigator.onLine` is unreliable on iOS — use it as a trigger, never as a gate (iOS).
- The status badge auto-hides after 2s, hiding reconnect progress — show "reconnecting (attempt N)" persistently until OPEN (iOS).
- Standalone / add-to-homescreen PWA lifecycle is more aggressive and is likely the primary usage mode — test there explicitly (iOS).

### Verdicts
- **P0-A (reconnect/heartbeat):** directionally right, unsound as written. Needs app-level ping, `pageshow`/BFCache handling, on-resume probe, `term` reuse, AND the 4 server changes above.
- **P0-B (scroll-lock/scrollback):** sound — ship the recipe above plus the `scrollLines` off-mode branch.
- **P0-C (TLS):** client-side detection + in-page banner + listen-line wording + `--help` fix. Consistent with ADR-0001; warrants a short addendum, not a rewrite.

