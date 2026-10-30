#!/usr/bin/env python3
"""S10f: public TUI composer and shell CLI against an owned fake Claude.

Every wait is on a socket, pipe or terminal redraw, with a finite fail ceiling.
No inherited FLOCK variables, real credentials, or live profile paths are used.
"""
import argparse
import codecs
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import shlex
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time


class Screen:
    """Track the cursor-addressed cells emitted by ratatui, including split UTF-8."""
    def __init__(self):
        self.cells = [[" " for _ in range(120)] for _ in range(50)]
        self.row = self.col = 0
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def feed(self, data):
        self.pending += self.decoder.decode(data)
        while self.pending:
            if self.pending.startswith("\x1b"):
                pattern = (r"\x1b\[[0-?]*[ -/]*[@-~]" if self.pending.startswith("\x1b[")
                           else r"\x1b\][\s\S]*?(?:\x07|\x1b\\)" if self.pending.startswith("\x1b]")
                           else r"\x1b.")
                match = re.match(pattern, self.pending)
                if match is None:
                    return
                code = match[0]
                if code.startswith("\x1b[") and code[-1] in "Hf":
                    args = code[2:-1].split(";")
                    self.row = int(args[0] or 1) - 1
                    self.col = int(args[1] or 1) - 1 if len(args) > 1 else 0
                elif code == "\x1b[2J":
                    self.cells = [[" " for _ in range(120)] for _ in range(50)]
                self.pending = self.pending[len(code):]
                continue
            c, self.pending = self.pending[0], self.pending[1:]
            if c == "\r":
                self.col = 0
            elif c == "\n":
                self.row += 1
            elif c >= " ":
                if 0 <= self.row < 50 and 0 <= self.col < 120:
                    self.cells[self.row][self.col] = c
                self.col += 1

    def text(self):
        return "\n".join("".join(row) for row in self.cells)

    def location(self, label):
        for y, row in enumerate(self.cells):
            x = "".join(row).find(label)
            if x >= 0:
                return x + 1, y + 1
        raise AssertionError(("button not painted", label, self.text()))


def run(binary, permission=False):
    repo = Path(__file__).resolve().parents[2]
    with tempfile.TemporaryDirectory(prefix="sf", dir="/tmp") as temporary:
        root = Path(temporary).resolve()
        (root / "bin").mkdir()
        (root / "project").mkdir()
        for name in ("ready", "shell-ready"):
            os.mkfifo(root / name, 0o600)
        ready = os.open(root / "ready", os.O_RDWR | os.O_NONBLOCK)
        shell_ready = os.open(root / "shell-ready", os.O_RDWR | os.O_NONBLOCK)
        fake = root / "bin/claude"
        fake.write_text(f"#!{sys.executable}\n" + (repo / "tests/fixtures/structured-public/claude.py").read_text())
        fake.chmod(0o700)
        (root / "init.json").write_bytes((repo / "tests/fixtures/structured-runtime/restricted-init.json").read_bytes())
        token = root / "token"
        token.write_text("fixture-token-not-a-credential")
        token.chmod(0o600)
        config = root / "config.toml"
        config.write_text(f'''onboarding = false
[keys]
detach = ['f8']
[terminal]
default_shell = '/bin/sh'
shell_mode = 'non_login'
[structured]
run_as = 'operator'
profile = 'acceptance'
token_file = {json.dumps(str(token))}
[security]
agent_account = 'none'
[fleet]
agent_spawn_enabled = true
''')
        env = {"PATH": str(root / "bin") + os.pathsep + os.environ["PATH"],
               "HOME": str(root), "XDG_CONFIG_HOME": str(root / "x"),
               "XDG_STATE_HOME": str(root / "s"), "TMPDIR": str(root),
               "FLOCK_CONFIG_PATH": str(config), "FLOCK_SOCKET_PATH": str(root / "a"),
               "FLOCK_CLIENT_SOCKET_PATH": str(root / "c"), "SHELL": "/bin/sh",
               "USER": "fixture-user", "LOGNAME": "fixture-user", "TERM": "xterm-256color",
               "LANG": "C", "LC_ALL": "C"}
        def cli(*args):
            answer = subprocess.run([binary, "--json", *args], env=env, cwd=root / "project",
                                    capture_output=True, text=True, timeout=15)
            assert answer.returncode == 0, (args, answer.returncode, answer.stdout, answer.stderr)
            return json.loads(answer.stdout) if answer.stdout.strip() else None
        def api(method, params):
            with socket.socket(socket.AF_UNIX) as connection:
                connection.settimeout(15)
                connection.connect(env["FLOCK_SOCKET_PATH"])
                connection.sendall((json.dumps({"id":"acceptance", "method":method,"params":params}) + "\n").encode())
                with connection.makefile("rb") as stream:
                    answer = json.loads(stream.readline())
                    assert "result" in answer, answer
                    return answer["result"]
        server = subprocess.Popen([binary, "server"], env=env, cwd=root / "project",
                                  stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        client = control = channel = master = None
        try:
            output = b""
            deadline = time.monotonic() + 15
            while b"client socket:" not in output:
                left = deadline - time.monotonic()
                assert server.poll() is None and left > 0 and select.select([server.stdout], [], [], left)[0], output
                block = os.read(server.stdout.fileno(), 65536)
                assert block, output
                output += block
            shell = api("workspace.create", {"cwd":str(root / "project"), "label":"acceptance-shell"})["root_pane"]["pane_id"]
            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 120, 0, 0))
            client = subprocess.Popen([binary], env=env, cwd=root / "project", stdin=slave,
                                      stdout=slave, stderr=slave, start_new_session=True)
            os.close(slave)
            screen = Screen()
            def expect(*markers):
                deadline = time.monotonic() + 15
                while not all(marker in screen.text() for marker in markers):
                    left = deadline - time.monotonic()
                    assert client.poll() is None and server.poll() is None and left > 0, (markers, screen.text())
                    assert select.select([master], [], [], left)[0], ("TUI evidence missing", markers, screen.text())
                    block = os.read(master, 65536)
                    assert block, "TUI closed before evidence"
                    screen.feed(block)
                    if b"\x1b[6n" in block:
                        os.write(master, b"\x1b[1;1R")
                    if b"\x1b]11;?" in block:
                        os.write(master, b"\x1b]11;rgb:0000/0000/0000\x07")
            expect("acceptance-shell")
            created = cli("agent", "start", "acceptance-native", "--runtime", "structured",
                          "--model", "s10f-mvp", "--cwd", str(root / "project"), "--", "claude")["result"]["agent"]
            assert select.select([ready], [], [], 15)[0], "fake initialization missing"
            assert os.read(ready, 1) == b"R"
            control = socket.socket(socket.AF_UNIX)
            control.settimeout(15)
            control.connect(str(root / "control"))
            channel = control.makefile("rwb", buffering=0)
            metadata = json.loads(channel.readline())
            assert metadata["stdin_tty"] is False and metadata["stdout_tty"] is False, metadata
            assert Path(metadata["temporary"]).is_relative_to(root), metadata
            def command(value):
                control.sendall((json.dumps(value) + "\n").encode())
                return json.loads(channel.readline())
            focused = cli("agent", "focus", created["agent_id"])["result"]["agent"]
            assert focused["view"]["pane_id"], focused
            expect("native-transcript", "Tool: Bash (fixture-tool)", "Tool result: Bash (fixture-tool)",
                   "tool-card-result", "Status:", "Send at idle")
            command("begin")
            expect("streaming-marker", "turn: working")
            # Keys travel through the attached TUI, never pane.send-text or a direct admission API.
            os.write(master, b"composer-message\r")
            expect("Queue: 1 queued for idle")
            inputs = command("inspect")
            assert inputs == [], ("busy turn consumed the queued composer", inputs)
            command("finish")
            expect("composer-answer", "turn: idle")
            inputs = command("inspect")
            assert len(inputs) == 1 and inputs[0]["type"] == "user", inputs
            assert "composer-message" in json.dumps(inputs[0]), inputs
            command("begin")
            expect("turn: working")
            invocation = shlex.join([binary, "--json", "agent", "send", "acceptance-native", "shell-message"])
            script = f"{invocation} > {shlex.quote(str(root / 'shell-response'))}; printf R > {shlex.quote(str(root / 'shell-ready'))}"
            cli("pane", "send-text", "--raw", shell, script)
            cli("pane", "send-keys", shell, "Enter")
            assert select.select([shell_ready], [], [], 15)[0], "shell CLI never acknowledged"
            assert os.read(shell_ready, 1) == b"R"
            response = json.loads((root / "shell-response").read_text())
            assert "result" in response, response
            expect("Queue: 1 queued for idle")
            assert len(command("inspect")) == 1, "busy turn consumed the shell send"
            command("finish")
            expect("shell-answer", "turn: idle")
            inputs = command("inspect")
            assert len(inputs) == 2 and inputs[1]["type"] == "user", inputs
            assert "shell-message" in json.dumps(inputs[1]) and "operator via pane" in json.dumps(inputs[1]), inputs
            if permission:
                # Enable with S10e: each painted decision must reach model stdin.
                for behavior, label in [("deny", "[Deny]"), ("allow", "[Allow once]")]:
                    command("permission-" + behavior)
                    expect(label)
                    x, y = screen.location(label)
                    os.write(master, f"\x1b[<0;{x};{y}M\x1b[<0;{x};{y}m".encode())
                    expect("permission-" + behavior + "-answer")
                    response = command("inspect")[-1]
                    assert response["type"] == "control_response", response
                    assert response["response"]["response"]["behavior"] == behavior, response
            cli("agent", "stop", created["agent_id"])
            after = 0
            deadline = time.monotonic() + 15
            while cli("agent", "status", created["agent_id"])["result"]["agent"]["lifecycle"] != "offline":
                assert time.monotonic() < deadline and server.poll() is None, "stop never reached offline"
                after = api("events.read", {"after":after, "wait_ms":1000})["next_after"]
            for pid in (metadata["pid"], metadata["mcp_pid"]):
                try:
                    os.kill(pid, 0)
                except OSError as error:
                    assert error.errno == errno.ESRCH, error
                else:
                    raise AssertionError(("owned descendant survived stop", pid))
            stopped = json.loads((root / "stopped.json").read_text())
            assert stopped["model_exit"] == 0, stopped
            # The server owns the whole model group; its MCP may receive the same TERM.
            import signal
            assert stopped["mcp_exit"] in (0, -signal.SIGTERM), stopped
            assert not Path(metadata["temporary"]).exists(), "temporary launch files survived stop"
            assert Path(metadata["profile"]).exists(), "persistent resume profile must be retained"
            # Detach through the real UI and drain its terminal until EOF so a
            # final repaint cannot block the client's orderly exit on a full PTY.
            os.write(master, b"\x1b[19~")
            deadline = time.monotonic() + 15
            while True:
                left = deadline - time.monotonic()
                assert left > 0 and select.select([master], [], [], left)[0], "TUI detach did not finish"
                try:
                    block = os.read(master, 65536)
                except OSError as error:
                    assert error.errno == errno.EIO, error
                    break
                if not block:
                    break
            assert client.wait(timeout=15) == 0
            cli("server", "stop")
            assert server.wait(timeout=15) == 0
            return {"status":"passed", "real_tui":True, "composer":True, "shell_cli":True,
                    "pipes_not_pty":True, "descendants_gone":True, "temporary_removed":True}
        finally:
            if channel is not None:
                channel.close()
            if control is not None:
                control.close()
            for child in (client, server):
                if child is not None:
                    if child.poll() is None:
                        child.terminate()
                    try:
                        child.wait(timeout=15)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait(timeout=5)
            if master is not None:
                os.close(master)
            server.stdout.close()
            os.close(ready)
            os.close(shell_ready)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--flk", required=True)
    parser.add_argument("--permission", action="store_true")
    args = parser.parse_args()
    print(json.dumps(run(str(Path(args.flk).resolve()), args.permission)))
