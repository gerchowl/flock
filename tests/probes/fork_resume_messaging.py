#!/usr/bin/env python3
"""Exercise PTY inbox identity with owned fake harnesses, without model calls."""
import argparse
import json
import os
from pathlib import Path
import select
import socket
import subprocess
import sys
import tempfile
import time


def exchange(path, method, params):
    with socket.socket(socket.AF_UNIX) as peer:
        peer.settimeout(10)
        peer.connect(str(path))
        peer.sendall((json.dumps(dict(id="probe", method=method, params=params)) + "\n").encode())
        return json.loads(peer.makefile("rb").readline())


def harness():
    # The server supplies this process's pane environment and captures its birth.
    peer = socket.socket(socket.AF_UNIX)
    peer.settimeout(30)
    peer.connect(os.environ["PROBE_CONTROL"])
    stream = peer.makefile("rwb", buffering=0)
    stream.write((json.dumps(dict(pane=os.environ["FLOCK_PANE_ID"], argv=sys.argv,
                                 run_id=os.environ.get("FLOCK_RUN_ID"))) + "\n").encode())
    for line in stream:
        command = json.loads(line)
        if command.get("stop"):
            break
        if "cli" in command:
            env = dict(os.environ, FLOCK_PANE_ID=command["pane"])
            result = subprocess.run([os.environ["PROBE_BIN"], *command["cli"]],
                                    text=True, env=env, capture_output=True, timeout=10)
            answer = dict(code=result.returncode, stdout=result.stdout, stderr=result.stderr)
        elif "hook" in command:
            env = dict(os.environ, FLOCK_ENV="1", FLOCK_PANE_ID=command["pane"])
            result = subprocess.run([os.environ["PROBE_BIN"], "hook", "claude", "stop"],
                                    input=json.dumps(command["hook"]), text=True, env=env,
                                    capture_output=True, timeout=10)
            answer = dict(code=result.returncode, stdout=result.stdout, stderr=result.stderr)
        else:
            answer = exchange(os.environ["FLOCK_SOCKET_PATH"], command["method"], command["params"])
        stream.write((json.dumps(answer) + "\n").encode())


def success(answer):
    assert "result" in answer, answer
    return answer["result"]


def observe(binary, mode):
    with tempfile.TemporaryDirectory(prefix="fm-", dir="/tmp") as directory:
        root = Path(directory)
        api = root / "api"
        control = socket.socket(socket.AF_UNIX)
        control.bind(str(root / "ctl"))
        control.listen(4)
        control.settimeout(15)
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("FLOCK_", "CLAUDE", "CODEX_", "LD_", "DYLD_"))}
        env.update(HOME=directory, XDG_CONFIG_HOME=str(root / "c"),
                   XDG_RUNTIME_DIR=str(root / "r"), XDG_DATA_HOME=str(root / "d"),
                   XDG_STATE_HOME=str(root / "s"), XDG_CACHE_HOME=str(root / "cache"),
                   FLOCK_SOCKET_PATH=str(api), FLOCK_CLIENT_SOCKET_PATH=str(root / "client"),
                   FLOCK_HOST_NAME="host", SHELL="/bin/sh", TERM="xterm-256color",
                   PROBE_CONTROL=str(root / "ctl"), PROBE_BIN=str(binary))
        env["PATH"] = directory + os.pathsep + env["PATH"]
        subprocess.run(["git", "init", "--quiet", directory], env=env, check=True,
                       capture_output=True, timeout=10)
        subprocess.run(["git", "-C", directory, "-c", "user.name=Fixture",
                        "-c", "user.email=fixture@example.invalid", "-c", "commit.gpgsign=false",
                        "commit", "--quiet", "--allow-empty", "-m", "fixture"],
                       env=env, check=True, capture_output=True, timeout=10)
        for app in ("flock", "flock-dev"):
            config = root / "c" / app
            config.mkdir(parents=True)
            (config / "config.toml").write_text('onboarding = false\n')
        fake = root / "claude"
        fake.write_text(f"#!{sys.executable}\nimport runpy\nrunpy.run_path({str(Path(__file__).resolve())!r}, run_name='__harness__')\n")
        fake.chmod(0o755)
        server = subprocess.Popen([str(binary), "server"], env=env, cwd=root,
                                  stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                  stderr=subprocess.STDOUT)
        peers = []
        try:
            output = bytearray()
            deadline = time.monotonic() + 30
            while b"client socket:" not in output:
                remaining = deadline - time.monotonic()
                assert remaining > 0 and select.select([server.stdout], [], [], remaining)[0], "silent server"
                chunk = os.read(server.stdout.fileno(), 65536)
                assert chunk, output.decode(errors="replace")
                output.extend(chunk)
            assert b"flk server running;" in output, output

            def accepted():
                peer, _ = control.accept()
                peer.settimeout(15)
                stream = peer.makefile("rwb", buffering=0)
                ready = json.loads(stream.readline())
                peers.append((peer, stream))
                return ready, stream

            def start(name, flags):
                success(exchange(api, "agent.start", dict(name=name, cwd=directory,
                                 argv=[str(fake), *flags], focus=False)))
                return accepted()

            def command(stream, value):
                stream.write((json.dumps(value) + "\n").encode())
                answer = stream.readline()
                assert answer, "harness exited"
                return json.loads(answer)

            def ask(stream, method, params):
                return command(stream, dict(method=method, params=params))

            def hook(stream, pane):
                return command(stream, dict(hook=dict(session_id="fixture-session",
                               last_assistant_message="※ recap: done"), pane=pane))

            parent, parent_stream = start("parent", [])
            flags = ["--resume", "fixture-session"]
            if mode == "fork":
                transcript = root / ".claude" / "projects" / "fixture" / "fixture-session.jsonl"
                transcript.parent.mkdir(parents=True)
                transcript.write_text('{"type":"user","sessionId":"fixture-session"}\n')
                success(ask(parent_stream, "pane.report_agent_session", dict(
                    pane_id=parent["pane"], source="flock:claude", agent="claude",
                    agent_session_id="fixture-session", session_start_source="startup")))
                success(exchange(api, "agent.fork", dict(target=parent["pane"], branch="fixture-fork",
                                 path=str(root / "fork"), pivot="", focus=False)))
                child, stream = accepted()
                assert "--fork-session" in child["argv"]
                assert child["run_id"].startswith("fork:"), child
            else:
                child, stream = start(mode, flags)
            assert child["pane"] != parent["pane"]
            assert "--resume" in child["argv"]
            success(exchange(api, "msg.send", dict(to={"type": "pane", "pane": parent["pane"]},
                            body="parent mail", intent="needs_reply")))
            refused = ask(stream, "msg.read", dict(pane=parent["pane"]))
            assert refused["error"]["code"] == "operator_only", refused
            # Explicit stale targets must not wake for an inbox the caller cannot read.
            wake = ask(stream, "msg.wake", dict(pane=parent["pane"]))
            print(mode, "stale wake", json.dumps(wake), flush=True)
            # Emulate an inherited environment after fork/resume. The kernel ancestry
            # remains the child's, so it must never become the parent's mail identity.
            for _ in range(2):
                stopped = hook(stream, parent["pane"])
                assert stopped["code"] == 0, stopped
                print(mode, "hook", json.dumps(stopped), flush=True)
                assert not stopped["stdout"], stopped
            assert wake["error"]["code"] == "operator_only", wake
            own = success(ask(stream, "msg.read", {}))
            assert own["messages"] == [], own
            own_pane = success(ask(stream, "msg.wake", {}))["pane"]
            success(ask(parent_stream, "msg.send", dict(to={"type": "pane", "pane": child["pane"]},
                        body="child mail", intent="needs_reply")))
            stopped = hook(stream, parent["pane"])
            nudge = json.loads(stopped["stdout"])
            assert nudge["decision"] == "block" and "1 unread message" in nudge["reason"], stopped
            assert "`flk msg read`" in nudge["reason"], stopped
            read = command(stream, dict(cli=["msg", "read", "--json"], pane=parent["pane"]))
            assert read["code"] == 0, read
            read = json.loads(read["stdout"])
            messages = read.get("result", read)["messages"]
            assert len(messages) == 1 and messages[0]["body"] == "child mail", messages
            success(ask(stream, "msg.reply", dict(correlation_id=messages[0]["correlation_id"],
                        body="reply from child")))
            assert not hook(stream, parent["pane"])["stdout"]
            sent = success(ask(stream, "msg.send", dict(to={"type": "pane", "pane": parent["pane"]},
                           body="child reply", intent="fyi")))
            assert sent, sent
            mail = success(ask(parent_stream, "msg.read", {}))["messages"]
            assert len(mail) == 3, mail
            assert all(message["from_pane"] == own_pane for message in mail[1:]), mail
            print(mode, "PASS", flush=True)
        finally:
            for peer, stream in peers:
                try:
                    stream.write(b'{"stop":true}\n')
                except OSError:
                    pass
                stream.close()
                peer.close()
            control.close()
            server.terminate()
            try:
                server.wait(timeout=10)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait(timeout=5)


if __name__ == "__harness__":
    harness()
elif __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin", type=Path, required=True)
    parser.add_argument("--mode", choices=("fork", "resume"), required=True)
    args = parser.parse_args()
    observe(args.bin.resolve(), args.mode)
