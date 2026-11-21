#!/usr/bin/env python3
"""Drive a real attach client against isolated fake peers with an 8s fail ceiling."""
import argparse
import json
import os
from pathlib import Path
import pty
import select
import socket
import struct
import subprocess
import tempfile
import termios
import time


MESSAGE = "server did not answer attach within 5s (server busy?)"


def varint(value):
    if value < 251:
        return bytes([value])
    return b"\xfb" + struct.pack("<H", value)


def welcome(version, error):
    text = error.encode()
    payload = b"\0" + varint(version) + b"\0\1" + varint(len(text)) + text
    return struct.pack("<I", len(payload)) + payload


def read_exact(peer, count, deadline):
    data = bytearray()
    while len(data) < count:
        remaining = deadline - time.monotonic()
        if remaining <= 0 or not select.select([peer], [], [], remaining)[0]:
            raise AssertionError("fake peer did not receive complete Hello before ceiling")
        part = peer.recv(count - len(data))
        if not part:
            raise AssertionError("client closed before complete Hello")
        data.extend(part)
    return bytes(data)


def observe(binary, mode, machine_output=False):
    with tempfile.TemporaryDirectory(prefix="aw-", dir="/tmp") as directory:
        root = Path(directory)
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(str(root / "client.sock"))
        listener.listen(4)
        listener.setblocking(False)
        master, slave = pty.openpty()
        termios.tcsetwinsize(slave, (40, 120))
        original = termios.tcgetattr(slave)
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("FLOCK_", "LD_", "DYLD_"))}
        env.update(HOME=directory, XDG_CONFIG_HOME=str(root / "c"),
                   XDG_RUNTIME_DIR=str(root / "r"), XDG_DATA_HOME=str(root / "d"),
                   XDG_STATE_HOME=str(root / "s"), XDG_CACHE_HOME=str(root / "cache"),
                   FLOCK_CLIENT_SOCKET_PATH=str(root / "client.sock"),
                   SHELL="/bin/sh", TERM="xterm-256color")
        for app in ("flock", "flock-dev"):
            config = root / "c" / app
            config.mkdir(parents=True)
            (config / "config.toml").write_text("onboarding = false\n")
        args = [str(binary), "client"] + (["--json"] if machine_output else [])
        child = subprocess.Popen(args, env=env, cwd=root, stdin=slave, stdout=slave, stderr=slave)
        output = bytearray()
        peers = []
        attempts = 0
        started = time.monotonic()
        deadline = started + 8
        hello_at = None
        next_byte = None
        trickle = bytearray()
        try:
            while child.poll() is None:
                now = time.monotonic()
                assert now < deadline, f"{mode}: live client exceeded 8s ceiling"
                if next_byte is not None and now >= next_byte:
                    if trickle:
                        peers[0].sendall(trickle[:1])
                        del trickle[:1]
                    next_byte += 1.5
                ready, _, _ = select.select([master, listener, *peers], [], [],
                                             min(0.05, deadline - now))
                for source in ready:
                    if source == master:
                        output.extend(os.read(master, 65536))
                    elif source is listener:
                        peer, _ = listener.accept()
                        peers.append(peer)
                        attempts += 1
                        size = struct.unpack("<I", read_exact(peer, 4, deadline))[0]
                        assert size < 1024 * 1024, "unbounded Hello"
                        hello = read_exact(peer, size, deadline)
                        assert hello and hello[0] == 0, "expected Hello"
                        if hello_at is None:
                            hello_at = time.monotonic()
                            # Typed after Hello: it must never be sent before Welcome or replayed.
                            os.write(master, b"do-not-replay\n")
                        if mode == "eof":
                            peer.shutdown(socket.SHUT_WR)
                        elif mode == "malformed":
                            peer.sendall(struct.pack("<I", 1) + b"\xff")
                        elif mode == "version":
                            peer.sendall(welcome(65535, "incompatible protocol"))
                        elif mode == "handoff":
                            peer.sendall(welcome(34, "live update in progress; reconnect after handoff completes")
                                         if attempts == 1 else welcome(65535, "incompatible protocol"))
                        elif mode in ("prefix", "payload"):
                            if mode == "payload":
                                peer.sendall(struct.pack("<I", 100))
                                trickle = bytearray(b"\0" * 10)
                            else:
                                trickle = bytearray(struct.pack("<I", 100) + b"\0" * 10)
                            next_byte = time.monotonic()
                        elif mode != "silent":
                            raise AssertionError(f"unknown mode {mode}")
                    else:
                        data = source.recv(65536)
                        assert not data, f"{mode}: terminal input was replayed after Hello"
                        peers.remove(source)
                        source.close()
            # The slave is deliberately retained to inspect termios after client exit.
            while select.select([master], [], [], 0)[0]:
                output.extend(os.read(master, 65536))
            assert hello_at is not None, "client never sent Hello"
            elapsed = time.monotonic() - hello_at
            assert termios.tcgetattr(slave) == original, "PTY state was not restored"
            text = output.decode(errors="replace")
            if mode in ("silent", "prefix", "payload"):
                assert child.returncode == 124, (mode, child.returncode, text)
                assert MESSAGE in text, (mode, text)
                assert 4.9 <= elapsed < 6.5, (mode, elapsed)
                assert attempts == 1, "timeout must not redial"
                if machine_output:
                    document = json.JSONDecoder().raw_decode(text[text.index('{'):])[0]
                    error = document["error"]
                    assert error["code"] == "attach_timeout", document
                    assert error["exit"] == 124 and error["retry"] == "yes", document
                    assert error["data"] == {"phase": "welcome", "timeout_ms": 5000}, document
            else:
                assert child.returncode == 1 and MESSAGE not in text, (mode, text)
                assert ("incompatible protocol" if mode in ("version", "handoff") else "protocol error") in text, text
                assert attempts == (2 if mode == "handoff" else 1), (mode, attempts)
            return {"mode": mode, "json": machine_output, "exit": child.returncode,
                    "after_hello_seconds": round(elapsed, 3), "attempts": attempts,
                    "restored_pty": True, "replayed_input_bytes": 0}
        finally:
            if child.poll() is None:
                child.kill()
            child.wait(timeout=2)
            for peer in peers:
                peer.close()
            listener.close()
            os.close(master)
            os.close(slave)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin", type=Path, required=True)
    parser.add_argument("--case", choices=("silent", "prefix", "payload", "eof", "malformed", "version", "handoff"))
    args = parser.parse_args()
    binary = args.bin.resolve(strict=True)
    cases = [args.case] if args.case else ["silent", "prefix", "payload", "eof", "malformed", "version", "handoff"]
    for mode in cases:
        print(json.dumps(observe(binary, mode)), flush=True)
    if not args.case:
        print(json.dumps(observe(binary, "silent", machine_output=True)), flush=True)


if __name__ == "__main__":
    main()
