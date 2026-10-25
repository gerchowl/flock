#!/usr/bin/env python3
"""#370: real isolated TUI/API refusal, with an owned repo and sandbox HOME.

Run with --flk /absolute/path/to/flk. No inherited FLOCK variables, live
profiles or server connections. All producers and waits have finite ceilings.
"""
import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import socket
import struct
import subprocess
import tempfile
import termios
import time


def run(binary):
    with tempfile.TemporaryDirectory(prefix="wr", dir="/tmp") as temporary:
        root = Path(temporary).resolve()
        repo = root / "repo"
        repo.mkdir()
        config = root / "config.toml"
        config.write_text('onboarding = false\n[keys]\nkill_worktree = ["f8"]\n')
        env = {"PATH": os.environ["PATH"], "HOME": str(root), "XDG_CONFIG_HOME": str(root / "x"),
               "XDG_STATE_HOME": str(root / "s"), "TMPDIR": str(root), "SHELL": "/bin/sh",
               "TERM": "xterm-256color", "LANG": "C", "LC_ALL": "C",
               "FLOCK_CONFIG_PATH": str(config), "FLOCK_SOCKET_PATH": str(root / "a"),
               "FLOCK_CLIENT_SOCKET_PATH": str(root / "c")}
        def git(*args):
            subprocess.run(["git", "-c", "commit.gpgsign=false", "-c", "user.name=Fixture",
                "-c", "user.email=fixture@example.invalid", "-C", str(repo), *args],
                env=env, check=True, capture_output=True, timeout=10)
        git("init", "--quiet")
        (repo / "tracked").write_text("owned fixture")
        git("add", "tracked")
        git("commit", "--quiet", "-m", "fixture")
        def api(method, params):
            with socket.socket(socket.AF_UNIX) as connection:
                connection.settimeout(10)
                connection.connect(str(root / "a"))
                connection.sendall((json.dumps({"id":"probe", "method":method, "params":params}) + "\n").encode())
                with connection.makefile("rb") as stream:
                    return json.loads(stream.readline())
        server = subprocess.Popen([binary, "server"], cwd=repo, env=env,
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        client = None
        master = None
        try:
            output = b""
            deadline = time.monotonic() + 15
            while b"client socket:" not in output:
                remaining = deadline - time.monotonic()
                assert server.poll() is None and remaining > 0, output.decode(errors="replace")
                assert select.select([server.stdout], [], [], remaining)[0], "server readiness missing"
                block = os.read(server.stdout.fileno(), 65536)
                assert block, output.decode(errors="replace")
                output += block
            parent = api("workspace.create", {"cwd":str(repo), "label":"probe-root"})
            assert "result" in parent, parent
            created = api("worktree.create", {"workspace_id":parent["result"]["workspace"]["workspace_id"],
                "path":str(root / "checkout"), "branch":"worktree/probe", "label":"unreachable-probe", "focus":True})
            assert "result" in created, created
            checkout = root / "checkout"
            (checkout / ".git").unlink()
            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 42, 120, 0, 0))
            client = subprocess.Popen([binary], cwd=repo, env=env, stdin=slave, stdout=slave,
                                      stderr=slave, start_new_session=True)
            os.close(slave)
            screen = b""
            def expect(*markers):
                nonlocal screen
                deadline = time.monotonic() + 15
                while True:
                    # Cursor positioning separates screen fragments rather than words.
                    clean = re.sub(rb"\x1b\][\s\S]*?(?:\x07|\x1b\\)", b"", screen)
                    clean = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b" ", clean)
                    text = " ".join(clean.decode(errors="replace").split())
                    compact = "".join(text.split())
                    if all("".join(marker.split()) in compact for marker in markers):
                        return text
                    remaining = deadline - time.monotonic()
                    assert client.poll() is None and server.poll() is None and remaining > 0, (markers, text[-6000:])
                    assert select.select([master], [], [], remaining)[0], ("screen evidence missing", markers, text[-6000:])
                    block = os.read(master, 65536)
                    assert block, "client closed before screen evidence"
                    screen += block
                    if b"\x1b[6n" in block:
                        os.write(master, b"\x1b[1;1R")
                    if b"\x1b]11;?" in block:
                        os.write(master, b"\x1b]11;rgb:0000/0000/0000\x07")
            expect("probe-root")
            focused = api("workspace.focus", {"workspace_id":created["result"]["workspace"]["workspace_id"]})
            assert "result" in focused, focused
            os.write(master, b"\x1b[19~")
            expect("checkout is gone")
            os.write(master, b"\r")
            expect("Remount and retry", "repo-wide", "temporarily absent checkouts.")
            refused = api("worktree.remove", {"workspace_id":created["result"]["workspace"]["workspace_id"], "force":True})
            assert refused["error"]["code"] == "worktree_checkout_unreachable", refused
            assert refused["error"]["data"]["path"] == str(checkout)
            refused_cli = subprocess.run([binary, "--json", "worktree", "kill", "--path", str(checkout),
                "--force", "--keep-branch", "--keep-procs"], cwd=repo, env=env,
                capture_output=True, text=True, timeout=15)
            assert refused_cli.returncode == 10, (refused_cli.returncode, refused_cli.stdout, refused_cli.stderr)
            assert json.loads(refused_cli.stdout)["error"]["code"] == "worktree_checkout_unreachable"
            assert checkout.is_dir() and (checkout / "tracked").read_text() == "owned fixture"
            return {"status":"passed", "real_tui":True, "api_refusal":True, "recovery_visible":True, "cli_exit":10}
        finally:
            if client is not None:
                if client.poll() is None:
                    client.terminate()
                try:
                    client.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    client.kill()
                    client.wait(timeout=5)
            if master is not None:
                os.close(master)
            if server.poll() is None:
                try:
                    subprocess.run([binary, "server", "stop"], cwd=repo, env=env, capture_output=True, timeout=15)
                finally:
                    try:
                        server.wait(timeout=15)
                    except subprocess.TimeoutExpired:
                        server.kill()
                        server.wait(timeout=5)
            server.stdout.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--flk", required=True)
    args = parser.parse_args()
    print(json.dumps(run(str(Path(args.flk).resolve()))))
