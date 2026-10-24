"""Owned fake Claude. Shapes reconstructed from the committed S1 catalogue."""
import json
import os
import pathlib
import signal
import select
import socket
import stat
import subprocess
import time
import sys

root = pathlib.Path(__file__).parent
case = json.loads((root / "case.json").read_text())
def parent_gone():
    fd = os.open(root / "lifetime", os.O_RDONLY | os.O_NONBLOCK)
    select.select([fd], [], [])
    os.close(fd)

allowed = {"PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TMPDIR",
           "CLAUDE_CONFIG_DIR", "CLAUDE_CODE_OAUTH_TOKEN"}
# Darwin's Python startup adds this CoreFoundation locale marker after exec.
# It is interpreter metadata, not an inherited launch variable or credential.
environment = set(os.environ)
if sys.platform == "darwin":
    environment.discard("__CF_USER_TEXT_ENCODING")
assert os.getpgrp() == os.getpid(), "missing process_group(0)"
if sys.argv[1:] == ["--version"]:
    assert environment == allowed - {"CLAUDE_CODE_OAUTH_TOKEN"}, "version environment leak"
    (root / "version.pid").write_text(str(os.getpid()))
    if case.get("version") == "hang":
        parent_gone()
        sys.exit(9)
    if case.get("version") == "fail":
        sys.exit(7)
    if case.get("version") == "remove":
        (root / "claude").unlink()
        print("2.1.296 (Claude Code)", flush=True)
        sys.exit(0)
    print(case.get("version", "2.1.296") + " (Claude Code)", flush=True)
    sys.exit(0)
assert sys.argv[1:] == json.loads((root / "argv.json").read_text()), "argv drift"
assert environment == allowed, "model environment leak"
if "cwd" in case:
    assert pathlib.Path.cwd() == pathlib.Path(case["cwd"]).resolve(), "requested cwd ignored"
if "model" in case:
    assert sys.argv[-2:] == ["--model", case["model"]], "requested model ignored"
assert os.environ["CLAUDE_CODE_OAUTH_TOKEN"] == "fixture-token-not-a-credential", "token seam"
assert "fixture-token-not-a-credential" not in json.dumps(sys.argv), "token in argv"
settings = pathlib.Path(sys.argv[sys.argv.index("--settings") + 1])
mcp = pathlib.Path(sys.argv[sys.argv.index("--mcp-config") + 1])
assert json.loads(settings.read_text()) == {"permissions": {"defaultMode": "default", "allow": [], "deny": []}}
registration = json.loads(mcp.read_text())
assert set(registration["mcpServers"]) == {"flock"}, "missing session flock MCP"
server = registration["mcpServers"]["flock"]
assert server["type"] == "stdio" and server["args"] == ["mcp", "serve", "--structured"]
assert pathlib.Path(server["command"]).is_absolute(), "unpinned MCP executable"
assert set(server["env"]) == {"FLOCK_SOCKET_PATH", "FLOCK_MCP_LEASE_CREDENTIAL"}, "MCP env shape"
assert "FLOCK_MCP_LEASE_CREDENTIAL" not in os.environ, "credential in Claude launch env"
assert len(server["env"]["FLOCK_MCP_LEASE_CREDENTIAL"]) == 44, "lease credential shape"
assert "fixture-token-not-a-credential" not in mcp.read_text(), "token in MCP JSON"
(root / "observed-mcp.json").write_text(json.dumps(registration))
assert stat.S_IMODE(settings.stat().st_mode) == 0o600
assert stat.S_IMODE(mcp.stat().st_mode) == 0o600
assert "--no-session-persistence" not in sys.argv
profile = pathlib.Path(os.environ["CLAUDE_CONFIG_DIR"])
(profile / "sessions").mkdir(mode=0o700, exist_ok=True)
(profile / "transcript.jsonl").write_text("persistent fixture transcript")
(root / "model.pid").write_text(str(os.getpid()))
(root / "validated").write_text("argv/env/settings validated")
catalogue = [json.loads(line) for line in (root / "events.jsonl").read_text().splitlines()]

def find(kind, subtype=None):
    return next(event for event in catalogue if event["type"] == kind and
                (subtype is None or event.get("subtype") == subtype))

def emit(event, newline=True):
    sys.stdout.write(json.dumps(event) + ("\n" if newline else ""))
    sys.stdout.flush()

def messaging_socket():
    sockets = root / "cc-socks"
    sockets.mkdir(mode=0o700)
    listener = socket.socket(socket.AF_UNIX)
    sockpath = sockets / (str(os.getpid()) + ".sock")
    # macOS TMPDIR plus the private fixture root can exceed sockaddr_un.
    # Bind relative to the owned directory; report the same absolute socket
    # path in registry/init so runtime inspection and cleanup remain real.
    previous = os.open(".", os.O_RDONLY)
    try:
        os.chdir(sockets)
        listener.bind(sockpath.name)
    finally:
        os.fchdir(previous)
        os.close(previous)
    sockpath.chmod(0o600)
    registry.write_text(json.dumps({"messagingSocketPath": str(sockpath)}))
    return listener, sockpath

mode = case["mode"]
registry = profile / "sessions" / (str(os.getpid()) + ".json")
if mode.startswith("registry"):
    if mode == "registry-socket":
        listener, sockpath = messaging_socket()
    elif mode == "registry-malformed":
        registry.write_text("{")
    elif mode == "registry-link":
        original = profile / "original-sessions"
        (profile / "sessions").rename(original)
        (profile / "sessions").symlink_to(original, target_is_directory=True)
        registry.write_text("{}")
    else:
        registry.write_text(json.dumps({"messagingSocketPath": None}))
# Readiness is a fixture handshake, not a claim about real harness readiness.
emit(find("system", "hook_started"))
# An owned socket drives each stage; stdin is observed before lifecycle emission.
if mode.startswith("queue"):
    emit(json.loads((root / "restricted-init.json").read_text()))
    with socket.socket(socket.AF_UNIX) as driver:
        driver.connect(str(root / "queue.sock"))
        control = driver.makefile("r")
        current_uuid = None
        count = 0
        barrier = None
        while True:
            ready, _, _ = select.select([sys.stdin, driver], [], [])
            if driver in ready:
                request = control.readline()
                if not request:
                    sys.exit(0)
                request = json.loads(request)
                if request["op"] == "exit":
                    sys.exit(7)
                if request["op"] == "event":
                    emit(request["event"])
                elif request["op"] == "lifecycle":
                    event = dict(find("command_lifecycle"))
                    event["state"] = request["state"]
                    event["command_uuid"] = request.get("uuid", current_uuid)
                    emit(event)
                elif request["op"] == "result":
                    event = dict(find("result", "success"))
                    if request.get("nested"):
                        event["parent_tool_use_id"] = "child-tool"
                    if request.get("unknown"):
                        event["subtype"] = "future_result"
                    emit(event)
                elif request["op"] == "barrier":
                    assert barrier is None, "only one fixture barrier at a time"
                    barrier = request["after_count"]
                elif request["op"] == "loss":
                    sys.stdout.write("x" * (2 * 1024 * 1024 + 1) + "\n")
                    sys.stdout.flush()
                else:
                    raise AssertionError("unexpected driver stage")
            if sys.stdin in ready:
                line = sys.stdin.readline()
                if not line:
                    sys.exit(0)
                message = json.loads(line)
                if message["type"] == "control_response":
                    emit({"type": "fixture_control_input", "input": message})
                    continue
                assert message["type"] == "user", "queue writes only user turns"
                current_uuid = message["uuid"]
                count += 1
                content = message["message"]["content"]
                native = len(content) == 1 and content[0]["text"].startswith("/")
                if mode != "queue-silent":
                    emit({"type": "fixture_input", "input": message,
                          "executed_command": native, "count": count})
            if barrier is not None and count >= barrier:
                emit({"type": "fixture_barrier", "count": count})
                barrier = None
    sys.exit(0)

if mode == "idle":
    sys.exit(0)
if mode == "no-read":
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    parent_gone()
    sys.exit(0)
line = sys.stdin.readline()
if not line:
    sys.exit(0)
assert json.loads(line)["type"] == "user", "private user envelope seam"
(root / "input-received").write_text("input")
# S4-only mode: no stdout evidence after accepting the user envelope.
# Keep this mode separate from queue delivery fixture scenarios.
if mode == "s4-accepted-silent":
    sys.stdin.read()
    sys.exit(0)
emit(find("system", "hook_response"))
init = json.loads((root / "restricted-init.json").read_text())
if mode in ("init-oversize", "init-invalid-utf8"):
    init["padding"] = "x" * (2 * 1024 * 1024) if mode == "init-oversize" else "invalid-byte"
    init["messaging_socket_path"] = "fixture-socket"
    raw = json.dumps(init).encode()
    if mode == "init-invalid-utf8":
        raw = raw.replace(b"invalid-byte", b"\xff")
    sys.stdout.buffer.write(raw + b"\n")
    sys.stdout.buffer.flush()
    sys.stdin.read()
    sys.exit(0)
if mode.startswith("init-socket"):
    if mode.endswith("string"):
        listener, sockpath = messaging_socket()
        init["messaging_socket_path"] = str(sockpath)
    else:
        init["messaging_socket_path"] = 42 if mode.endswith("wrong-type") else None
emit(init)
if mode == "mcp-lease-calls":
    # Execute precisely the registration received via --mcp-config.
    env = dict(os.environ)
    env.update(server["env"])
    responses = []
    with (root / "mcp.stderr").open("wb") as errors:
        tool = subprocess.Popen([server["command"], *server["args"]], env=env,
                                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=errors)
        (root / "tool.pid").write_text(str(tool.pid))
        pending = b""
        try:
            for request in json.loads((root / "mcp-requests.json").read_text()):
                tool.stdin.write((json.dumps(request) + "\n").encode())
                tool.stdin.flush()
                deadline = time.monotonic() + 10
                while b"\n" not in pending:
                    remaining = deadline - time.monotonic()
                    assert remaining > 0 and tool.poll() is None, "MCP producer failed or silent"
                    readable, _, _ = select.select([tool.stdout], [], [], remaining)
                    assert readable, "MCP response ceiling"
                    chunk = os.read(tool.stdout.fileno(), 65536)
                    assert chunk, "MCP producer closed without response"
                    pending += chunk
                    assert len(pending) <= 1048576, "MCP response cap"
                line, pending = pending.split(b"\n", 1)
                response = json.loads(line)
                assert response["id"] == request["id"], "unexpected MCP stage"
                responses.append(response)
            (root / "tool-responses.json").write_text(json.dumps(responses))
        finally:
            tool.stdin.close()
            try:
                assert tool.wait(timeout=10) == 0, "MCP failed"
            except subprocess.TimeoutExpired:
                tool.kill()
                tool.wait(timeout=10)
                raise AssertionError("MCP did not exit on EOF")
            tool.stdout.close()
    emit(find("result", "success"))
    sys.stdin.read()
    sys.exit(0)
if mode in ("state-reducer", "worker-exit", "worker-exit-ok", "worker-stubborn"):
    if mode == "worker-stubborn":
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        descendant = os.fork()
        if descendant == 0:
            os.setsid()
            parent_gone()
            os._exit(0)
        (root / "grandchild.pid").write_text(str(descendant))
    for event in (root / "state-events.jsonl").read_text().splitlines():
        emit(json.loads(event))
    if mode == "worker-stubborn":
        parent_gone()
    if mode.startswith("worker-exit"):
        sys.exit(0 if mode == "worker-exit-ok" else 7)

    sys.stdin.read()
    sys.exit(0)
if mode == "lease-calls":
    tool = os.fork()
    if tool == 0:
        responses = []
        for request in json.loads((root / "requests.json").read_text()):
            with socket.socket(socket.AF_UNIX) as client:
                client.settimeout(10)
                client.connect(str(root / "api.sock"))
                client.sendall((json.dumps(request) + "\n").encode())
                with client.makefile("r") as reader:
                    responses.append(json.loads(reader.readline()))
        (root / "tool-responses.json").write_text(json.dumps(responses))
        os._exit(0)
    (root / "tool.pid").write_text(str(tool))
    _, status = os.waitpid(tool, 0)
    assert os.waitstatus_to_exitcode(status) == 0, "tool process failed"
    emit(find("result", "success"))
    sys.stdin.read()
    sys.exit(0)
if mode == "later-lossy-result":
    emit({"type": "result", "padding": "x" * (2 * 1024 * 1024)})
    assert json.loads(sys.stdin.readline())["type"] == "user"
    emit(find("result", "success"))
    sys.stdin.read()
    sys.exit(0)
if mode in ("later-oversize", "later-invalid-utf8", "later-init-socket"):
    if mode == "later-init-socket":
        emit(dict(init, messaging_socket_path=None))
    elif mode == "later-oversize":
        emit({"type": "assistant", "padding": "x" * (2 * 1024 * 1024)})
    else:
        sys.stdout.buffer.write(b'{"type":"assistant","padding":"\xff"}\n')
        sys.stdout.buffer.flush()
    emit(find("system", "status"))
    sys.stdin.read()
    sys.exit(0)
if mode == "idle-turns":
    emit(find("result", "success"))
    while sys.stdin.readline():
        emit(find("result", "success"))
    sys.exit(0)
if mode == "interrupt-1":
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    sys.stdin.read()
    sys.exit(1)
if mode == "tree":
    events = {signal.SIGTERM, signal.SIGUSR1, signal.SIGUSR2, signal.SIGCHLD}
    signal.pthread_sigmask(signal.SIG_BLOCK, events)
    rd, wr = os.pipe()
    child = os.fork()
    if child == 0:
        os.close(rd)
        os.setsid()
        (root / "grandchild.pid").write_text(str(os.getpid()))
        os.write(wr, b"ready")
        os.close(wr)
        signal.pthread_sigmask(signal.SIG_UNBLOCK, {signal.SIGTERM})
        parent_gone()
        sys.exit(0)
    os.close(wr)
    assert os.read(rd, 5) == b"ready"
    os.close(rd)
    emit(find("system", "status"))
    while True:
        event = signal.sigwait(events)
        if event == signal.SIGUSR2:
            os._exit(7)
        if event == signal.SIGCHLD:
            if os.waitpid(child, os.WNOHANG)[0]:
                sys.exit(0)
if mode in ("missing-result", "exit-143"):
    sys.exit(143 if mode == "exit-143" else 0)
if mode == "flood":
    (root / "flood-started").write_text("flood")
    while True:
        emit(find("system", "status"))
if mode.startswith("init-socket"):
    sys.stdin.read()
    sys.exit(0)
assistant = find("assistant")
assistant["message"]["content"] = [{"type": "text", "text": "x" * 1048576}]
emit(assistant)
emit(find("result", "success"))
sys.stdout.write("final-without-lf")
sys.stdout.flush()
