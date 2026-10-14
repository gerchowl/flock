#!/usr/bin/env python3
"""S0 probe 5 (ADR-0036 §10b, P25, #1321): `set_permission_mode` over stdio
in `-p`, mid-session, with no respawn.

One session, `--permission-prompt-tool stdio`, tools `Bash,Write`, started
in `default` mode. Each step asks for one `Write` of a new file and records
whether a `can_use_tool` arrived (it is denied), whether the file exists,
and the turn's `system/init.permissionMode`:

1. default: Write prompts,
2. `set_permission_mode {mode: acceptEdits}` at idle: Write does not prompt,
3. `set_permission_mode {mode: default}` at idle: Write prompts again,
4. mid-turn: the turn runs `sleep 4`, the driver switches to `acceptEdits`
   during the sleep, and the same turn then Writes,
5. other values: `bypassPermissions` (the session was not started with
   `--dangerously-skip-permissions`, as the refusal says), `plan`, and a
   bogus mode,
6. a plain reply, to read the mode the session is left in.

    S0_OAUTH_TOKEN_FILE=<token file> S0_OUT=/tmp/s0 \\
        python3 -I docs/adr/0036-probes/s0_permmode.py
"""

import os
import sys

sys.dont_write_bytecode = True  # importing s0lib must not leave a __pycache__ here
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import s0lib as L  # noqa: E402

FLAGS = ["--tools", "Bash,Write", "--allowedTools", "Bash(sleep:*)", "--permission-prompt-tool", "stdio"]


def turn(sess, prompt, during=None):
    mark = sess.mark()
    sess.user(prompt)
    step = {"prompt": prompt}
    if during is not None:
        use = sess.wait(lambda e: any(u["name"] == "Bash" for u in L.tool_uses([e])), timeout=60, start=mark)
        step["mid_turn_switch"] = during(sess) if use is not None else "no sleep call"
    asks = []
    pos = mark
    while True:
        e = sess.wait(lambda e: L.is_can_use_tool(e) or L.is_result(e), timeout=120, start=pos)
        if e is None or L.is_result(e):
            res = e
            break
        asks.append(e["request"].get("tool_name"))
        sess.answer(e["request_id"], {"behavior": "deny", "message": "denied by the S0 probe"})
        pos = e["_i"] + 1
    inits = [e for e in sess.since(mark) if e.get("type") == "system" and e.get("subtype") == "init"]
    step["init_permissionMode"] = [i.get("permissionMode") for i in inits]
    step["can_use_tool"] = asks
    step["tool_calls"] = [u["name"] for u in L.tool_uses(sess.since(mark))]
    step["result"] = None if res is None else {"subtype": res.get("subtype"), "text": str(res.get("result"))[:160]}
    return step


def set_mode(sess, mode, **extra):
    rid = sess.control("set_permission_mode", mode=mode, **extra)
    resp = sess.wait(L.is_control_response(rid), timeout=20)
    return {"mode": mode, "t": sess.now(), "response": None if resp is None else resp.get("response")}


def main():
    sess = L.Session("permmode", FLAGS)
    steps = []
    steps.append({"step": "1 default", **turn(sess, "Use the Write tool to create one.txt containing 1. Do nothing else.")})
    steps.append({"step": "2 set acceptEdits", "switch": set_mode(sess, "acceptEdits"),
                  **turn(sess, "Use the Write tool to create two.txt containing 2. Do nothing else.")})
    steps.append({"step": "3 set default", "switch": set_mode(sess, "default"),
                  **turn(sess, "Use the Write tool to create three.txt containing 3. Do nothing else.")})
    steps.append({"step": "4 mid-turn acceptEdits",
                  **turn(sess, "Run `sleep 4` with Bash. After it finishes, use the Write tool to create four.txt containing 4.",
                         during=lambda s: set_mode(s, "acceptEdits"))})
    steps.append({"step": "5 refused values", "switches": [set_mode(sess, m) for m in ("bypassPermissions", "plan", "bogus")]})
    # a plain reply only reads the mode: a Write in plan mode would write a
    # plan file into the profile's plans/ directory
    steps.append({"step": "6 after refusals", **turn(sess, "Reply with exactly: ok")})
    files = sorted(f for f in os.listdir(sess.cwd) if f.endswith(".txt") and f not in ("a.txt", "b.txt"))
    code = sess.close()
    L.emit({"steps": steps, "files_created": files, "exit": code})


if __name__ == "__main__":
    main()
