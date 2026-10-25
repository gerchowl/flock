#!/usr/bin/env python3
"""Run #1833's actual-server/browser dogfood in the public admission sandbox."""
import argparse
import importlib.util
import json
from pathlib import Path
import os
import subprocess
import sys

spec = importlib.util.spec_from_file_location("structured_public", Path(__file__).with_name("structured_public.py"))
public = importlib.util.module_from_spec(spec)
spec.loader.exec_module(public)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--flk", required=True, type=Path)
    parser.add_argument("--disable", choices=("stream", "send", "permission", "decide", "result", "fleet", "revocation"))
    parser.add_argument("--verify-controls", action="store_true")
    args = parser.parse_args()
    binary = str(args.flk.resolve(strict=True))
    repo = Path(__file__).resolve().parents[2]
    if args.verify_controls:
        assert args.disable is None
        evidence = []
        for disabled in (None, "stream", "send", "permission", "decide", "result", "fleet", "revocation"):
            command = [sys.executable, "-I", str(Path(__file__).resolve()), "--flk", binary]
            if disabled:
                command.extend(["--disable", disabled])
            result = subprocess.run(command, env={"PATH": os.environ["PATH"]}, capture_output=True, text=True, timeout=45)
            if disabled:
                stages = {
                    "stream": ("real permission stream and fleet handle",),
                    "send": ("dogfood-1", '"scope"'),
                    "permission": ("real permission stream and fleet handle",),
                    "decide": ("permission_request_unknown", '"scope"'),
                    "result": ("decision close, harness result and fleet clear",),
                    "fleet": ("unexpected reconnect",),
                    "revocation": ("revocation close",),
                }
                assert result.returncode == 1 and all(stage in result.stderr for stage in stages[disabled]), (disabled, result.stdout, result.stderr)
            else:
                assert result.returncode == 0, (result.stdout, result.stderr)
            evidence.append({"disabled": disabled, "exit": result.returncode})
        print(json.dumps({"status": "passed", "causal_controls": evidence}))
        return

    def wire(cli, api, agent, env, root):
        if args.disable:
            (root / "proof-disable").write_text(args.disable)
        scopes = ["read", "send", "answer", "operate"]
        if args.disable == "send":
            scopes.remove("send")
        if args.disable == "decide":
            scopes.remove("answer")
        token = cli("client", "token", "add", "dogfood-wire", "--scopes", ",".join(scopes), "--show-token")["result"]["issued"]["token"]
        reader = cli("client", "token", "add", "dogfood-reader", "--scopes", "read", "--show-token")["result"]["issued"]["token"]
        endpoint = api("client.open", {})["result"]["url"].split("#", 1)[0]
        endpoint = endpoint.replace("http://", "ws://").rstrip("/") + "/v1/ws"
        result = subprocess.run(["bun", str(repo / "web/client/tests/wire/dogfood-server.ts"), endpoint, token, reader, agent, binary, args.disable or "none"],
                                cwd=root, env=env, capture_output=True, text=True, timeout=30)
        if result.returncode != 0:
            artifacts = repo / ".local" / "1833-r2-server-evidence"
            artifacts.mkdir(parents=True, exist_ok=True)
            for index, path in enumerate(root.rglob("*.log")):
                (artifacts / f"{index}-{path.name}").write_bytes(path.read_bytes())
        assert result.returncode == 0, (result.stdout, result.stderr)
        proof = json.loads(result.stdout)
        assert proof["status"] == "passed", proof
        print(json.dumps(proof), flush=True)

    proof = public.run(binary, "capacity", wire=wire)
    assert proof["status"] == "passed", proof


if __name__ == "__main__":
    main()
