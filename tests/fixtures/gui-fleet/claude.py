"""Deterministic, offline stream-json model for GUI fleet tests. See README.md."""
import atexit
import json
import os
from pathlib import Path
import re
import socket
import sys
import uuid


def require(condition, message):
    if not condition:
        raise ValueError(message)


def decode(text):
    try:
        return json.loads(text)
    except (ValueError, TypeError) as error:
        raise ValueError("invalid JSON") from error


def store(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(".tmp")
    with temporary.open("w") as output:
        json.dump(value, output)
        output.flush()
        os.fsync(output.fileno())
    temporary.replace(path)
    directory = os.open(path.parent, os.O_RDONLY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def main():
    if sys.argv[1:] == ["--version"]:
        print("2.1.296 (Claude Code)")
        return
    flags = [flag for flag in ("--session-id", "--resume") if flag in sys.argv]
    require(len(flags) == 1, "exactly one of --session-id or --resume is required")
    flag = flags[0]
    require(sys.argv.count(flag) == 1, "duplicate session flag")
    session = str(uuid.UUID(sys.argv[sys.argv.index(flag) + 1]))
    home = Path(os.environ["HOME"])
    (home / "gui-fleet-error.json").unlink(missing_ok=True)
    script = decode((home / "gui-fleet.json").read_text())
    require(isinstance(script, list), "script must be a list")
    for step in script:
        require(isinstance(step, dict), "unknown script step")
        operation = (step.get("expect"), step.get("emit"))
        fields = {
            ("message", None): {"expect", "text"},
            ("decision", None): {"expect", "behavior"},
            ("gate", None): {"expect", "name"},
            (None, "stream"): {"emit", "chunks"},
            (None, "permission"): {"emit", "request_id", "input"},
            (None, "reply"): {"emit", "text"},
        }.get(operation)
        require(fields is not None, "unknown script step")
        require(set(step) == fields, "unknown or missing script field")
        if step.get("expect") == "gate":
            require(isinstance(step["name"], str) and bool(step["name"]), "invalid gate name")
        if "text" in step:
            require(isinstance(step["text"], str), "script text must be a string")
        if step.get("emit") == "stream":
            require(isinstance(step["chunks"], list) and bool(step["chunks"])
                    and all(isinstance(chunk, str) for chunk in step["chunks"]), "invalid stream chunks")
        if step.get("expect") == "decision":
            require(step["behavior"] in ("allow", "deny"), "invalid decision behavior")
        if step.get("emit") == "permission":
            require(isinstance(step["request_id"], str) and bool(step["request_id"])
                    and isinstance(step["input"], dict), "invalid permission step")
    state = Path(os.environ["CLAUDE_CONFIG_DIR"]) / "gui-fleet" / f"{session}.json"
    cursor = 0
    if flag == "--resume":
        saved = decode(state.read_text())
        require(saved["script"] == script, "resume script changed")
        cursor = saved["cursor"]
        require(type(cursor) is int and 0 <= cursor <= len(script), "invalid resume cursor")
    else:
        require(not state.exists(), "session already exists; use --resume")
        store(state, {"cursor": cursor, "script": script})
    launches = home / "gui-fleet-launches.json"
    history = decode(launches.read_text()) if launches.exists() else []
    history.append({"session_id": session, "resume": flag == "--resume"})
    store(launches, history)

    def emit(value):
        print(json.dumps(dict(value, session_id=session)), flush=True)

    def receive():
        line = sys.stdin.readline()
        require(bool(line), "unexpected EOF while awaiting script input")
        value = decode(line)
        require(isinstance(value, dict), "input must be an object")
        return value

    emit({"type": "system", "subtype": "init", "model": "fixture-model",
          "claude_code_version": "2.1.296", "tools": ["Bash"],
          "permissionMode": "default", "capabilities": [], "mcp_servers": [],
          "skills": [], "slash_commands": []})
    listener = None
    if any(step.get("expect") == "gate" for step in script):
        endpoint = home / f"g-{session}"
        endpoint.unlink(missing_ok=True)
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(str(endpoint))
        listener.listen(1)
        def close_gate():
            listener.close()
            endpoint.unlink(missing_ok=True)
        atexit.register(close_gate)
    pending = None
    active = False
    for index in range(cursor, len(script)):
        step = script[index]
        if step.get("expect") == "gate":
            connection, _ = listener.accept()
            with connection, connection.makefile("rb") as channel:
                line = channel.readline(4097)
                require(line.endswith(b"\n") and len(line) <= 4096, "invalid gate input")
                require(decode(line) == step["name"], "unexpected gate name")
        elif step.get("expect") == "message":
            require(not active, "message before previous reply")
            value = receive()
            require(value.get("type") == "user", "expected user message")
            message = value.get("message", {})
            content = message.get("content")
            require(message.get("role") == "user" and isinstance(content, list)
                    and all(isinstance(block, dict) and block.get("type") == "text"
                            and isinstance(block.get("text"), str) for block in content), "malformed user message")
            text = "".join(block["text"] for block in content)
            framed = re.search(r"\n<<<flk-item ([0-9a-f]{32})\n(.*)\nflk-item \1>>>\Z", text, re.DOTALL)
            require((framed[2] if framed else text) == step["text"], "unexpected user text")
            active = True
            emit({"type": "command_lifecycle", "command_uuid": value["uuid"], "state": "started"})
        elif step.get("emit") == "stream":
            require(active and pending is None, "stream outside active turn")
            emit({"type": "stream_event", "event": {"type": "content_block_start", "index": 0,
                  "content_block": {"type": "text", "text": ""}}})
            for chunk in step["chunks"]:
                emit({"type": "stream_event", "event": {"type": "content_block_delta", "index": 0,
                      "delta": {"type": "text_delta", "text": chunk}}})
            emit({"type": "stream_event", "event": {"type": "content_block_stop", "index": 0}})
        elif step.get("emit") == "permission":
            require(active and pending is None, "permission outside active turn")
            pending = step["request_id"]
            emit({"type": "control_request", "request_id": pending, "request": {
                "subtype": "can_use_tool", "tool_name": "Bash", "tool_use_id": pending,
                "input": step["input"], "requires_user_interaction": True, "permission_suggestions": []}})
        elif step.get("expect") == "decision":
            require(pending is not None, "decision without permission")
            value = receive()
            require(value.get("type") == "control_response", "expected control_response")
            response = value["response"]
            require(response["subtype"] == "success" and response["request_id"] == pending,
                    "decision request mismatch")
            require(response["response"]["behavior"] == step["behavior"], "unexpected decision behavior")
            pending = None
        elif step.get("emit") == "reply":
            require(active and pending is None, "reply before message or decision")
            store(state, {"cursor": index + 1, "script": script})
            emit({"type": "assistant", "message": {"role": "assistant", "content": [
                {"type": "text", "text": step["text"]}]}})
            emit({"type": "result", "subtype": "success", "is_error": False, "result": step["text"]})
            active = False
    require(not active and pending is None, "unfinished scripted turn")
    require(not sys.stdin.readline(), "unexpected input after script completed")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, IndexError, TypeError, OSError) as error:
        print(f"gui-fleet fixture: {error}", file=sys.stderr, flush=True)
        try:
            store(Path(os.environ["HOME"]) / "gui-fleet-error.json", {"error": str(error)})
        except (KeyError, OSError):
            pass
        sys.exit(1)
