"""Owned supervision fixture driven by signals; no model or network use."""
import os
import pathlib
import signal
import sys

root = pathlib.Path(sys.argv[1])
mode = sys.argv[2]
events = {signal.SIGTERM, signal.SIGUSR1, signal.SIGUSR2, signal.SIGCHLD}
signal.pthread_sigmask(signal.SIG_BLOCK, events)
if mode == "tree":
    ready_read, ready_write = os.pipe()
    grandchild = os.fork()
    if grandchild == 0:
        os.close(ready_read)
        os.setsid()
        (root / "grandchild").write_text(str(os.getpid()))
        os.write(ready_write, b"ready")
        os.close(ready_write)
        # TERM's default action ends this owned descendant, even before sigwait.
        signal.pthread_sigmask(signal.SIG_UNBLOCK, {signal.SIGTERM})
        while True:
            signal.pause()
    os.close(ready_write)
    assert os.read(ready_read, 5) == b"ready"
    os.close(ready_read)
else:
    grandchild = None
print("ready", flush=True)
while True:
    event = signal.sigwait(events)
    if event == signal.SIGTERM:
        (root / "term").write_text("term")
    elif event == signal.SIGUSR2:
        os._exit(7)
    elif event == signal.SIGUSR1:
        if grandchild is not None:
            os.kill(grandchild, signal.SIGTERM)
            os.waitpid(grandchild, 0)
        sys.exit(0)
    elif event == signal.SIGCHLD and grandchild is not None:
        waited, _ = os.waitpid(grandchild, os.WNOHANG)
        if waited:
            sys.exit(0)
