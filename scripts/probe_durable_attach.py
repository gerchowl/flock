#!/usr/bin/env python3
"""Measure a real isolated server's attach while event-log sync stalls 8s.

Run in nix develop: python3 scripts/probe_durable_attach.py --bin /path/to/flk
Requires an unsigned dynamic debug build on macOS, cc, and Python 3.
--assert-responsive is the future regression gate (fails on the baseline).
Only the owned server tree receives the interposer. No inherited FLOCK_* survives.
"""

import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import signal
import socket
import subprocess
import tempfile
import time


def wait_for(predicate, server, description, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if server.poll() is not None:
            raise RuntimeError(f"server exited waiting for {description}")
        if predicate():
            return
        time.sleep(0.01)
    raise TimeoutError(f"silent producer: {description}")


def request(path, method, params):
    peer = socket.socket(socket.AF_UNIX)
    peer.settimeout(20)
    peer.connect(str(path))
    peer.sendall((json.dumps({"id": "probe", "method": method, "params": params}) + "\n").encode())
    return peer


def attach(binary, env):
    # The client times out before terminal setup, so no terminal is needed for
    # the stalled baseline. A successful handshake proceeds to terminal setup.
    import pty
    import termios
    master, slave = pty.openpty()
    termios.tcsetwinsize(slave, (40, 120))
    started = time.monotonic()
    child = subprocess.Popen([str(binary)], env=env, stdin=slave, stdout=slave,
                             stderr=slave, start_new_session=True)
    os.close(slave)
    output = bytearray()
    import select
    deadline = started + 7
    try:
        while time.monotonic() < deadline:
            if select.select([master], [], [], 0.05)[0]:
                try:
                    data = os.read(master, 65536)
                except OSError:
                    break
                if not data:
                    break
                output.extend(data)
                # Alternate-screen entry is after the Welcome handshake.
                if b"\x1b[?1049h" in output:
                    return {"seconds": round(time.monotonic() - started, 3), "attached": True}
            if child.poll() is not None:
                break
        text = output.decode(errors="replace")
        return {"seconds": round(time.monotonic() - started, 3), "attached": False,
                "timeout_error": "os error" in text or "did not answer attach" in text,
                "exit": child.poll(), "diagnostic": text[-1500:]}
    finally:
        if child.poll() is None:
            os.killpg(child.pid, signal.SIGKILL)
        child.wait(timeout=5)
        os.close(master)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin", type=Path, required=True)
    parser.add_argument("--assert-responsive", action="store_true")
    args = parser.parse_args()
    binary = args.bin.resolve(strict=True)
    if os.getloadavg()[0] >= 10:
        raise RuntimeError("load must be below 10")
    if shutil.disk_usage("/tmp").free < 15 * 1024**3:
        raise RuntimeError("BLOCKED: disk")
    with tempfile.TemporaryDirectory(prefix="da-", dir="/tmp") as directory:
        root = Path(directory)
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("FLOCK_", "PROBE_SYNC_", "DYLD_", "LD_"))}
        env.update(HOME=str(root), XDG_CONFIG_HOME=str(root / "c"),
                   XDG_DATA_HOME=str(root / "d"), XDG_STATE_HOME=str(root / "s"),
                   XDG_CACHE_HOME=str(root / "cache"), XDG_RUNTIME_DIR=str(root / "r"),
                   SHELL="/bin/sh", TERM="xterm-256color", FLOCK_HOST_NAME="probe.example")
        for name in ("c", "d", "s", "cache", "r"):
            (root / name).mkdir(mode=0o700)
        for app in ("flock", "flock-dev"):
            config = root / "c" / app
            config.mkdir()
            (config / "config.toml").write_text("onboarding = false\n")
        library = root / "slow-sync.so"
        source = Path(__file__).with_name("slow_fsync.c")
        flags = ["-dynamiclib"] if platform.system() == "Darwin" else ["-shared", "-fPIC"]
        libraries = [] if platform.system() == "Darwin" else ["-ldl"]
        subprocess.run(["cc", *flags, str(source), *libraries, "-o", str(library)],
                       check=True, timeout=60)
        server_env = dict(env)
        server_env["DYLD_INSERT_LIBRARIES" if platform.system() == "Darwin" else "LD_PRELOAD"] = str(library)
        arm, entered = root / "arm", root / "entered"
        # data_dir() uses the config app directory. Both namespaces are private.
        server_env.update(PROBE_SYNC_TARGET=str(root / "c" / "flock-dev" / "event-log.jsonl"),
                          PROBE_SYNC_ARM=str(arm), PROBE_SYNC_ENTERED=str(entered))
        server = None
        trigger = None
        try:
            with (root / "server.log").open("wb") as log:
                server = subprocess.Popen([str(binary), "server"], env=server_env,
                                          stdout=log, stderr=log, start_new_session=True, cwd=root)
                wait_for(lambda: list(root.rglob("flock.sock")), server, "API socket")
                api = next(root.rglob("flock.sock"))
                # Wait for the real API to answer, not merely a socket inode.
                with request(api, "ping", {}) as peer:
                    reply = peer.recv(65536)
                    if not reply or b'"error"' in reply:
                        raise RuntimeError("API readiness refused")
                logs = list(root.rglob("event-log.jsonl"))
                if len(logs) != 1 or str(logs[0]) != server_env["PROBE_SYNC_TARGET"]:
                    raise RuntimeError("expected a debug event log in the private config directory")
                baseline = attach(binary, env)
                if not baseline["attached"]:
                    raise RuntimeError(f"control attach failed: {baseline}")
                arm.touch()
                trigger = request(api, "workspace.create", {"cwd": str(root), "focus": False})
                started = time.monotonic()
                wait_for(lambda: entered.exists() and entered.read_text() in ("main", "worker"),
                         server, "interposed event-log fsync", timeout=15)
                stalled = attach(binary, env)
                # Verify the trigger eventually finishes, even after a timeout.
                response = trigger.makefile("rb").readline(1024 * 1024)
                if not response or "error" in json.loads(response):
                    raise RuntimeError("trigger closed without a response")
                result = {"platform": platform.system(), "injected_sync_seconds": 8,
                          "sync_thread": entered.read_text(),
                          "trigger_seconds": round(time.monotonic() - started, 3),
                          "control": baseline, "stalled": stalled}
                print(json.dumps(result, sort_keys=True), flush=True)
                if args.assert_responsive and (not stalled["attached"] or stalled["seconds"] >= 2):
                    raise AssertionError("attach must complete within 2s during stalled fsync")
        finally:
            if trigger is not None:
                trigger.close()
            if server is not None:
                if server.poll() is None:
                    os.killpg(server.pid, signal.SIGTERM)
                    try:
                        server.wait(timeout=15)
                    except subprocess.TimeoutExpired:
                        os.killpg(server.pid, signal.SIGKILL)
                server.wait(timeout=5)


if __name__ == "__main__":
    def interrupted(signum, frame):
        raise KeyboardInterrupt(f"probe interrupted by signal {signum}")

    signal.signal(signal.SIGTERM, interrupted)
    main()
