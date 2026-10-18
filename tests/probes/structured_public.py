#!/usr/bin/env python3
"""#1843 dogfood: real public CLI/MCP, fake Claude, isolated server and HOME.

Run: python3 -I tests/probes/structured_public.py --flk target/debug/flk
All waits use pipes, socket reads or readiness output with finite ceilings.
The probe owns and joins its server and model descendants. No live profiles.
"""
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


def run(binary, scenario):
    repo = Path(__file__).resolve().parents[2]
    with tempfile.TemporaryDirectory(prefix="fc", dir="/tmp") as temporary:
        root = Path(temporary).resolve()
        (root / "bin").mkdir()
        (root / "project").mkdir()
        for name in ("ready", "turn-ready", "resume-ready"):
            os.mkfifo(root / name, 0o600)
        ready = os.open(root / "ready", os.O_RDWR | os.O_NONBLOCK)
        turn_ready = os.open(root / "turn-ready", os.O_RDWR | os.O_NONBLOCK)
        resume_ready = os.open(root / "resume-ready", os.O_RDWR | os.O_NONBLOCK)
        fake = root / "bin" / "claude"
        fake.write_text(f"#!{sys.executable}\n" + (repo / "tests/fixtures/structured-public/claude.py").read_text())
        fake.chmod(0o700)
        (root / "init.json").write_bytes((repo / "tests/fixtures/structured-runtime/restricted-init.json").read_bytes())
        token = root / "token"
        token.write_text("fixture-token-not-a-credential")
        token.chmod(0o600)
        env = {"PATH": str(root / "bin") + os.pathsep + os.environ["PATH"],
               "HOME": str(root), "XDG_CONFIG_HOME": str(root / "x"),
               "XDG_STATE_HOME": str(root / "s"), "TMPDIR": str(root),
               "SHELL": "/bin/sh", "LANG": "C", "LC_ALL": "C",
               "USER": "fixture-user", "LOGNAME": "fixture-user", "TERM": "xterm"}
        # Explicit, owned short endpoint; no inherited FLOCK variable survives.
        env["FLOCK_SOCKET_PATH"] = str(root / "a")
        env["FLOCK_CLIENT_SOCKET_PATH"] = str(root / "c")
        def configure(operator=True, enabled=True, capacity=1, depth=2, fanout=2):
            config = f'''onboarding = false
[structured]
run_as = '{"operator" if operator else "flk-agent"}'
profile = 'dogfood'
token_file = {json.dumps(str(token))}
[security]
agent_account = 'none'
[fleet]
agent_spawn_enabled = {str(enabled).lower()}
max_concurrent_agents = {capacity}
max_spawn_depth = {depth}
max_spawn_fanout = {fanout}
'''
            for app in ("flock", "flock-dev"):
                directory = root / "x" / app
                directory.mkdir(parents=True, exist_ok=True)
                (directory / "config.toml").write_text(config)
        configure(operator=scenario != "default", enabled=scenario != "disabled",
                  capacity=1 if scenario == "capacity" else 8,
                  depth=0 if scenario == "depth" else 2,
                  fanout=1 if scenario == "fanout" else 2)
        def cli(*args, code=0):
            result = subprocess.run([binary, "--json", *args], env=env, cwd=root / "project",
                                    capture_output=True, text=True, timeout=15)
            assert result.returncode == code, (args, result.returncode, result.stdout, result.stderr)
            return json.loads(result.stdout) if result.stdout.strip() else None
        def api(method, params):
            with socket.socket(socket.AF_UNIX) as client:
                client.settimeout(10)
                client.connect(env["FLOCK_SOCKET_PATH"])
                client.sendall((json.dumps({"id":"dogfood", "method":method,"params":params}) + "\n").encode())
                with client.makefile("rb") as stream:
                    return json.loads(stream.readline())
        def observe(target, wanted):
            deadline = time.monotonic() + 10
            while True:
                agent = cli("agent", "status", target)["result"]["agent"]
                if (agent["lifecycle"] == "offline" if wanted == "offline" else agent["state"]["turn"] == "idle"):
                    if wanted != "offline":
                        assert agent["state"]["source"]["turn"] == "session"
                    return agent
                assert agent["lifecycle"] in ("live", "cold"), agent
                assert server.poll() is None and time.monotonic() < deadline, (wanted, agent)
        server = subprocess.Popen([binary, "server"], env=env, cwd=root / "project",
                                  stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        channel = None
        try:
            deadline = time.monotonic() + 15
            output = b""
            while b"client socket:" not in output:
                assert server.poll() is None, output.decode(errors="replace")
                remaining = deadline - time.monotonic()
                assert remaining > 0 and select.select([server.stdout], [], [], remaining)[0], "server readiness missing"
                block = os.read(server.stdout.fileno(), 65536)
                assert block, output.decode(errors="replace")
                output += block
            start = ["agent", "start", "dogfood-parent", "--runtime", "structured", "--model", "dogfood-parent", "--cwd", str(root / "project"), "--", "claude"]
            if scenario == "default":
                refused = cli(*start, code=1)
                assert refused["error"]["code"] == "structured_needs_agent_user", refused
                cli("server", "stop")
                assert server.wait(timeout=15) == 0
                return {"scenario":scenario,"status":"passed"}
            created = cli(*start)["result"]["agent"]
            assert created["runtime"] == "structured" and "pane_id" not in created
            parent_id = created["agent_id"]
            assert select.select([ready], [], [], 10)[0], "fake parent readiness missing"
            assert os.read(ready, 1) == b"R"
            control = socket.socket(socket.AF_UNIX)
            control.settimeout(15)
            control.connect(str(root / "control"))
            channel = control.makefile("rwb", buffering=0)
            seq = 0
            def mcp(method, params):
                nonlocal seq
                seq += 1
                control.sendall((json.dumps({"jsonrpc":"2.0","id":seq,"method":method,"params":params}) + "\n").encode())
                result = json.loads(channel.readline())
                assert result["id"] == seq
                return result
            def tool(name, arguments):
                response = mcp("tools/call", {"name":name,"arguments":arguments})
                if "error" in response:
                    return response
                return json.loads(response["result"]["content"][0]["text"])
            mcp("initialize", {})
            tools = {t["name"] for t in mcp("tools/list", {})["result"]["tools"]}
            assert "flock_agent_stop" in tools
            assert "agent.start" not in tools
            workspace = api("workspace.create", {"cwd":str(root / "project")})
            assert "result" in workspace, workspace
            args = {"runtime":"structured", "agent":"claude", "prompt":"dogfood-task",
                    "model":"dogfood-child", "name":"dogfood-child",
                    "location":{"kind":"worktree_path","path":str(root / "project")}}
            raw = tool("agent.start", args)
            assert raw["error"]["data"]["refusal"] == "not_exposed_via_mcp", raw
            if scenario in ("disabled", "depth"):
                expected = "agent_spawn_disabled" if scenario == "disabled" else "at_lineage_depth"
                assert tool("flock_agent_start", args)["error"]["data"]["refusal"] == expected
                control.sendall(b'"close"\n')
                channel.close()
                channel = None
                control.close()
                cli("agent", "stop", parent_id)
                cli("server", "stop")
                assert server.wait(timeout=15) == 0
                return {"scenario":scenario,"status":"passed"}
            # Raw argv cannot replace the closed Claude launch or its admission.
            unattested = subprocess.run([binary, "mcp", "serve"], env=env, cwd=root,
                input=json.dumps({"jsonrpc":"2.0","id":"unattested", "method":"tools/call",
                    "params":{"name":"flock_agent_start","arguments":args}}) + "\n",
                capture_output=True, text=True, timeout=15)
            assert unattested.returncode == 0, unattested.stderr
            assert json.loads(unattested.stdout)["error"]["data"]["refusal"] == "spawn_caller_unknown"
            args["argv"] = ["untrusted-binary", "--unsafe"]
            child = tool("flock_agent_start", args)["agent"]
            child_id = child["agent_id"]
            assert child["runtime"] == "structured" and "pane_id" not in child
            assert select.select([turn_ready], [], [], 10)[0], "opening turn was not consumed"
            assert os.read(turn_ready, 1) == b"T"
            extra = dict(args, name="extra-child")
            expected = "at_agent_capacity" if scenario == "capacity" else "at_fanout_limit"
            assert tool("flock_agent_start", extra)["error"]["data"]["refusal"] == expected
            observe(child_id, "turn_idle")
            assert any("dogfood-complete" in path.read_text() for path in (root / "x").rglob("*.jsonl")), "turn result must be durably observed"
            listing = tool("flock_agent_list", {})
            assert {a["agent_id"] for a in listing["structured_agents"]} == {parent_id, child_id}
            assert all("pane_id" in a for a in listing["agents"])
            for target in (child_id, "dogfood-child"):
                assert cli("agent", "get", target)["result"]["agent"]["agent_id"] == child_id
                assert cli("agent", "status", target)["result"]["agent"]["agent_id"] == child_id
                assert tool("flock_agent_get", {"target":target})["agent"]["agent_id"] == child_id
            assert tool("flock_agent_stop", {"target":child_id})["error"]["data"]["refusal"] == "not_authorized"
            assert cli("agent", "focus", child_id, code=2)["error"]["code"] == "runtime_unsupported"
            assert api("agent.start", {"runtime":"structured","name":"argv-bypass","argv":["untrusted-binary"]})["error"]["code"] == "invalid_agent_argv"
            cli("agent", "stop", child_id)
            observe(child_id, "offline")
            assert tool("flock_agent_start", args)["error"]["data"]["refusal"] == "agent_name_taken"
            for target in ("dogfood-child", "dogfood-parent"):
                control.sendall((json.dumps({"fixture_cli": ["agent", "start", target,
                    "--runtime", "structured", "--", "claude"]}) + "\n").encode())
                refusal = json.loads(channel.readline())
                assert refusal["exit"] == 13, refusal
                assert refusal["response"]["error"]["code"] == "operator_only", refusal
            resumed = cli("agent", "start", "dogfood-child", "--runtime", "structured", "--", "claude")["result"]["agent"]
            assert resumed["agent_id"] == child_id and resumed["session_id"] == child["session_id"]
            assert select.select([resume_ready], [], [], 10)[0], "operator resume readiness missing"
            assert os.read(resume_ready, 1) == b"R"
            # Fanout still bills the original parent after an operator resume.
            assert tool("flock_agent_start", extra)["error"]["data"]["refusal"] == expected
            cli("agent", "stop", child_id)
            observe(child_id, "offline")
            control.sendall(b'"close"\n')
            channel.close()
            channel = None
            control.close()
            cli("agent", "stop", parent_id)
            cli("server", "stop")
            assert server.wait(timeout=15) == 0
            return {"scenario":scenario,"status":"passed", "public_cli":True,"real_mcp":True,
                    "scoped_identity":True,"framed_opening_turn":True,"isolated_home":True}
        finally:
            if channel is not None:
                channel.close()
            if server.poll() is None:
                server.terminate()
                try:
                    server.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait(timeout=5)
            server.stdout.close()
            os.close(ready)
            os.close(turn_ready)
            os.close(resume_ready)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--flk", required=True)
    binary = str(Path(parser.parse_args().flk).resolve())
    proofs = [run(binary, scenario) for scenario in ("default", "disabled", "depth", "capacity", "fanout")]
    assert all(proof["status"] == "passed" for proof in proofs)
    print(json.dumps({"status":"passed", "scenarios":proofs, "real_mcp":True,
                      "framed_opening_turn":True,"spawn_policy_lineage_capacity_fanout":True}))
