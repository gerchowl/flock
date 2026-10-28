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
# Composer tests need a ready fake session; queue admission is observed separately.
if "--model" in sys.argv and sys.argv[sys.argv.index("--model") + 1] == "fixture-worker-model":
    init = json.loads((root / "restricted-init.json").read_text())
    init["session_id"] = sys.argv[sys.argv.index("--session-id") + 1]
    print(json.dumps(init), flush=True)
    sys.stdin.read()
    sys.exit(0)
# Explicit test-only model selects partial streaming; ordinary launches bypass this.
if "--model" in sys.argv and sys.argv[sys.argv.index("--model") + 1].startswith("s10b-live"):
    mode = sys.argv[sys.argv.index("--model") + 1]
    def emit_live(value):
        print(json.dumps(value), flush=True)
    def emit_block(index, kind, field, text, identity=None):
        envelope = {"type": "stream_event"}
        if identity is not None:
            envelope["api_message_id"] = identity
        if "lifecycle" in mode:
            emit_live(dict(envelope, event={"type": "content_block_start", "index": index,
                      "content_block": {"type": "thinking" if field == "thinking" else "text", field: ""}}))
        emit_live(dict(envelope, event={"type": "content_block_delta", "index": index,
                  "delta": {"type": kind, field: text}}))
        if "lifecycle" in mode:
            emit_live(dict(envelope, event={"type": "content_block_stop", "index": index}))
    init = json.loads((root / "restricted-init.json").read_text())
    init["session_id"] = sys.argv[sys.argv.index("--session-id") + 1]
    emit_live(init)
    if "interleaved" in mode:
        for identity, text in [("first", "partial"), ("second", "later")]:
            emit_block(0, "text_delta", "text", text, identity)
    else:
        for index in range(40 if "burst" in mode else 3):
            kind, field, text = [("text_delta", "text", "partial"),
                                 ("thinking_delta", "thinking", "thinking"),
                                 ("text_delta", "text", "tool-result")][index % 3]
            emit_block(index, kind, field, text)
    emit_live({"type": "assistant", "message": {"id": "first", "role": "assistant", "content": [
        {"type": "text", "text": "partial"}, {"type": "thinking", "thinking": "thinking"}]}})
    emit_live({"type": "user", "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "fixture-tool", "content": "tool-result"}]}})
    if mode.endswith("exit"):
        sys.exit(0)
    sys.stdin.read()
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
if model == "s10f-mvp":
    # Deliberately gated turn: the public TUI must queue until this producer idles.
    import signal
    tool = subprocess.Popen([server["command"], *server["args"]],
                            env=dict(os.environ, **server["env"]),
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    tool.stdin.write(b'{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}\n')
    tool.stdin.flush()
    assert select.select([tool.stdout], [], [], 10)[0], "MCP initialization missing"
    assert json.loads(tool.stdout.readline())["id"] == 1
    def emit(value):
        print(json.dumps(dict(value, session_id=session)), flush=True)
    def finish():
        emit({"type": "result", "subtype": "success", "is_error": False, "result": "turn complete"})
    def begin():
        emit({"type": "command_lifecycle", "command_uuid": "00000000-0000-4000-8000-000000002495", "state": "started"})
        emit({"type": "stream_event", "api_message_id": "live-turn", "event": {
            "type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}})
        emit({"type": "stream_event", "api_message_id": "live-turn", "event": {
            "type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "streaming-marker"}}})
    def terminate(_signal, _frame):
        raise SystemExit(0)
    signal.signal(signal.SIGTERM, terminate)
    listener = socket.socket(socket.AF_UNIX)
    listener.bind(str(root / "control"))
    listener.listen(1)
    init = json.loads((root / "init.json").read_text())
    init["session_id"] = session
    print(json.dumps(init), flush=True)
    emit({"type": "assistant", "message": {"role": "assistant", "content": [
        {"type": "text", "text": "native-transcript"},
        {"type": "tool_use", "id": "fixture-tool", "name": "Bash", "input": {"command": "printf tool-card"}}]}})
    emit({"type": "user", "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "fixture-tool", "content": "tool-card-result"}]}})
    with open(root / "ready", "wb", buffering=0) as ready:
        ready.write(b"R")
    inputs = []
    try:
        listener.settimeout(15)
        control, _ = listener.accept()
        control.settimeout(15)
        with control, control.makefile("rb", buffering=0) as commands:
            control.sendall((json.dumps({"pid": os.getpid(), "mcp_pid": tool.pid,
                "stdin_tty": os.isatty(0), "stdout_tty": os.isatty(1),
                "temporary": str(config_path.parent), "profile": str(profile)}) + "\n").encode())
            while True:
                readable, _, _ = select.select([sys.stdin, commands], [], [], 15)
                assert readable, "acceptance driver stopped servicing the fake"
                if commands in readable:
                    line = commands.readline()
                    if not line:
                        break
                    command = json.loads(line)
                    if command == "finish":
                        finish()
                    elif command == "begin":
                        begin()
                    elif command in ("permission-deny", "permission-allow"):
                        emit({"type": "control_request", "request_id": command, "request": {
                            "subtype": "can_use_tool", "tool_name": "Bash", "tool_use_id": "fixture-tool",
                            "input": {"command": "true"}, "requires_user_interaction": True,
                            "permission_suggestions": []}})
                    else:
                        assert command == "inspect", command
                    control.sendall((json.dumps(inputs) + "\n").encode())
                if sys.stdin in readable:
                    line = sys.stdin.readline()
                    if not line:
                        break
                    value = json.loads(line)
                    inputs.append(value)
                    if value["type"] == "user":
                        text = "".join(b.get("text", "") for b in value["message"]["content"])
                        answer = "composer-answer" if "composer-message" in text else "shell-answer"
                        emit({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": answer}]}})
                        finish()
                    else:
                        assert value["type"] == "control_response", value
                        emit({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": value["response"]["request_id"] + "-answer"}]}})
                        finish()
    finally:
        listener.close()
        tool.stdin.close()
        mcp_exit = tool.wait(timeout=10)
        tool.stdout.close()
        error = sys.exc_info()[1]
        model_exit = error.code if isinstance(error, SystemExit) else (0 if error is None else 1)
        (root / "stopped.json").write_text(json.dumps({"model_exit": model_exit, "mcp_exit": mcp_exit}))
    sys.exit(0)
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
        (root / "control").unlink()
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
    if model == "dogfood-wire-child":
        request_number = 0
        pending_requests = set()
        def emit(value):
            print(json.dumps(dict(value, session_id=session)), flush=True)
        for line in sys.stdin:
            value = json.loads(line)
            if value.get("type") == "user":
                emit({"type": "command_lifecycle", "command_uuid": value["uuid"], "state": "started"})
                emit({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "wire-send-observed"}]}})
                disabled = (root / "proof-disable").read_text() if (root / "proof-disable").exists() else "none"
                if disabled == "permission":
                    continue
                # Retained permission IDs cannot be reused within a transport.
                request_number += 1
                request_id = "typed-request" if request_number == 1 else f"typed-request-{request_number}"
                pending_requests.add(request_id)
                emit({"type": "control_request", "request_id": request_id, "request": {
                    "subtype": "can_use_tool", "tool_name": "Bash", "tool_use_id": "tool-example",
                    "input": {"command": "true"}, "requires_user_interaction": True, "permission_suggestions": []}})
            elif value.get("type") == "control_response":
                pending_requests.remove(value["response"]["request_id"])
                emit({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "wire-decision-observed"}]}})
                if not (root / "proof-disable").exists() or (root / "proof-disable").read_text() != "result":
                    emit({"type": "result", "subtype": "success", "is_error": False, "result": "wire-complete"})
    else:
        sys.stdin.read()
