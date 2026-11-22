#!/usr/bin/env python3
"""S0 probe 1 (ADR-0036 E16, §1): the harness's own messaging socket.

For each variant: start a session, run one turn, read
`system/init.messaging_socket_path`, record the socket's and its directory's
owner and mode and the session-registry files under the config dir, then
connect to the socket as a plain same-uid process WITHOUT any token and
write (a) a `user` line, (b) a `user` line with `priority: "now"`, and
(c) a `control` `rename` line, and record whether a turn runs. Last, the
agent runs `printenv` through Bash to show whether its child processes
inherit the socket path and a token. The `restricted_profile` variant also
checks that a prompting `touch` still reaches the host over stdio.

Variants: default; `--restricted`; `CLAUDE_CODE_RESTRICTED=1`,
`--settings '{"crossSessionInbound":"refuse"}'`; and the same setting set to
`hold`.

    S0_OAUTH_TOKEN_FILE=<token file> S0_OUT=/tmp/s0 \\
        python3 -I docs/adr/0036-probes/s0_socket.py [variant ...]
"""

import glob
import json
import os
import shutil
import socket
import stat
import sys
import tempfile

sys.dont_write_bytecode = True  # importing s0lib must not leave a __pycache__ here
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import s0lib as L  # noqa: E402

FLAGS = ["--tools", "Bash,Read", "--allowedTools", "Bash(printenv:*)", "--permission-prompt-tool", "stdio"]

VARIANTS = {
    "default": ([], None),
    "restricted_flag": (["--restricted"], None),
    "restricted_env": ([], {"CLAUDE_CODE_RESTRICTED": "1"}),
    "inbound_refuse": (["--settings", json.dumps({"crossSessionInbound": "refuse"})], None),
    "inbound_hold": (["--settings", json.dumps({"crossSessionInbound": "hold"})], None),
}

# `--restricted` with the rest of the §1 launch profile: does a hook passed
# through `--settings` still run, and does an `--mcp-config` server attach?
# the marker lives in a private temporary dir that main() removes
HOOK_DIR = tempfile.mkdtemp(prefix="s0-hook-")
HOOK_MARK = os.path.join(HOOK_DIR, "stop-hook.mark")
STUB = os.path.join(os.path.dirname(os.path.abspath(__file__)), "s0_mcpstub.py")
VARIANTS["restricted_profile"] = ([
    "--restricted",
    "--settings", json.dumps({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": f"touch {HOOK_MARK}"}]}]}}),
    "--mcp-config", json.dumps({"mcpServers": {"s0stub": {"command": "python3", "args": ["-I", STUB]}}}),
    "--allowedTools", "mcp__s0stub__echo",
], None)
FIRST = {"restricted_profile": "Call the mcp__s0stub__echo tool with text 'stub-ok', then reply with exactly what it returned."}


def describe(path):
    try:
        st = os.lstat(path)
    except OSError as e:
        return {"path": path, "error": str(e)}
    return {
        "path": path,
        "uid": st.st_uid,
        "mode": oct(stat.S_IMODE(st.st_mode)),
        "kind": "socket" if stat.S_ISSOCK(st.st_mode) else "dir" if stat.S_ISDIR(st.st_mode) else "other",
    }


def send_lines(path, lines):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(5)
    try:
        s.connect(path)
        for obj in lines:
            s.sendall((json.dumps(obj) + "\n").encode())
        s.shutdown(socket.SHUT_WR)
        try:
            back = s.recv(4096)
        except OSError:
            back = b""
        return {"connected": True, "reply_bytes": back.decode(errors="replace")}
    except OSError as e:
        return {"connected": False, "error": str(e)}
    finally:
        s.close()


def turn_after(sess, start, timeout):
    """Did anything start a turn after `start` (a replayed user or a result)?"""
    res = sess.wait(L.is_result, timeout=timeout, start=start)
    evs = sess.since(start)
    replays = [
        e for e in evs
        if e.get("type") == "user" and e.get("isReplay")
    ]
    return {
        "result": None if res is None else {"t": res["_t"], "text": str(res.get("result"))[:200], "is_error": res.get("is_error")},
        "replayed_user_lines": len(replays),
        "replay_sample": None if not replays else json.dumps(replays[0].get("message", {}).get("content"))[:300],
        "event_types": sorted({f"{e.get('type')}/{e.get('subtype', '')}" for e in evs}),
    }


def run(name):
    flags, env = VARIANTS[name]
    with L.Session(f"socket-{name}", FLAGS + flags, env_extra=env) as sess:
        out = {"variant": name, "flags": flags, "env": env}
        if os.path.exists(HOOK_MARK):
            os.unlink(HOOK_MARK)
        sess.user(FIRST.get(name, "Reply with exactly: ready"))
        init = sess.wait(L.is_type("system", "init"), timeout=60)
        res = sess.wait(L.is_result, timeout=90)
        out["first_turn"] = None if res is None else {"text": res.get("result"), "is_error": res.get("is_error")}
        if init is None:
            out["init"] = None
            out["stderr"] = "".join(sess.stderr)[-1500:]
            sess.close()
            return out
        sock = init.get("messaging_socket_path")
        out["init"] = {
            "messaging_socket_path": sock,
            "capabilities": init.get("capabilities"),
            "permissionMode": init.get("permissionMode"),
            "tools": init.get("tools"),
            "slash_commands_n": len(init.get("slash_commands") or []),
            "claude_code_version": init.get("claude_code_version"),
            "mcp_servers": init.get("mcp_servers"),
        }
        out["hook_events"] = sorted({f"{e.get('subtype')}:{e.get('hook_event')}" for e in sess.since(0) if str(e.get("subtype", "")).startswith("hook_")})
        out["stop_hook_mark"] = os.path.exists(HOOK_MARK)
        if out["stop_hook_mark"]:
            os.unlink(HOOK_MARK)
        pid = sess.proc.pid
        cfg = sess.config_dir
        out["registry"] = [describe(p) for p in sorted(glob.glob(os.path.join(cfg, "sessions", f"{pid}*")))]
        if sock:
            out["socket"] = describe(sock)
            out["socket_dir"] = describe(os.path.dirname(sock))
            tests = [
                ("user_no_token", [{"type": "user", "message": {"content": "Reply with exactly: SOCKET-NEXT"}, "from": "s0-probe"}]),
                ("user_now_no_token", [{"type": "user", "priority": "now", "message": {"content": "Reply with exactly: SOCKET-NOW"}, "from": "s0-probe"}]),
                ("control_rename", [{"type": "control", "action": "rename", "name": "s0-renamed"}]),
            ]
            out["writes"] = {}
            for label, lines in tests:
                start = sess.mark()
                conn = send_lines(sock, lines)
                seen = turn_after(sess, start, timeout=45 if label != "control_rename" else 8)
                out["writes"][label] = {"conn": conn, **seen}
            out["registry_after"] = []
            for p in sorted(glob.glob(os.path.join(cfg, "sessions", f"{pid}*"))):
                d = describe(p)
                if p.endswith(".json"):
                    try:
                        with open(p) as f:
                            rec = json.load(f)
                        d["keys"] = sorted(rec.keys())
                        d["name"] = rec.get("name")
                        d["messagingSocketPath"] = rec.get("messagingSocketPath")
                    except (OSError, ValueError) as e:
                        d["read_error"] = str(e)
                out["registry_after"].append(d)
        if name == "restricted_profile":
            # does the stdio permission prompt still reach the host?
            start = sess.mark()
            sess.user("Run `touch s0-restricted.txt` with Bash, then say done.")
            req = sess.wait(lambda e: L.is_can_use_tool(e) or L.is_result(e), timeout=90, start=start)
            out["stdio_prompt"] = None if req is None or L.is_result(req) else req["request"].get("tool_name")
            if out["stdio_prompt"]:
                sess.answer(req["request_id"], {"behavior": "deny", "message": "denied by the S0 probe"})
            sess.wait(L.is_result, timeout=90, start=start)
        # does the agent's own Bash inherit the socket path (and a token)?
        start = sess.mark()
        sess.user("Run `printenv CLAUDE_CODE_MESSAGING_SOCKET` with Bash, then `printenv CLAUDE_CODE_MESSAGING_TOKEN | wc -c`, "
                  "and reply with the two outputs only.")
        res = sess.wait(L.is_result, timeout=90, start=start)
        out["bash_child_env"] = [x["content"] for x in L.tool_results(sess.since(start))]
        out["exit"] = sess.close()
        if sock:
            out["socket_after_exit"] = os.path.exists(sock)
        out["stderr_tail"] = "".join(sess.stderr)[-600:]
        return out


def main():
    names = sys.argv[1:] or list(VARIANTS)
    try:
        L.emit([run(n) for n in names])
    finally:
        shutil.rmtree(HOOK_DIR, ignore_errors=True)


if __name__ == "__main__":
    main()
