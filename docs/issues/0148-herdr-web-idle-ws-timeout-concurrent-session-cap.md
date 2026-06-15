---
number: 148
title: "herdr web: idle WS timeout + concurrent-session cap"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-15T00:34:11Z
closed: 2026-06-15T02:21:28Z
url: https://github.com/gerchowl/herdr/issues/148
---

# herdr web: idle WS timeout + concurrent-session cap

Follow-up to #131 (security/robustness P1 from the spike review panel).

## Motivation
`pump` (`src/web/mod.rs`) has no idle timeout and no cap. A forgotten phone tab pins a herdr client + PTY indefinitely; many stale tabs exhaust PTYs (macOS default ~127–1024) and pin one client per tab on the server. A stolen unlocked phone = standing shell.

## Proposed
- Configurable idle deadline: no WS traffic for N minutes → close the socket (drops the PTY + child).
- `--max-sessions` cap; reject new WS upgrades over the cap with a clear close reason.

## Acceptance
- [ ] Idle connections closed after the deadline; child + threads reaped
- [ ] New connections past the cap are refused
- [ ] Both configurable (flag/config), sane defaults

Refs #131.

---

## Comments

### gerchowl — 2026-06-15T02:21:27Z

Done in #154 (merged): `--idle-timeout` (0=off) + `--max-sessions` (default 16) via an RAII slot guard. Live-verified 503 over-cap and idle-close.

