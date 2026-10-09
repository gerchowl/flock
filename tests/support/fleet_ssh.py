#!/usr/bin/env python3
"""Local ssh transport for fleet.rs, including directed partitions and faults.

Fault modes alter handshake traffic or attempt an enrollment reset. Requests,
summary pushes and uplink frames traverse the real relay and enrollment gate.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time

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

# Gate only message sends on this directed edge. Discovery and held relays
# remain live, so a test can distinguish a blocked app loop from a slow hop.
gate = base / f"gate-message-{source}-{target}"
if "msg send" in command and gate.is_dir():
    (gate / "entered").touch()
    deadline = time.monotonic() + 30
    while not (gate / "release").exists():
        if not gate.is_dir() or time.monotonic() >= deadline:
            sys.stderr.write("fake-ssh: message gate was not released before its deadline\n")
            sys.exit(255)
        time.sleep(0.01)

# A topology cannot always start every pollee before its poller. Keep the
# initial relay from reporting no_local_server and entering enrollment backoff
# while the harness is still starting that target. Readiness markers survive
# restarts, so this does not hide later outages or alter partition fault modes.
ready = base / f"ready-{target}"
if "peers relay" in command and not ready.exists():
    (base / f"startup-wait-{source}-{target}").touch()
    deadline = time.monotonic() + 10
    while not ready.exists():
        if time.monotonic() >= deadline:
            sys.stderr.write(f"fake-ssh: initial server readiness timed out for {target}\n")
            sys.exit(255)
        time.sleep(0.01)

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
deliveries = set()
collections = set()


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
            mode = "disabled" if (base / f"old-peer-{target}").exists() else node["mesh"]
            if method == "mesh.collect":
                collections.add(request.get("id"))
                replay = base / f"replay-collect-{source}-{target}"
                if replay.exists():
                    request["params"] = json.loads(replay.read_text())
                    line = json.dumps(request) + "\n"
                gate = base / f"gate-collect-ack-{source}-{target}"
                if gate.is_dir() and request.get("params", {}).get("ack"):
                    (gate / "entered").touch()
                    deadline = time.monotonic() + 30
                    while not (gate / "release").exists() and gate.is_dir():
                        if time.monotonic() >= deadline:
                            break
                        time.sleep(0.01)
            if method == "mesh.deliver":
                deliveries.add(request.get("id"))
                if (base / f"spoof-host-{source}-{target}").exists():
                    envelope = request["params"]["envelope"]
                    payload = json.loads(bytes(envelope["body"]))
                    payload["message"]["from_host"] = "spoofed.example"
                    envelope["body"] = list(json.dumps(payload).encode())
                    line = json.dumps(request) + "\n"
                capture = base / f"capture-delivery-{source}-{target}"
                if capture.exists():
                    capture.write_text(json.dumps(request["params"]))
                    emit(json.dumps({"id": request.get("id"), "error": {
                        "code": "held_by_test", "message": "captured before receiver import",
                    }}) + "\n")
                    continue
                replay = base / f"replay-delivery-{source}-{target}"
                if replay.exists():
                    params = json.loads(replay.read_text())
                    params["forwarded_by"] = request["params"]["envelope"]["key"]["origin_node"]
                    request["params"] = params
                    line = json.dumps(request) + "\n"
                attack = base / f"forge-origin-{source}-{target}"
                if attack.exists():
                    params = request["params"]
                    envelope = params["envelope"]
                    params["forwarded_by"] = envelope["key"]["origin_node"]
                    envelope["key"]["origin_node"] = "disallowed.example"
                    envelope["return_binding"]["request"] = dict(envelope["key"])
                    line = json.dumps(request) + "\n"
                gate = base / f"gate-message-{source}-{target}"
                if gate.is_dir():
                    (gate / "entered").touch()
                    deadline = time.monotonic() + 30
                    while not (gate / "release").exists():
                        if not gate.is_dir() or time.monotonic() >= deadline:
                            break
                        time.sleep(0.01)
            if isinstance(method, str) and method.startswith("mesh.") and mode == "disabled":
                emit(json.dumps({"id": request.get("id"), "error": {
                    "code": "invalid_request", "message": f"unknown variant `{method}`",
                }}) + "\n")
            else:
                if mode == "forged_signature" and method == "mesh.hello" and request.get("params", {}).get("phase") == "finish":
                    request["params"]["signature"] = [0] * 64
                    line = json.dumps(request) + "\n"
                if mode == "legacy_dialer" and method == "mesh.hello":
                    request["method"] = "ping"
                    request["params"] = {}
                    line = json.dumps(request) + "\n"
                if mode == "relay_reset" and method == "peers.summary":
                    request["method"] = "peers.enroll_reset"
                    request["params"] = {"peer": source, "source": "inbound"}
                    line = json.dumps(request) + "\n"
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
        response = json.loads(line)
        if node["mesh"] == "forged_challenge":
            challenge = response.get("result", {}).get("challenge")
            if challenge:
                challenge["signature"] = [0] * 64
                line = json.dumps(response) + "\n"
        if node["mesh"] == "relay_reset" and response.get("error", {}).get("code") == "operator_only":
            (base / f"reset-refused-{target}").write_text(line)
        if response.get("id") in collections:
            collections.discard(response.get("id"))
            if response.get("error"):
                (base / f"collect-refused-{source}-{target}").write_text(line)
        if response.get("id") in deliveries:
            deliveries.discard(response.get("id"))
            gate = base / f"lose-receipt-{source}-{target}"
            if gate.exists() and response.get("result", {}).get("state") == "delivered":
                gate.rename(base / f"lost-receipt-{source}-{target}")
                line = json.dumps({"id": response["id"], "error": {
                    "code": "lost_receipt", "message": "receipt lost after inbox commit",
                }}) + "\n"
            elif response.get("result", {}).get("state") == "duplicate":
                (base / f"delivered-receipt-{source}-{target}").touch()
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
