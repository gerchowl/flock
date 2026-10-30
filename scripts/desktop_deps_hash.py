#!/usr/bin/env python3
"""Refresh the desktop CLI dependency pin; restore it on any failed fetch."""

import hashlib
import json
from pathlib import Path
import re
import signal
import subprocess


def main():
    pin = Path("nix/desktop-deps.json")
    original = pin.read_bytes()
    complete = False

    def restore():
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        pin.write_bytes(original)

    def interrupted(signum, _frame):
        if not complete:
            restore()
        raise SystemExit(128 + signum)

    signal.signal(signal.SIGINT, interrupted)
    signal.signal(signal.SIGTERM, interrupted)

    def write(value):
        pin.write_text(json.dumps(value, indent=2) + "\n")

    def build():
        return subprocess.run(
            ["nix", "build", ".#flock-desktop.deps", "--no-link",
             "--cores", "2", "--max-jobs", "1"],
            text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        )

    try:
        value = {
            "lock": hashlib.sha256(Path("desktop/bun.lock").read_bytes()).hexdigest(),
            "manifest": hashlib.sha256(Path("desktop/package.json").read_bytes()).hexdigest(),
            "hash": "",
        }
        write(value)
        value["bun"] = subprocess.check_output(
            ["nix", "eval", "--raw", ".#flock-desktop.deps.bunVersion"], text=True
        ).strip()
        fetched = build()
        print(fetched.stdout, end="")
        hashes = re.findall(r"^\s*got:\s+(sha256-\S+)", fetched.stdout, re.MULTILINE)
        if fetched.returncode == 0 or not hashes:
            raise SystemExit("desktop-deps-hash: expected a fixed-output hash mismatch; pin restored")
        value["hash"] = hashes[-1]
        write(value)
        verified = build()
        print(verified.stdout, end="")
        if verified.returncode:
            raise SystemExit(verified.returncode)
        complete = True
        print(f"{pin}: {value['hash']}")
    finally:
        if not complete:
            restore()


if __name__ == "__main__":
    main()
