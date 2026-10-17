# Test build profiles

The cleanup sweep compares canonical executable paths against
`env!("CARGO_BIN_EXE_flk")`, exactly as the fixtures launch the CLI. It accepts
that build and symlinks to it, including release/custom target layouts, and
rejects another file with the same basename or another worktree's build.
Only registered fixture runtime directories can authorize cleanup.

State/config fixture paths use `support::paths::app_dir_name()`: `flock-dev`
with debug assertions and `flock` otherwise. The manual Claude question probe
requires the matching `--app-dir` explicitly; Rust callers pass that helper's
value. A binary's filename or directory spelling does not determine its namespace.
The automated probe test intercepts integration installation before any harness
or model is launched and checks both renamed and misleading `debug/` paths.

## Fake SSH relay scope

The local fake SSH relay needs `FLOCK_TEST_RELAY_ANCESTOR=flk` in place of real
sshd ancestry. The product deliberately honors this substitution only with debug
assertions (`src/app/api/edges.rs::relay_ancestor_override`); release must never
accept that environment variable as remote-caller authority. Every test using
this substitution has its own documented `#[cfg(debug_assertions)]` gate.
Single-node and unrelated tests remain available in release, including the
actual detached-daemon reaping and cleanup matcher tests.

The debug-only relay inventory is 182 tests (#1715):

| Binary | Debug-only tests |
| --- | ---: |
| `fleet_gossip` | 5 |
| `mesh_step2` | 31 |
| `mesh_collection` | 17 |
| `mesh_delivery` | 10 |
| `mesh_enrollment` | 22 |
| `mesh_identity` | 4 |
| `mesh_tombstones` | 3 |
| `mesh_routes` | 9 |
| `mesh_forwarding` | 34 |
| `mesh_step1` | 30 |
| `mcp_fleet_messaging` | 17 |

Run the affected binaries with `cargo nextest run --release --retries 0`; the
remaining tests must pass. The debug inventory retains these 182 tests. This
restriction is a fixture limit, not a platform gate or a product release override.

## Six original release failures

The architect's #1715 comment records the original #1706 run at `5b0e6678`.
These are historical observations, distinct from this change's validation:

| Test | Observed failure | Mechanism |
| --- | --- | --- |
| `auto_detect::auto_detect_daemon_is_reaped_by_the_runtime_dir_sweep` | 14.740 s, daemon not visible to sweep | The fixture launched Cargo's release binary; the matcher compared only `target/debug/flk`. The assertion failed before proving reaping. |
| `mcp_fleet_messaging::an_agent_discovers_and_messages_another_host_through_mcp_alone` | 31.241 s, agent-directory discovery timed out | The local fake SSH chain requires the debug-only ancestry substitution; release ignores it, so discovery fails before the MCP assertion. |
| `mesh_delivery::cross_host_send_and_boot_projection_preserve_exactly_one_inbox_import` | 30.772 s, mesh route status timed out | The same fake SSH pair cannot establish its release relay, before inbox import or restart assertions. |
| `mesh_collection::incident_direct_reply_is_held_and_collected_without_a_waiter` | 90.924 s, recipient discovery timed out | The same fake SSH pair fails discovery before held-reply collection. |
| `mesh_tombstones::reply_to_removed_sender_lands_in_origin_status_not_a_replacement_inbox` | 30.904 s, mesh route status timed out | The same pair fails setup before sender removal/replacement or the reply assertion. |
| `mesh_enrollment::peer_restart_reenrolls_promptly_after_missing_server_hello` | 90.484 s, mesh enrollment timed out | The same pair cannot initially enroll, before peer restart or recovery. |

The five relay attributions follow the shared fixture and ignored override;
they are not five separately demonstrated product regressions. A seventh
reported failure, `mesh_forwarding::chain_request_is_forwarded_and_imported_once`,
was cancellation (exit 130 after 17.121 s), not an independently observed failure.
Six selected tests did not run. The original selection passed 40/53 before
cancellation (exit 100); #1706 separately reported 43/43 supported release cases.
