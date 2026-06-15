---
number: 150
title: "herdr web: native socket bridge (drop the per-connection PTY) — unblocks #128/#129/#130"
kind: issue
state: OPEN
author: gerchowl
labels: []
created: 2026-06-15T00:34:14Z
closed: 
url: https://github.com/gerchowl/herdr/issues/150
---

# herdr web: native socket bridge (drop the per-connection PTY) — unblocks #128/#129/#130

Follow-up to #131 (transport v2 — the spike's deferred option 1(b)).

## Motivation
v1 spawns a `herdr client` in a PTY per WS and pumps opaque bytes. That inherits handshake/retry/fleet-snapshot/server-switch logic for free, but: (a) PTY-per-connection scales poorly (macOS pty limits), and (b) the browser only sees an opaque ANSI stream, so a real "connected" badge (#128), key bar (#129), and mouse/wheel forwarding (#130) are awkward.

## Proposed (spike first)
A `herdr client --stdio --render-encoding=ansi-raw` mode (no raw-mode/OSC capture, byte shuttle over stdio) the bridge pipes WS↔stdio — no PTY — OR the bridge speaks the `herdr-client.sock` bincode protocol directly (`src/protocol/wire.rs`). Either gives structured frames + clean lifecycle. Weigh the reimplementation cost (handshake/retry/switch) before committing.

## Acceptance
- [ ] No PTY per connection; clean close on disconnect
- [ ] Structured signal for connect/disconnect (#128) and input channel for #129/#130
- [ ] Decision recorded (extends the ADR)

Refs #131, #128, #129, #130.

---

## Comments

### gerchowl — 2026-06-15T13:22:44Z

## Re-scoping note (from starting the impl)

Verified against the code while scoping the stdio mode — **the premise that #150 unblocks #128/#129/#130 is wrong**:

- The server already parses SGR mouse sequences straight out of the `ClientMessage::Input` byte stream (`src/raw_input.rs:644` `parse_sgr_mouse`; `src/server/client_transport.rs:424` → `ServerEvent::ClientInput`). So **#130 (mouse/wheel)** is frontend-only: xterm enables mouse reporting and forwards the escape bytes over today’s PTY transport.
- **#128** (badge) and **#129** (key bar) are pure `index.html`.

This matches the round-1 transport review ("all three are frontend-only"). So #150’s only real benefit is **scaling** (dropping the PTY-per-connection ceiling), which does not bite a personal phone+laptop fleet — especially now that #148 added a concurrent-session cap.

Additional cost found: a `herdr client --stdio` mode has **no TTY**, so resize needs a new side-channel (no PTY `winsize`/SIGWINCH) — real client surgery for marginal benefit today.

**Recommendation: defer #150** as premature optimization. Do #128/#129/#130 (the actual phone-usability wins) on the current transport first; revisit #150 if/when multi-client scale becomes real.

