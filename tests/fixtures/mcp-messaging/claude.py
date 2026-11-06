"""Fake harness: actual session lifecycle and real registered MCP child."""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import threading

if sys.argv[1:] == ["--version"]:
    print("2.1.296 (Claude Code)")
    sys.exit(0)
structured = "--mcp-config" in sys.argv
if structured:
    registration = json.loads(Path(sys.argv[sys.argv.index("--mcp-config") + 1]).read_text())["mcpServers"]["flock"]
    endpoint = Path(os.environ["HOME"]) / sys.argv[sys.argv.index("--model") + 1]
    session = sys.argv[sys.argv.index("--session-id") + 1]
    def emit(value):
        print(json.dumps(value), flush=True)
    emit({"type":"system", "subtype":"hook_started", "hook_id":"fixture", "hook_name":"fixture", "hook_event":"SessionStart"})
    emit({"type":"system", "subtype":"init", "model":"fixture", "claude_code_version":"2.1.296", "tools":[], "session_id":session})
    def consume():
        for line in sys.stdin:
            value = json.loads(line)
            assert value["type"] == "user", value
            with endpoint.with_suffix(".inputs").open("a") as log:
                log.write(json.dumps(value) + "\n")
            emit({"type":"command_lifecycle", "command_uuid":value["uuid"], "state":"started", "session_id":session})
            emit({"type":"result", "subtype":"success", "is_error":False, "result":"ok", "session_id":session})
    threading.Thread(target=consume, daemon=True).start()
else:
    registration = {"command":sys.argv[1], "args":["mcp", "serve"], "env":{}}
    endpoint = Path(sys.argv[2])
with socket.socket(socket.AF_UNIX) as control:
    control.connect(str(endpoint))
    tool = subprocess.Popen([registration["command"], *registration["args"]],
        env=dict(os.environ, **registration["env"]), stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    try:
        with control.makefile("rb") as commands:
            for line in commands:
                tool.stdin.write(line)
                tool.stdin.flush()
                response = tool.stdout.readline()
                assert response, "MCP child closed"
                control.sendall(response)
    finally:
        tool.stdin.close()
        try:
            assert tool.wait(timeout=10) == 0
        except subprocess.TimeoutExpired:
            tool.kill()
            tool.wait(timeout=10)
            raise AssertionError("owned MCP child did not close")
