"""Real-server failed-start fixture for #1371 (ADR-0037 §1).

Real replies and events are forwarded unchanged, with their ordering controlled
by child launch/exit and readiness subscription/snapshot stages. The child
acknowledges launch and exits either before the start response is released or
after the real readiness watch and live snapshot. The timeout child stays
gated before logging until rollback kills it. One failure-only deadline covers the
protocol and CLI; no timing sleep or product hook determines success. Python
is already required by just check.
"""

import json
import os
from pathlib import Path
import select
import shlex
import socket
import subprocess
import sys
import threading
import time


class Protocol:
    def __init__(self, ceiling=120):
        self.deadline = time.monotonic() + ceiling

    def remaining(self):
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("rollback fixture failure ceiling: producer stayed silent")
        return remaining

    def line(self, stream):
        data = bytearray()
        while True:
            stream.settimeout(self.remaining())
            byte = stream.recv(1)
            if not byte:
                raise EOFError("rollback fixture producer closed before expected stage")
            if byte == b"\n":
                return data.decode()
            data.extend(byte)
            if len(data) > 65536:
                raise ValueError("rollback fixture stage exceeds line limit")

    def expect(self, stream, expected):
        actual = self.line(stream)
        if actual != expected:
            raise ValueError(f"expected {expected}, got {actual}")

    def eof(self, stream):
        stream.settimeout(self.remaining())
        if stream.recv(1):
            raise ValueError("unexpected output after terminal fixture stage")


def fixture(gate, base, behavior):
    with socket.socket(socket.AF_UNIX) as control:
        control.connect(gate)
        control.sendall(b"boot\n")
        command = control.recv(1)
        if behavior == "dead":
            return
        if behavior == "unexpected":
            control.sendall(b"wrong-stage\n")
            return
        if behavior == "silent" or command == b"H":
            # No readiness paint or launch log while gated, including under load.
            control.recv(1)
            return
        if command != b"L":
            raise ValueError(f"unexpected fixture release: {command!r}")
        with (Path(base) / "argv.log").open("a") as log:
            log.write(" ".join(sys.argv[5:]) + "\n")
        control.sendall(b"launched\n")
        if control.recv(1) != b"X":
            raise ValueError("fixture closed before exit release")
    sys.exit(1)


def listener(path):
    stream = socket.socket(socket.AF_UNIX)
    stream.bind(str(path))
    stream.listen()
    return stream


def request(protocol, path, method, params):
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(protocol.remaining())
        stream.connect(path)
        stream.sendall(json.dumps({"id": "rollback-fixture", "method": method, "params": params}).encode() + b"\n")
        return json.loads(protocol.line(stream))


def run(base, server, binary, mode):
    if mode not in {"exit-before-response", "response-before-exit", "timeout"}:
        raise ValueError(f"unexpected rollback mode: {mode}")
    protocol = Protocol()
    base = Path(base)
    gate_path = base / "rollback-gate.sock"
    proxy_path = base / "rollback-proxy.sock"
    controls = []
    failures = []
    stage_finished = threading.Event()
    steps = []
    subscribed = threading.Event()
    release_lock = threading.Lock()
    exit_released = False
    target_pane_id = None
    delivery_rx, delivery_tx = socket.socketpair()
    proxy = listener(proxy_path)
    gate = listener(gate_path)
    exits = socket.socket(socket.AF_UNIX)
    exits.settimeout(protocol.remaining())
    exits.connect(server)
    exits.sendall(json.dumps({
        "id": "rollback-watch", "method": "events.subscribe",
        "params": {"subscriptions": [{"type": "pane.exited"}]},
    }).encode() + b"\n")
    ack = json.loads(protocol.line(exits))
    if ack.get("result", {}).get("type") != "subscription_started":
        raise ValueError(f"unexpected exit subscription: {ack}")
    launcher = "#!/bin/sh\nexec " + " ".join(shlex.quote(arg) for arg in
        [sys.executable, str(Path(__file__).resolve()), "fixture", str(gate_path), str(base), "normal"]) + ' "$@"\n'
    (base / "bin" / "opencode").write_text(launcher)
    (base / "bin" / "opencode").chmod(0o755)

    def observe_exit(pane_id):
        while True:
            event = json.loads(protocol.line(exits))
            if event.get("event") != "pane_exited":
                raise ValueError(f"unexpected exit-monitor event: {event}")
            if event["data"]["pane_id"] == pane_id:
                steps.append("exit_queued")
                return

    def relay(client):
        nonlocal exit_released, target_pane_id
        subscription = False
        try:
            with client, socket.socket(socket.AF_UNIX) as upstream:
                upstream.settimeout(protocol.remaining())
                upstream.connect(server)
                message = protocol.line(client)
                subscription = json.loads(message)["method"] == "events.subscribe"
                upstream.sendall(message.encode() + b"\n")
                reply = protocol.line(upstream)
                if json.loads(message)["method"] == "agent.start":
                    agent = json.loads(reply)["result"]["agent"]
                    target_pane_id = agent["pane_id"]
                    while True:
                        readable, _, _ = select.select([gate, exits], [], [], protocol.remaining())
                        if not readable:
                            raise TimeoutError("fixture never connected before failure ceiling")
                        if exits in readable:
                            event = json.loads(protocol.line(exits))
                            if event.get("event") != "pane_exited":
                                raise ValueError(f"unexpected exit-monitor event: {event}")
                            if event["data"]["pane_id"] == agent["pane_id"]:
                                raise EOFError("fixture pane exited before connecting")
                        if gate in readable:
                            control, _ = gate.accept()
                            controls.append(control)
                            break
                    protocol.expect(control, "boot")
                    control.sendall(b"H" if mode == "timeout" else b"L")
                    if mode != "timeout":
                        protocol.expect(control, "launched")
                    if mode == "exit-before-response":
                        control.sendall(b"X")
                        protocol.eof(control)
                        observe_exit(agent["pane_id"])
                        stage_finished.set()
                    elif mode == "timeout":
                        stage_finished.set()
                    steps.append("response_released")
                if subscription and mode == "response-before-exit":
                    if json.loads(reply).get("result", {}).get("type") != "subscription_started":
                        raise ValueError(f"unexpected readiness subscription: {reply}")
                    steps.append("readiness_subscribed")
                    subscribed.set()
                client.sendall(reply.encode() + b"\n")
                if (
                    mode == "response-before-exit" and subscribed.is_set()
                    and json.loads(message)["method"] == "agent.get"
                ):
                    with release_lock:
                        release = not exit_released
                        exit_released = True
                    if release:
                        agent = json.loads(reply).get("result", {}).get("agent")
                        if not agent:
                            raise ValueError(f"missing live snapshot before exit: {reply}")
                        steps.append("live_snapshot_released")
                        controls[0].sendall(b"X")
                        protocol.eof(controls[0])
                        observe_exit(agent["pane_id"])
                        stage_finished.set()
                        delivery_tx.sendall(b"exit_queued\n")
                if subscription and "error" not in json.loads(reply):
                    while True:
                        event = protocol.line(upstream)
                        value = json.loads(event)
                        if (
                            mode == "response-before-exit" and value.get("event") == "pane_exited"
                            and value["data"]["pane_id"] == target_pane_id
                        ):
                            # The actual monitor event releases this actual delivery.
                            # EOF, malformed stages and silence fail via Protocol.
                            protocol.expect(delivery_rx, "exit_queued")
                            steps.append("exit_delivered")
                        client.sendall(event.encode() + b"\n")
        except (BrokenPipeError, ConnectionResetError):
            # A subscription consumer closes normally when the CLI finishes.
            if not subscription or not stage_finished.is_set():
                failures.append("unexpected connection closure during fixture transport")
                delivery_tx.shutdown(socket.SHUT_WR)
        except Exception as error:
            failures.append(str(error))
            delivery_tx.shutdown(socket.SHUT_WR)

    def accept():
        try:
            while True:
                proxy.settimeout(protocol.remaining())
                client, _ = proxy.accept()
                worker = threading.Thread(target=relay, args=(client,), daemon=True)
                worker.start()
        except OSError:
            return

    before = request(protocol, server, "workspace.list", {})
    threading.Thread(target=accept, daemon=True).start()
    environment = {key: value for key, value in os.environ.items() if not key.startswith("FLOCK_")}
    environment["FLOCK_SOCKET_PATH"] = str(proxy_path)
    child = subprocess.Popen([binary, "delegate", "start", "d1", "--brief", str(base / "briefs" / "task.md"),
        "--cwd", str(base / "work"), "--ready-timeout", "5000", "--json"],
        env=environment, cwd=base, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        stdout, stderr = child.communicate(timeout=protocol.remaining())
        if not stage_finished.is_set():
            raise ValueError(f"CLI finished without fixture handshake: {failures}: {stderr}")
        if mode == "timeout":
            protocol.eof(controls[0])
        if failures:
            raise ValueError(f"fixture transport failed: {failures}")
        after = request(protocol, server, "workspace.list", {})
        print(json.dumps({"code": child.returncode, "stdout": stdout, "stderr": stderr,
            "before": before, "after": after, "steps": steps}), flush=True)
    finally:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=5)
        proxy.close()
        gate.close()
        exits.close()
        delivery_rx.close()
        delivery_tx.close()
        for control in controls:
            control.close()


def check_stage(behavior):
    """Exercise the same receiver with a dead, malformed or live silent child."""
    import tempfile
    with tempfile.TemporaryDirectory(prefix="stage-", dir=tempfile.gettempdir()) as base:
        gate_path = str(Path(base) / "g.sock")
        with listener(gate_path) as gate:
            producer = subprocess.Popen([sys.executable, __file__, "fixture", gate_path, base, behavior])
            try:
                protocol = Protocol()
                gate.settimeout(protocol.remaining())
                stream, _ = gate.accept()
                with stream:
                    protocol.expect(stream, "boot")
                    stream.sendall(b"L")
                    # A short injected failure ceiling tests failure, never a duration assertion.
                    protocol = Protocol(1)
                    try:
                        protocol.expect(stream, "launched")
                    except (TimeoutError, EOFError, ValueError) as error:
                        print(json.dumps({"error": type(error).__name__, "reason": str(error),
                            "producer_alive": producer.poll() is None}), flush=True)
                        return 1
                    raise AssertionError("injected stage failure unexpectedly succeeded")
            finally:
                if producer.poll() is None:
                    producer.kill()
                producer.wait(timeout=5)


if __name__ == "__main__":
    if sys.argv[1] == "fixture":
        fixture(*sys.argv[2:5])
    elif sys.argv[1] == "check-stage":
        sys.exit(check_stage(sys.argv[2]))
    else:
        run(*sys.argv[1:5])
