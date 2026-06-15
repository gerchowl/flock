---
number: 116
title: "fix(remote): federation switch fails-with-notice instead of an install prompt that corrupts the terminal (#115)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-13T21:55:51Z
closed: 2026-06-13T21:56:23Z
merged: 2026-06-13T21:56:23Z
base: master
head: switch-no-install-prompt
url: https://github.com/gerchowl/herdr/pull/116
---

# fix(remote): federation switch fails-with-notice instead of an install prompt that corrupts the terminal (#115)

## Summary

Closes #115. A side-pane SWITCH to a remote with a missing or version-mismatched `herdr` binary was running the same interactive install/upgrade prompt as `herdr --remote <target>` from a shell. The leg loop may still be holding the alternate screen + raw mode for the seamless swap (#69/#72), so the prompt's `eprintln!` scribbled on the held frame and the `stdin().read_line` blocked the terminal until the user typed something - corrupting the tab.

### The split

Added a `LaunchContext` enum on `RemoteLaunch` (`Cli` vs `FederationSwitch`) and threaded it through:

- `run_remote` -> `prepare_remote_herdr` -> `ensure_remote_server_ready`
- the three `confirm_remote_*` prompt sites (`confirm_remote_install`, `confirm_remote_install_with_running_server`, `confirm_remote_server_stop`)

The explicit `herdr --remote <target>` path (the `extract_remote_args` parser) keeps `LaunchContext::Cli` and still prompts. The federation-switch path constructed in `decide_next_leg` uses `LaunchContext::FederationSwitch`: every prompt branch becomes a hard `io::Error` instead, which rides the existing leg-loop fall-back rail - the previous leg relaunches with a top-right failure notice (#67) and `force_restore_host_terminal` clears the hold on the way out.

### Restore audit

The non-interactive branches return `Err` *before* any `eprintln!` or `stdin().read_line`, so the held alt-screen is never written to and no stdin read is attempted. The error then flows into `run_remote`'s `?`, back to `run_attach_legs`, which `decide_next_leg` converts into a `FallBack { to: previous, notice }` step. The fall-back path re-attaches the previous leg, which repaints over the held frame; if the fall-back also fails, `Finish { restore_terminal: true, .. }` triggers `force_restore_host_terminal()`.

## Test plan

- [x] `cargo test --bin herdr launch_context` - 2 passed (`allows_install_prompt` true for Cli, false for FederationSwitch)
- [x] `cargo test --bin herdr confirm_remote` - 3 passed (each confirm function errors under FederationSwitch)
- [x] `cargo test --bin herdr -- decide_next_leg cli_remote_leg` - 6 passed (switch leg's context is `FederationSwitch`; CLI `--remote` parser yields `Cli`)
- [x] `cargo test --bin herdr -- --test-threads=2` - 2197 passed (the 1 flake is `server::autodetect::is_server_listening_returns_false_when_listener_dropped`, unrelated socket-reuse race; passes isolated)
- [x] `cargo test --test live_handoff -- --test-threads=2` - 16 passed
- [x] `cargo test --test peer_federation -- --test-threads=2` - 7 passed
- [x] `cargo test --test client_mode -- --test-threads=2` - 19 passed
- [x] `cargo fmt --check` clean
- [x] `cargo clippy --all-targets -- -D warnings` clean
