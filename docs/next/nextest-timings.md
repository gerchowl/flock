# Nextest hang-ceiling measurements (#1317)

Reproduce: `python3 scripts/nextest_timings.py .local/nextest-timings --download > docs/next/nextest-timings.md`.
Omit `--download` to render the saved input again. Requires authenticated `gh` and Python 3.

Validate ceilings and override reachability: `python3 -I scripts/test_nextest_timings.py`.
The test script also accepts an absolute path from any working directory (Python 3.11+).
Shadow checks prove full coverage per setting for literal binary selectors and exact/substring test selectors with `&`, `|` and parentheses.
Binary selectors default to globs: supported names without wildcards match exactly, with or without `=`.
Partially shadowed overrides with a reachable OR arm are deliberately not flagged.
Platform-only overrides without a filter are reported as unproved; their ceilings are still checked.
Regexes, negation, other predicates and unequal platform scopes remain unproved, not rejected.

Latest 20 completed dev CI runs at collection time, including cancelled runs.
Recorded executions: 240,138; binaries: 52.
Partial runs contribute only recorded executions, never zeroes for missing tests.
JUnit records final attempts and may omit earlier retries or tests cancelled before reporting.
This is an observed sample, not a guarantee about future load or absent binaries.
p99 uses nearest rank per binary per platform. The larger platform p99 sets sizing.
Ceilings round up to 60-second periods: at least 10× p99 and at least 180 seconds.

Platform gaps: macOS live_handoff is excluded by `.config/ci-shards.toml`.
Ubuntu session_health is unobserved because `tests/session_health.rs` is macOS-only
(`#![cfg(target_os = "macos")]`): it tests macOS session loss using `sandbox-exec` and mach-lookup denial.

| Run | Created (UTC) | Result | Reports |
| --- | --- | --- | ---: |
| [38510093054](https://github.com/gerchowl/flock/actions/runs/38510093054) | 2026-10-14T04:11:13Z | cancelled | 5 |
| [38506522664](https://github.com/gerchowl/flock/actions/runs/38506522664) | 2026-10-14T03:29:35Z | success | 10 |
| [38504004159](https://github.com/gerchowl/flock/actions/runs/38504004159) | 2026-10-14T03:00:28Z | success | 10 |
| [38499917612](https://github.com/gerchowl/flock/actions/runs/38499917612) | 2026-10-14T02:13:34Z | success | 10 |
| [38497213095](https://github.com/gerchowl/flock/actions/runs/38497213095) | 2026-10-14T01:43:02Z | cancelled | 7 |
| [38494560882](https://github.com/gerchowl/flock/actions/runs/38494560882) | 2026-10-14T01:13:47Z | success | 10 |
| [38490379106](https://github.com/gerchowl/flock/actions/runs/38490379106) | 2026-10-14T00:28:31Z | success | 10 |
| [38486895101](https://github.com/gerchowl/flock/actions/runs/38486895101) | 2026-10-13T23:52:50Z | success | 10 |
| [38482341401](https://github.com/gerchowl/flock/actions/runs/38482341401) | 2026-10-13T23:08:06Z | cancelled | 7 |
| [38476957308](https://github.com/gerchowl/flock/actions/runs/38476957308) | 2026-10-13T22:19:02Z | cancelled | 9 |
| [38471647853](https://github.com/gerchowl/flock/actions/runs/38471647853) | 2026-10-13T21:34:38Z | success | 10 |
| [38468617197](https://github.com/gerchowl/flock/actions/runs/38468617197) | 2026-10-13T21:10:51Z | cancelled | 9 |
| [38462485230](https://github.com/gerchowl/flock/actions/runs/38462485230) | 2026-10-13T20:23:54Z | cancelled | 5 |
| [38457524505](https://github.com/gerchowl/flock/actions/runs/38457524505) | 2026-10-13T19:46:36Z | success | 10 |
| [38450199814](https://github.com/gerchowl/flock/actions/runs/38450199814) | 2026-10-13T18:51:51Z | success | 10 |
| [38445204701](https://github.com/gerchowl/flock/actions/runs/38445204701) | 2026-10-13T18:15:03Z | success | 10 |
| [38428949889](https://github.com/gerchowl/flock/actions/runs/38428949889) | 2026-10-13T16:15:07Z | success | 10 |
| [38423964378](https://github.com/gerchowl/flock/actions/runs/38423964378) | 2026-10-13T15:39:40Z | success | 10 |
| [38401759046](https://github.com/gerchowl/flock/actions/runs/38401759046) | 2026-10-13T13:01:54Z | success | 10 |
| [38392195987](https://github.com/gerchowl/flock/actions/runs/38392195987) | 2026-10-13T11:43:30Z | success | 10 |

| Binary | macOS n / p99 s / max s | Ubuntu n / p99 s / max s | Minimum ceiling s |
| --- | ---: | ---: | ---: |
| `agent_settled` | 595 / 7.664 / 7.894 | 700 / 7.848 / 11.665 | 180 |
| `api_ping` | 764 / 10.389 / 14.880 | 898 / 6.528 / 8.501 | 180 |
| `auto_detect` | 493 / 6.132 / 9.476 | 580 / 1.398 / 2.045 | 180 |
| `bin/flk` | 82031 / 0.765 / 9.909 | 102586 / 0.336 / 11.889 | 180 |
| `channel_adapter` | 357 / 2.896 / 3.468 | 420 / 0.462 / 0.523 | 180 |
| `cli_delegate_compatibility` | 731 / 10.228 / 10.377 | 860 / 10.096 / 10.237 | 180 |
| `cli_error_envelope` | 543 / 0.414 / 0.462 | 639 / 0.079 / 0.079 | 180 |
| `cli_events_wait` | 323 / 0.350 / 0.397 | 380 / 0.260 / 0.260 | 180 |
| `cli_exit_moves` | 408 / 0.098 / 0.176 | 480 / 0.016 / 0.016 | 180 |
| `cli_msg_flags` | 374 / 0.039 / 0.058 | 440 / 0.014 / 0.014 | 180 |
| `cli_notification_show` | 374 / 0.055 / 0.064 | 440 / 0.019 / 0.021 | 180 |
| `cli_owner` | 391 / 3.434 / 3.598 | 460 / 0.625 / 0.774 | 180 |
| `cli_pane_report` | 442 / 0.051 / 0.068 | 520 / 0.012 / 0.020 | 180 |
| `cli_report_body` | 459 / 0.092 / 0.143 | 540 / 0.055 / 0.075 | 180 |
| `cli_subcommand_help` | 408 / 0.281 / 0.299 | 480 / 0.109 / 0.162 | 180 |
| `cli_wait_flags` | 357 / 0.062 / 0.070 | 420 / 0.022 / 0.028 | 180 |
| `cli_wrapper` | 1394 / 3.877 / 4.394 | 1640 / 1.398 / 1.822 | 180 |
| `client_mode` | 561 / 6.860 / 7.909 | 660 / 2.662 / 3.264 | 180 |
| `cross_area` | 368 / 8.737 / 9.377 | 460 / 3.684 / 3.956 | 180 |
| `delegate` | 864 / 11.567 / 13.965 | 1080 / 7.287 / 9.131 | 180 |
| `delegate_claude` | 510 / 7.438 / 8.018 | 600 / 4.110 / 4.361 | 180 |
| `delegate_codex` | 592 / 36.863 / 37.352 | 740 / 33.976 / 34.087 | 420 |
| `delegate_r4` | 480 / 13.147 / 13.505 | 600 / 10.037 / 11.976 | 180 |
| `delegate_safety` | 512 / 34.311 / 34.521 | 640 / 30.818 / 31.105 | 360 |
| `delegate_table_hook` | 323 / 5.948 / 7.013 | 380 / 2.175 / 4.070 | 180 |
| `delegate_verdict` | 697 / 30.396 / 30.526 | 820 / 30.208 / 30.299 | 360 |
| `delegate_wait_events` | 323 / 10.351 / 11.098 | 380 / 7.199 / 9.012 | 180 |
| `detach_reattach` | 425 / 5.758 / 5.831 | 500 / 2.914 / 3.098 | 180 |
| `fleet_gossip` | 416 / 3.701 / 5.363 | 520 / 2.686 / 4.217 | 180 |
| `guarded_submit_cli` | 23 / 0.156 / 0.156 | 46 / 0.122 / 0.122 | 180 |
| `integration_sync` | 374 / 0.341 / 0.505 | 440 / 0.069 / 0.124 | 180 |
| `live_handoff` | unobserved | 1100 / 6.058 / 6.682 | 180 |
| `mcp_fleet_messaging` | 560 / 15.335 / 16.381 | 700 / 15.531 / 16.215 | 180 |
| `mcp_serve` | 391 / 3.957 / 4.017 | 460 / 0.907 / 1.247 | 180 |
| `mesh_collection` | 646 / 9.383 / 9.940 | 760 / 7.692 / 10.762 | 180 |
| `mesh_delivery` | 510 / 20.769 / 64.725 | 600 / 63.390 / 63.689 | 660 |
| `mesh_enrollment` | 672 / 7.133 / 19.752 | 840 / 6.303 / 7.275 | 180 |
| `mesh_forwarding` | 884 / 25.996 / 30.610 | 1040 / 22.447 / 24.853 | 300 |
| `mesh_identity` | 459 / 3.519 / 3.836 | 540 / 2.769 / 2.957 | 180 |
| `mesh_routes` | 459 / 20.416 / 21.652 | 540 / 18.476 / 19.444 | 240 |
| `mesh_step1` | 768 / 19.239 / 21.840 | 960 / 16.453 / 20.942 | 240 |
| `mesh_step2` | 752 / 36.409 / 38.149 | 940 / 36.354 / 37.514 | 420 |
| `mesh_tombstones` | 527 / 7.920 / 8.947 | 620 / 7.401 / 7.471 | 180 |
| `multi_client` | 476 / 12.053 / 12.383 | 560 / 2.415 / 2.894 | 180 |
| `owner_policy` | 323 / 3.720 / 4.604 | 380 / 0.416 / 0.431 | 180 |
| `peer_federation` | 374 / 9.399 / 9.454 | 440 / 3.699 / 3.862 | 180 |
| `question_hook` | 408 / 10.234 / 10.381 | 480 / 10.023 / 10.025 | 180 |
| `remote_bridge` | 323 / 0.429 / 0.749 | 380 / 0.259 / 0.260 | 180 |
| `server_environment` | 391 / 0.061 / 0.082 | 460 / 0.007 / 0.008 | 180 |
| `server_headless` | 510 / 5.175 / 5.336 | 600 / 4.381 / 4.632 | 180 |
| `server_readiness` | 280 / 0.039 / 0.043 | 356 / 0.007 / 0.007 | 180 |
| `session_health` | 408 / 13.251 / 13.449 | unobserved | 180 |
