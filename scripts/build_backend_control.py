"""Owned process/cache lifetime and GC quarantine for the measurement harness."""

from contextlib import contextmanager
import errno
import os
from pathlib import Path
import signal
import socket
import subprocess
import threading


class RunInterrupted(InterruptedError):
    def __init__(self, signum):
        super().__init__(f"interrupted by {signal.Signals(signum).name}")
        self.signum = signum


def endpoint_alive(path):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(1)
        result = connection.connect_ex(str(path))
    if result not in (0, errno.ENOENT, errno.ECONNREFUSED):
        raise RuntimeError("cannot verify the private cache endpoint")
    return result == 0


class OwnedCache:
    def __init__(self, env, audit):
        self.env = env
        self.audit = audit
        self.endpoint = Path(env["SCCACHE_SERVER_UDS"])
        if self.endpoint != Path(env["SCCACHE_DIR"]) / "server.sock" or "SCCACHE_SERVER_PORT" in env:
            raise RuntimeError("cache endpoint must belong to the experiment directory")
        self.children = []
        self.lock = threading.RLock()
        self.cancelled = None
        self.cancel_event = threading.Event()
        self.starting = True
        self.started = False

    def spawn(self, argv, env=None, **kwargs):
        with self.lock:
            if self.cancelled:
                raise RunInterrupted(self.cancelled)
            child = subprocess.Popen(argv, env=env or self.env, start_new_session=True, **kwargs)
            self.children.append(child)
            return child

    def cancel_children(self):
        with self.lock:
            for child in self.children:
                if child.poll() is None:
                    try:
                        os.killpg(child.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
            for child in self.children:
                child.wait(timeout=30)

    def run(self, argv, **kwargs):
        child = self.spawn(argv, **kwargs)
        try:
            code = child.wait(timeout=30)
        except BaseException:
            self.cancel_children()
            raise
        if code:
            raise subprocess.CalledProcessError(code, argv)

    def interrupt(self, signum, _frame):
        self.cancelled = signum
        self.cancel_event.set()
        if not self.starting:
            self.cancel_children()
            raise RunInterrupted(signum)

    def start(self):
        if endpoint_alive(self.endpoint):
            raise RuntimeError("private cache endpoint already active; refusing to adopt it")
        self.started = True
        # Startup signals are deferred until the daemon has answered its client.
        # This closes the race between detached startup and finally cleanup.
        self.run(["sccache", "--start-server"], stdout=subprocess.DEVNULL,
                 stderr=subprocess.DEVNULL)
        self.starting = False
        if self.cancelled:
            raise RunInterrupted(self.cancelled)
        if not endpoint_alive(self.endpoint):
            raise RuntimeError("private cache startup did not produce a listening endpoint")
        self.audit["started"] = True

    def close(self):
        self.cancel_children()
        if not self.started:
            return
        if endpoint_alive(self.endpoint):
            stopped = subprocess.run(["sccache", "--stop-server"], env=self.env,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                     timeout=30)
            self.audit["stop_exit"] = stopped.returncode
        self.audit["endpoint_gone"] = not endpoint_alive(self.endpoint)
        if not self.audit["endpoint_gone"]:
            raise RuntimeError("private cache shutdown verification failed")


@contextmanager
def owned_cache(env, audit):
    cache = OwnedCache(env, audit)
    previous = {sig: signal.signal(sig, cache.interrupt)
                for sig in (signal.SIGINT, signal.SIGTERM)}
    try:
        cache.start()
        yield cache
    finally:
        # Finish cleanup even if another termination request arrives during it.
        for sig in previous:
            signal.signal(sig, signal.SIG_IGN)
        try:
            cache.close()
        finally:
            for sig, handler in previous.items():
                signal.signal(sig, handler)


def quarantine(rows):
    """GC is store-wide, so contamination of one concurrent member taints all."""
    contaminated = any(row.get("nix_auto_gc_messages", 0)
                       or row.get("nix_gc_lock_messages", 0) for row in rows)
    for row in rows:
        row["gc_contaminated"] = contaminated
        row["aggregate_eligible"] = not contaminated and row["exit"] == 0
    return not contaminated
