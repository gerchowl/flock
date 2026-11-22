"""Observe native PTY geometry changes for the isolated split fixture."""
import os
from pathlib import Path
import select
import signal
import sys


def main():
    destination = Path(sys.argv[1])
    full_columns = int(sys.argv[2])
    resize_read, resize_write = os.pipe()
    signal.signal(signal.SIGWINCH, lambda *_: os.write(resize_write, b"r"))
    phase = "base"
    pending = None
    previous_columns = None

    def emit(marker, rows, columns):
        with destination.with_name(destination.name + ".events").open("a") as trace:
            trace.write(f"{marker} {rows} {columns}\n")
        print(marker, flush=True)

    def sample():
        nonlocal phase, pending, previous_columns
        columns, rows = os.get_terminal_size(0)
        if phase == "base" and columns == full_columns:
            previous_columns = columns
            phase = "first"
            emit("ARM1608", rows, columns)
        elif pending is not None and columns < previous_columns:
            previous_columns = columns
            if pending == "first":
                phase = "second"
                marker = "SP1"
            else:
                destination.write_text(f"{rows} {columns}\n")
                phase = "done"
                marker = "SZA"
            pending = None
            emit(marker, rows, columns)

    sample()
    while phase != "done":
        ready, _, _ = select.select([0, resize_read], [], [])
        if resize_read in ready:
            os.read(resize_read, 4096)
            sample()
        if 0 in ready:
            data = os.read(0, 4096)
            if not data:
                raise EOFError("observer input closed before both splits")
            command = data.decode().strip()
            if command != phase or pending is not None:
                raise RuntimeError(f"unexpected command {command!r} in {phase!r}")
            pending = command
            sample()


try:
    main()
except Exception:
    print("FAIL1608", flush=True)
    raise
