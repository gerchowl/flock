#!/usr/bin/env python3
"""Read the shared CI/devShell nextest pin; reject invalid or too-old versions."""

from pathlib import Path
import re
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def version_tuple(text: str) -> tuple[int, int, int]:
    """Parse a canonical release version with at most one trailing newline."""
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\n?", text):
        raise ValueError(f"invalid nextest release version: {text!r}")
    return tuple(int(part) for part in text.removesuffix("\n").split("."))


def read_version(root: Path = ROOT) -> str:
    """Return the shared pin, failing if it is malformed or below the repo minimum."""
    text = (root / "nix/nextest-version").read_bytes().decode("utf-8")
    minimum = tomllib.loads((root / ".config/nextest.toml").read_text())["nextest-version"]
    if version_tuple(text) < version_tuple(minimum):
        raise ValueError(f"nextest pin {text.rstrip()} is below required {minimum}")
    return text.removesuffix("\n")


if __name__ == "__main__":
    try:
        print(read_version())
    except (OSError, ValueError, KeyError) as error:
        sys.exit(str(error))
