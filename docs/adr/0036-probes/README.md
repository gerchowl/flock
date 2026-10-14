# ADR-0036 S0 probes (#1307)

Drivers behind ADR-0036 Appendix A.11 to A.17. Each one starts throwaway
`claude -p` stream-json sessions with the isolation of `s0lib.py` (no
`FLOCK_*` / `CLAUDE_CODE_*` variables, a temporary working directory,
`--setting-sources local --strict-mcp-config --no-session-persistence`) and
prints a JSON summary. They never start, reach or restart a flk server.

```sh
export CLAUDE_PROFILE_DIR=~/.claude-profiles/work   # a logged-in profile (E6)
export S0_OUT=/tmp/s0                                # raw captures, not committed
python3 -I docs/adr/0036-probes/s0_socket.py     # A.11 the messaging socket
python3 -I docs/adr/0036-probes/s0_carrier.py 4  # A.12 deny vs allow carrier
python3 -I docs/adr/0036-probes/s0_parallel.py 2 # A.13 parallel control requests
python3 -I docs/adr/0036-probes/s0_queue.py      # A.14 the harness's own queue
python3 -I docs/adr/0036-probes/s0_permmode.py   # A.15 set_permission_mode
sh docs/adr/0036-probes/s0_login_flk_agent.sh    # A.16, run by the operator AS the agent user
```

`s0_mcpstub.py` is the one-tool MCP server the socket and parallel probes
attach. `S0_MODEL` (default `haiku`) and `S0_CLAUDE` (default `claude`)
override the model and binary.

The runs leave empty `projects/<cwd>/memory/` and `session-env/<id>/`
directories in the profile even with `--no-session-persistence`; remove
them afterwards by the session ids in the captures.
