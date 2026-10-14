# ADR-0036 S0 probes (#1307)

Drivers behind ADR-0036 Appendix A.11 to A.17. Each one starts throwaway
`claude -p` stream-json sessions with the isolation of `s0lib.py` and prints
a JSON summary. They never start, reach or restart a flk server, and never
use a real Claude profile:

- every `FLOCK_*` / `CLAUDE_CODE_*` variable (and `CLAUDECODE`, `CLAUDE_PID`,
  `CLAUDE_EFFORT`) is removed from the child's environment,
- each session gets a fresh temporary `CLAUDE_CONFIG_DIR` (0700), which
  `Session.close()` deletes with everything the harness wrote into it
  (session registry and inbox key, `session-env/`, `projects/`, `plans/`),
- the working directory is a fresh temporary directory, also deleted,
- `--setting-sources local --strict-mcp-config --no-session-persistence`,
- `--permission-mode default`: without it, a session with an empty config
  dir started in `auto` and nothing prompted (ADR-0036 A.17, #1404).

An empty config dir is not logged in (E6), so the login is passed in as
`CLAUDE_CODE_OAUTH_TOKEN`. Get one with `claude setup-token` (once, in a
terminal), store it in a 0600 file and point `S0_OAUTH_TOKEN_FILE` at it.
Exporting `CLAUDE_CODE_OAUTH_TOKEN` works too. The drivers never print the
token or write it to a capture, and refuse to start without one.

```sh
export S0_OAUTH_TOKEN_FILE=<path to the 0600 token file>
export S0_OUT=/tmp/s0                            # raw captures, not committed
python3 -I -B docs/adr/0036-probes/s0_socket.py     # A.11 the messaging socket
python3 -I -B docs/adr/0036-probes/s0_carrier.py 4  # A.12 deny vs allow carrier
python3 -I -B docs/adr/0036-probes/s0_parallel.py 2 # A.13 parallel control requests (+ controls)
python3 -I -B docs/adr/0036-probes/s0_parallel.py 2 10 read_behind_permission # A.13 a Read behind an open permission
python3 -I -B docs/adr/0036-probes/s0_queue.py      # A.14 the harness's own queue
python3 -I -B docs/adr/0036-probes/s0_permmode.py   # A.15 set_permission_mode
sh docs/adr/0036-probes/s0_login_flk_agent.sh       # A.16, run by the operator AS the agent user
```

`s0_mcpstub.py` is the one-tool MCP server the socket and parallel probes
attach. `S0_MODEL` (default `haiku`) and `S0_CLAUDE` (default `claude`)
override the model and binary. The scripts take scenario names as
arguments and have no `--help`: any argument is run as a scenario.

The numbers in A.11 to A.15 were recorded before this isolation was in
place, with `CLAUDE_CONFIG_DIR` set to the operator's profile (see the
Appendix A S0 preamble). Of them, only A.13's `sleep_then_mcp` and
`slowtouch_then_mcp` controls have been re-run under the token path
(2026-10-14).
