#!/usr/bin/env python3
"""Local ssh transport for fleet.rs, including directed partitions and faults.

Only mesh requests are synthetic in fault modes. Every other request, summary
push and uplink frame still traverses the real flk peers relay process.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import threading

base = Path(__file__).resolve().parent.parent
manifest = json.loads((base / "nodes.json").read_text())
target, command = sys.argv[-2:]
source = os.environ.get("FLOCK_FLEET_SOURCE", "probe")
node = manifest["nodes"].get(target)
if node is None:
    sys.exit(255)
if (base / f"refuse-ssh-{target}").exists() or (
    base / f"refuse-edge-{source}-{target}"
).exists():
    sys.stderr.write(f"ssh: connect to host {target} port 22: Connection refused\n")
    sys.exit(255)

# Start empty and admit only the process basics, then set sandbox paths.
env = {
    key: value for key, value in os.environ.items()
    if key in {"PATH", "TMPDIR", "USER", "LANG"} or key.startswith("LC_")
}
env.update(
    HOME=node["home"],
    XDG_CONFIG_HOME=node["config"],
    XDG_RUNTIME_DIR=node["runtime"],
    XDG_DATA_HOME=str(Path(node["home"]) / "data"),
    XDG_STATE_HOME=str(Path(node["home"]) / "state"),
    XDG_CACHE_HOME=str(Path(node["home"]) / "cache"),
    FLOCK_SOCKET_PATH=node["socket"],
    FLOCK_FLEET_SOURCE=target,
    PATH=manifest["bin"] + os.pathsep + os.environ["PATH"],
)
if "peers relay" not in command:
    os.execve("/bin/sh", ["sh", "-c", command], env)

# Killing this group closes both halves of the held edge, without touching
# another node or any of the user's processes. The Rust owner checks argv
# against this fixture's unique script path before using the pid file.
os.setsid()
pid_file = base / "edges" / f"{source}-{target}-{os.getpid()}"
child = subprocess.Popen(
    ["/bin/sh", "-c", command], env=env, stdin=subprocess.PIPE,
    stdout=subprocess.PIPE, text=True, bufsize=1,
)
lock = threading.Lock()


def emit(line):
    with lock:
        sys.stdout.write(line)
        sys.stdout.flush()


def forward_input():
    try:
        for line in sys.stdin:
            try:
                request = json.loads(line)
            except ValueError:
                request = None
            if not isinstance(request, dict):
                sys.stderr.write("fake-ssh: non-object JSON request forwarded unchanged\n")
                sys.stderr.flush()
                child.stdin.write(line)
                child.stdin.flush()
                continue
            method = request.get("method", "")
            mode = node["mesh"]
            if isinstance(method, str) and method.startswith("mesh.") and mode != "native":
                if mode == "disabled":
                    code, message = "invalid_request", f"unknown variant `{method}`"
                else:
                    # A synthetic refusal, not a proposed mesh.hello schema.
                    code = "mesh_version_mismatch"
                    message = f"fixture mesh version {mode['version_mismatch']} is incompatible"
                emit(json.dumps({"id": request.get("id"), "error": {
                    "code": code, "message": message,
                }}) + "\n")
            else:
                child.stdin.write(line)
                child.stdin.flush()
    except (BrokenPipeError, ValueError):
        pass
    finally:
        child.stdin.close()


try:
    # Publish only after the group and relay exist. Atomic rename avoids a
    # partially written pid being mistaken for a stale record by the owner.
    pending = pid_file.with_suffix(".pending")
    pending.write_text(str(os.getpid()))
    pending.replace(pid_file)
    threading.Thread(target=forward_input, daemon=True).start()
    for line in child.stdout:
        emit(line)
    sys.exit(child.wait())
finally:
    pid_file.unlink(missing_ok=True)
    if child.poll() is None:
        child.terminate()
        try:
            child.wait(timeout=2)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()
