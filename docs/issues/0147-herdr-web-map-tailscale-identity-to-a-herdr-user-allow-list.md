---
number: 147
title: "herdr web: map tailscale identity to a herdr user / allow-list (auth beyond loopback)"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-15T00:34:10Z
closed: 2026-06-15T02:21:11Z
url: https://github.com/gerchowl/herdr/issues/147
---

# herdr web: map tailscale identity to a herdr user / allow-list (auth beyond loopback)

Follow-up to #131 (security P1 from the spike review panel).

## Motivation
`herdr web` v1 treats "anyone on the tailnet who passes the same-origin check" as authorized — it is a full interactive shell. `tailscale serve` can pass identity headers (`Tailscale-User-Login`, `Tailscale-User-Profile-Pic`). A compromised/shared tailnet device, or an ACL slip, currently means a full shell. A lost/stolen phone with a live tab is a standing shell.

## Proposed
- Parse `Tailscale-User-Login` on the WS upgrade (`src/web/mod.rs` `ws_handler`).
- Reject if not in a configured allow-list (`--allowed-user` flag / `[web] allowed_users` config). Absent header (direct loopback, native client) stays allowed.

## Acceptance
- [ ] WS upgrade rejected (403) for tailnet identities not in the allow-list
- [ ] Absent identity (loopback) still allowed
- [ ] Config + flag surface documented

Refs #131, #109.
