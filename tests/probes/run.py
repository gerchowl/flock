#!/usr/bin/env python3
"""Run a TUI probe with an owned, crash-sweepable server and binary copy (#529)."""
import argparse
import ctypes
import ctypes.util
import sys
import json
import os
from pathlib import Path
import selectors
import shutil
import subprocess
import tempfile
import time


def process_birth(pid):
    """Native birth identity matching the Rust sweep, with no executable lookup."""
    try:
        if sys.platform == "linux":
            return int(Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[19])
        if sys.platform == "darwin":
            # Public proc_bsdinfo ABI from the SDK's sys/proc_info.h.
            class BsdInfo(ctypes.Structure):
                _fields_ = [("ids", ctypes.c_uint32 * 12), ("names", ctypes.c_char * 48),
                            ("details", ctypes.c_uint32 * 6), ("seconds", ctypes.c_uint64),
                            ("micros", ctypes.c_uint64)]
            library = ctypes.util.find_library("proc")
            if library is None:
                return None
            info = BsdInfo()
            proc = ctypes.CDLL(library)
            proc.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64,
                                         ctypes.c_void_p, ctypes.c_int]
            size = proc.proc_pidinfo(pid, 3, 0, ctypes.byref(info), ctypes.sizeof(info))
            if size == ctypes.sizeof(info) and info.ids[3] == pid and info.seconds:
                return info.seconds * 1000000 + info.micros
    except (OSError, ValueError, IndexError):
        pass
    return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin", required=True, type=Path)
    parser.add_argument("--driver", required=True, nargs="+")
    parser.add_argument("--runbook", required=True, type=Path)
    parser.add_argument("--artifacts", type=Path, help="keep driver artifacts outside the disposable sandbox")
    parser.add_argument("--root", type=Path, default=Path("/tmp"), help=argparse.SUPPRESS)
    args = parser.parse_args()
    root = args.root.resolve(strict=True)
    if not root.is_relative_to(Path("/tmp").resolve()):
        parser.error("sandbox root must be under /tmp")
    source = args.bin.resolve(strict=True)
    runbook = args.runbook.resolve(strict=True)
    base = Path(tempfile.mkdtemp(prefix=f"flock-autodetect-test-{os.getpid()}-", dir=root))
    server = None
    try:
        for name in ("bin", "h", "s", "c", "r", "out"):
            (base / name).mkdir()
        binary = base / "bin/flk"
        shutil.copy2(source, binary)
        env = {key: value for key, value in os.environ.items() if not key.startswith("FLOCK_")}
        env.update(HOME=str(base / "h"), XDG_STATE_HOME=str(base / "s"), XDG_CONFIG_HOME=str(base / "c"),
                   XDG_RUNTIME_DIR=str(base / "r"), XDG_DATA_HOME=str(base / "data"),
                   CLAUDE_CONFIG_DIR=str(base / "h/.claude"), CODEX_HOME=str(base / "h/.codex"), SHELL="/bin/sh")
        for app in ("flock", "flock-dev"):
            config = base / "c" / app
            config.mkdir()
            (config / "config.toml").write_text("onboarding = false\n")
        server = subprocess.Popen([str(binary), "server"], cwd=base, env=env,
                                  stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        record = dict(pid=server.pid, binary=str(binary), home=env["HOME"], state=env["XDG_STATE_HOME"], start_time=process_birth(server.pid),
                      argv=[str(binary), "server"])
        pending = base / ".server.pending"
        with pending.open("w") as out:
            json.dump(record, out)
            out.flush()
            os.fsync(out.fileno())
        pending.replace(base / f".server-{server.pid}.json")
        directory = os.open(base, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
        output = bytearray()
        deadline = time.monotonic() + 30
        with selectors.DefaultSelector() as ready:
            ready.register(server.stdout, selectors.EVENT_READ)
            while b"flk server running;" not in output:
                if server.poll() is not None:
                    raise RuntimeError(f"server exited during startup: {output.decode(errors='replace')}")
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not ready.select(remaining):
                    raise TimeoutError("sandbox server never announced readiness")
                chunk = os.read(server.stdout.fileno(), 4096)
                if not chunk or len(output) + len(chunk) > 65536:
                    raise RuntimeError(f"incomplete server startup: {output.decode(errors='replace')}")
                output.extend(chunk)
        print(json.dumps(dict(sandbox=str(base))), flush=True)
        artifacts = args.artifacts.resolve() if args.artifacts else base / "out"
        result = subprocess.run([*args.driver, "run", str(runbook), "--artifacts", str(artifacts),
                                 "--var", f"bin={binary}", "--var", f"home={base / 'h'}", "--var", f"xdg={base / 'c'}"],
                                cwd=base, env=env)
        return result.returncode
    finally:
        if server is not None:
            if server.poll() is None:
                server.terminate()
                try:
                    server.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait(timeout=5)
            server.stdout.close()
        shutil.rmtree(base)


if __name__ == "__main__":
    raise SystemExit(main())
