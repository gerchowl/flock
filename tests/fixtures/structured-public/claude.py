"""Owned fake harness for the public structured admission dogfood (#1843)."""
import json
import os
from pathlib import Path
import select
import socket
import stat
import subprocess
import sys

root = Path(os.environ["HOME"])
if sys.argv[1:] == ["--version"]:
    print("2.1.296 (Claude Code)")
    sys.exit(0)
assert Path.cwd() == root / "project", "launch cwd was not preserved"
allowed = {"PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TMPDIR",
           "CLAUDE_CONFIG_DIR", "CLAUDE_CODE_OAUTH_TOKEN"}
environment = set(os.environ)
# Darwin's Python interpreter adds its own locale marker after exec.
if sys.platform == "darwin":
    environment.discard("__CF_USER_TEXT_ENCODING")
assert environment == allowed, "model launch environment is not the explicit allowlist"
assert "FLOCK_MCP_LEASE_CREDENTIAL" not in os.environ, "lease secret in model environment"
assert not any(key.startswith("FLOCK_") for key in os.environ), "unlisted model environment"
assert "--restricted" in sys.argv and "--strict-mcp-config" in sys.argv
assert os.getpgrp() == os.getpid(), "unowned process group"
session = sys.argv[sys.argv.index("--session-id") + 1]
model = sys.argv[sys.argv.index("--model") + 1]
config_path = Path(sys.argv[sys.argv.index("--mcp-config") + 1])
assert stat.S_IMODE(config_path.stat().st_mode) == 0o600, "MCP config is not private"
config = json.loads(config_path.read_text())
server = config["mcpServers"]["flock"]
assert set(server["env"]) == {"FLOCK_SOCKET_PATH", "FLOCK_MCP_LEASE_CREDENTIAL"}
assert Path(server["env"]["FLOCK_SOCKET_PATH"]).is_absolute()
assert Path(server["command"]).is_absolute()
assert server["args"] == ["mcp", "serve", "--structured"]
profile = Path(os.environ["CLAUDE_CONFIG_DIR"])
(profile / "sessions").mkdir(exist_ok=True, mode=0o700)
(profile / "sessions" / f"{os.getpid()}.json").write_text("{}")
print(json.dumps({"type": "system", "subtype": "hook_started", "hook_id": "fixture", "hook_name": "fixture", "hook_event": "SessionStart"}), flush=True)
if model == "dogfood-parent":
    env = dict(os.environ, **server["env"])
    tool = subprocess.Popen([server["command"], *server["args"]], env=env,
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    def call(request):
        tool.stdin.write((json.dumps(request) + "\n").encode())
        tool.stdin.flush()
        assert select.select([tool.stdout], [], [], 10)[0], "MCP response missing"
        response = json.loads(tool.stdout.readline())
        assert response["id"] == request["id"], "MCP response order"
        return response
    listener = socket.socket(socket.AF_UNIX)
    try:
        listener.bind(str(root / "control"))
        listener.listen(1)
        with open(root / "ready", "wb", buffering=0) as ready:
            ready.write(b"R")
        listener.settimeout(15)
        control, _ = listener.accept()
        control.settimeout(15)
        with control, control.makefile("rwb", buffering=0) as channel:
            for line in channel:
                request = json.loads(line)
                if request == "close":
                    break
                if isinstance(request, dict) and "fixture_cli" in request:
                    cli_env = dict(os.environ, FLOCK_SOCKET_PATH=server["env"]["FLOCK_SOCKET_PATH"])
                    result = subprocess.run([server["command"], "--json", *request["fixture_cli"]],
                                            env=cli_env, capture_output=True, text=True, timeout=10)
                    response = {"exit": result.returncode, "response": json.loads(result.stdout)}
                else:
                    response = call(request)
                control.sendall((json.dumps(response) + "\n").encode())
    finally:
        listener.close()
        tool.stdin.close()
        try:
            assert tool.wait(timeout=10) == 0, "MCP exit failure"
        except subprocess.TimeoutExpired:
            tool.kill()
            tool.wait(timeout=10)
            raise AssertionError("MCP failed to close")
        tool.stdout.close()
else:
    seen = root / f"seen-{session}"
    if seen.exists():
        init = json.loads((root / "init.json").read_text())
        init["session_id"] = session
        print(json.dumps(init), flush=True)
        with open(root / "resume-ready", "wb", buffering=0) as ready:
            ready.write(b"R")
        sys.stdin.read()
        sys.exit(0)
    seen.write_text("first launch")
    line = sys.stdin.readline()
    assert line, "opening turn was never delivered"
    user = json.loads(line)
    content = "".join(block["text"] for block in user["message"]["content"])
    assert "UNTRUSTED TASK" in content and "dogfood-task" in content, "missing framed opening turn"
    init = json.loads((root / "init.json").read_text())
    init["session_id"] = session
    print(json.dumps(init), flush=True)
    print(json.dumps({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "dogfood-complete"}]}}), flush=True)
    print(json.dumps({"type": "result", "subtype": "success", "is_error": False, "result": "dogfood-complete", "session_id": session}), flush=True)
    with open(root / "turn-ready", "wb", buffering=0) as ready:
        ready.write(b"T")
    sys.stdin.read()
