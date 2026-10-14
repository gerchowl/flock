"""Owned supervision fixture. No harness, model, socket server or network use."""
import os
import pathlib
import signal
import sys
import time

root = pathlib.Path(sys.argv[1])
mode = sys.argv[2]
signal.signal(signal.SIGTERM, lambda *_: (root / "term").write_text("term"))
if mode == "tree":
    grandchild = os.fork()
    if grandchild == 0:
        os.setsid()
        signal.signal(signal.SIGTERM, signal.SIG_DFL)
        (root / "grandchild").write_text(str(os.getpid()))
        while True:
            time.sleep(0.01)
else:
    grandchild = None
(root / "ready").write_text(str(os.getpid()))
while True:
    if (root / "crash").exists():
        os._exit(7)
    if (root / "finish").exists():
        if grandchild is not None:
            os.kill(grandchild, signal.SIGTERM)
            os.waitpid(grandchild, 0)
        sys.exit(0)
    if grandchild is not None:
        waited, _ = os.waitpid(grandchild, os.WNOHANG)
        if waited:
            sys.exit(0)
    time.sleep(0.01)
