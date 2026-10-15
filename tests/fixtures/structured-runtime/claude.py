"""Owned fake Claude. Shapes reconstructed from the committed S1 catalogue."""
import json
import os
import pathlib
import signal
import select
import socket
import stat
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
assert os.environ["CLAUDE_CODE_OAUTH_TOKEN"] == "fixture-token-not-a-credential", "token seam"
assert "fixture-token-not-a-credential" not in json.dumps(sys.argv), "token in argv"
settings = pathlib.Path(sys.argv[sys.argv.index("--settings") + 1])
mcp = pathlib.Path(sys.argv[sys.argv.index("--mcp-config") + 1])
assert json.loads(settings.read_text()) == {"permissions": {"defaultMode": "default", "allow": [], "deny": []}}
assert json.loads(mcp.read_text()) == {"mcpServers": {}}
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

mode = case["mode"]
registry = profile / "sessions" / (str(os.getpid()) + ".json")
if mode.startswith("registry"):
    if mode == "registry-socket":
        sockets = root / "cc-socks"
        sockets.mkdir(mode=0o700)
        listener = socket.socket(socket.AF_UNIX)
        sockpath = sockets / (str(os.getpid()) + ".sock")
        listener.bind(str(sockpath))
        sockpath.chmod(0o600)
        registry.write_text(json.dumps({"messagingSocketPath": str(sockpath)}))
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
        sockets = root / "cc-socks"
        sockets.mkdir(mode=0o700)
        listener = socket.socket(socket.AF_UNIX)
        sockpath = sockets / (str(os.getpid()) + ".sock")
        listener.bind(str(sockpath))
        sockpath.chmod(0o600)
        registry.write_text(json.dumps({"messagingSocketPath": str(sockpath)}))
        init["messaging_socket_path"] = str(sockpath)
    else:
        init["messaging_socket_path"] = 42 if mode.endswith("wrong-type") else None
emit(init)
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
