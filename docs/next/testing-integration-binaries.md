| Proposed binary | Current binaries or modules | Safety gate before a later merge | CI constraints to preserve |
| --- | --- | --- | --- |
| `gui_build` | `gui_bundle_build`, `gui_feature_gate` | Merged in this slice: no environment/cwd mutation, signal hooks, listeners or constructors; one module owns its process-local sandbox cleanup | `unit-2`, zero retries, both web feature variants in `just gui-check` |
| `external_cli_acceptance` | `attach_welcome`, `fork_resume_messaging` | Rust wrappers have no shared state; retain bounded subprocess isolation and audit both Python drivers | `unit-2` |
| `cli_contract` | `cli_argv`, `cli_error_envelope`, `cli_events_wait`, `cli_exit_moves`, `cli_msg_flags`, `cli_notification_show`, `cli_pane_report`, `cli_report_body`, `cli_subcommand_help`, `cli_wait_flags` | Audit shared cleanup/signal hooks and fixed absent-socket names first; retain every copy of support tests | `unit-2` |
| `cli_authority` | `cli_owner`, `cli_structured_grants`, `owner_policy` | Audit shared cleanup/signal hooks and fixture lifetimes first | `unit-2` |
| `cli_delegate_contract` | `cli_delegate_compatibility`, `guarded_submit_cli` | Audit shared cleanup/signal hooks and scripted peer lifetimes first | `unit-2` |
| `cli_security` | `agent_name_security`, `config_sink_security`, `permission_principal`, `profile_bindings` | Audit shared cleanup/signal hooks and compile-probe isolation first | `unit-2` |
| `delegate_core` | `delegate`, `delegate_codex`, `delegate_r4`, `delegate_safety` | Audit shared cleanup/signal hooks, fake harnesses and module-relative fixture paths first | `unit-1`; retain module-scoped timeout, retry and leak overrides |
| `delegate_hooks` | `delegate_backfill`, `delegate_base`, `delegate_table_hook`, `delegate_terminal_change`, `delegate_wait_events` | Audit shared cleanup/signal hooks and producer ownership first | `unit-2` |
| `delegate_claude_contract` | `delegate_claude`, `delegate_verdict`, `question_hook` | Audit shared cleanup/signal hooks and subprocess protocols first | `unit-2`; preserve verdict test's timeout override |
| `mcp_contract` | `mcp_serve`, `mcp_native_reply`, `mcp_structured_messaging` | Audit shared cleanup/signal hooks and per-binary locks first | `unit-2`; retain zero retries for native replies |
| `gui_fixture` | `gui_fleet_fixture`, `scripted_command` | Audit shared cleanup/signal hooks and scripted subprocess isolation first | `unit-2` |
| `headless_lifecycle` | `auto_detect`, `client_mode`, `detach_reattach`, `multi_client`, `server_environment`, `server_headless`, `server_readiness` | Audit shared cleanup/signal hooks, per-binary locks and daemon cleanup first | `unit-2`; retain readiness timeout override |
| `local_acceptance` | `agent_settled`, `channel_adapter`, `integration_sync`, `remote_bridge`, `s7_conformance`, `structured_public` | Audit shared cleanup/signal hooks, adapter initialization and fixture names first | `unit-2`; retain settled timeout override |
| `mesh_forwarding_contract` | `mesh_forwarding`, `mesh_routes` | Audit shared cleanup/signal hooks and fleet ownership first | `serial-1`; retain debug-only relay gates and forwarding zero retries |
| `mesh_enrollment_contract` | `mesh_enrollment`, `mesh_step1`, `mcp_fleet_messaging` | Audit shared cleanup/signal hooks and fleet ownership first | `serial-2`; retain debug-only relay gates and step1 zero retries |
| `mesh_recovery_contract` | `mesh_step2`, `fleet_gossip`, `cross_area` | Audit shared cleanup/signal hooks, per-binary locks and static fixture counters first | `serial-3`; retain step2 zero retries and the other modules' retries |
| `mesh_mailbox_contract` | `mesh_collection`, `mesh_identity`, `mesh_tombstones` | Audit shared cleanup/signal hooks, fleet ownership and static fixture counters first | `unit-2`; retain individual debug-only relay gates |
| `api_ping` | `api_ping` | Keep separate: parent-process environment mutation and per-binary lock | `unit-2`, existing timeout |
| `cli_wrapper` | `cli_wrapper` | Keep separate: parent-process environment mutation | `unit-2`, existing timeout |
| `client_tokens` | `client_tokens` | Keep separate: token locks, mount fixture and listener ownership | `unit-2`, zero retries |
| `fleet_api` | `fleet_api` | Keep separate: direct socket helper tests have no related hook-free partner | `unit-2` |
| `gui_e2e_fleet` | `gui_e2e_fleet` | Keep separate: dedicated browser acceptance driver | Dedicated browser job, excluded from ordinary shards |
| `live_handoff` | `live_handoff` | Keep separate: replacement-server and cleanup lifecycle | `serial-3`, macOS exclusion, existing timeout |
| `mesh_delivery` | `mesh_delivery` | Keep separate: measured delivery hang ceiling | `unit-2`, existing 660-second ceiling |
| `peer_federation` | `peer_federation` | Keep separate: federation fixture and PTY serialization | `serial-1` |
| `sandbox_sweep` | `sandbox_sweep` | Keep separate: cleanup registry and process-identity acceptance | `unit-2` |
| `session_health` | `session_health` | Keep separate: native session-loss sandbox | `unit-2`, existing macOS gate |
| `structured_tui` | `structured_tui` | Keep separate: real TUI acceptance and permission driver | `serial-1`, existing serialization override |
