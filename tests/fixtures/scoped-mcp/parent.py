"""Owned launch parent for #1734: all ordering uses pipes and waitpid."""
import json
import os
import signal
import subprocess
import socket
import threading
import sys

with open(sys.argv[2], encoding="utf-8") as source:
    registration = json.load(source)["mcpServers"]["flock"]
assert registration["command"] == sys.argv[1]
assert registration["args"] == ["mcp", "serve", "--structured"]
output = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
output.connect(sys.argv[3])

def stage(value):
    os.write(sys.stdout.fileno(), (json.dumps(value) + "\n").encode())

child = subprocess.Popen(
    [sys.argv[1], "--exact", "app::api::scoped_mcp_tests::mcp_fixture_child", "--quiet", "--nocapture"],
    stdin=subprocess.PIPE,
    stdout=output,
    env={**os.environ, **registration["env"], "SCOPED_MCP_FIXTURE_CHILD": "1"},
)
stage({"stage": "CHILD", "pid": child.pid})

def watch_child():
    status = child.wait()
    stage({"stage": "CHILD_EXITED", "status": status})

threading.Thread(target=watch_child, daemon=True).start()
for line in sys.stdin:
    command = json.loads(line)
    if command["stage"] == "BUFFER":
        os.kill(child.pid, signal.SIGSTOP)
        stopped, status = os.waitpid(child.pid, os.WUNTRACED)
        assert stopped == child.pid and os.WIFSTOPPED(status)
        assert os.WSTOPSIG(status) == signal.SIGSTOP
    child.stdin.write((json.dumps(command["request"]) + "\n").encode())
    child.stdin.flush()
    if command["stage"] == "BUFFER":
        stage({"stage": "QUEUED"})
raise RuntimeError("controller must kill and reap this owned parent")
