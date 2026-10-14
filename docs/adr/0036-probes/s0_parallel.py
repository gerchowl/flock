#!/usr/bin/env python3
"""S0 probe 3 (ADR-0036 §2b case 1): parallel control requests, and an
AskUserQuestion while a permission request is open.

The prompt asks for two tool calls in ONE assistant message: a Bash
`touch` (which needs permission: no `--allowedTools`) and an
AskUserQuestion. The driver leaves the first `can_use_tool` open and
watches for `hold_s` seconds whether the second one arrives. If it does, it
answers the second first and watches whether that tool's result arrives
while the first is still open; then it answers the first. Both orders are
tried (`touch` first in the prompt, and the question first), and also two
parallel permission-needing calls (two `touch`es). The `*_mcp*` shapes pair
the `touch` with a pre-allowed MCP tool (`s0_mcpstub.py`, standing in for
`flock_question_ask`) and record whether its result arrives while the
permission request is open. `sleep_then_mcp` is their control: the same MCP
call behind a pre-allowed `sleep 5` (and behind a pre-allowed, writing
`sleep 5 && touch`), with no permission open. `read_behind_permission`
puts a concurrency-safe Read after the prompting `touch` in the same message
and records whether the Read result arrives before the permission is
answered.

    S0_OAUTH_TOKEN_FILE=<token file> S0_OUT=/tmp/s0 \\
        python3 -I docs/adr/0036-probes/s0_parallel.py [runs] [hold_s]
"""

import json
import os
import sys

sys.dont_write_bytecode = True  # importing s0lib must not leave a __pycache__ here
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import s0lib as L  # noqa: E402

FLAGS = ["--tools", "Bash,Read,AskUserQuestion", "--permission-prompt-tool", "stdio"]

PROMPTS = {
    "touch_then_ask": (
        "In ONE response, make exactly these two tool calls in parallel, in this order, "
        "without waiting for either: (1) Bash: `touch s0-probe.txt`; (2) AskUserQuestion: "
        "ask me whether I prefer red or blue. Then summarise both outcomes in one line."
    ),
    "ask_then_touch": (
        "In ONE response, make exactly these two tool calls in parallel, in this order, "
        "without waiting for either: (1) AskUserQuestion: ask me whether I prefer red or "
        "blue; (2) Bash: `touch s0-probe.txt`. Then summarise both outcomes in one line."
    ),
    "touch_then_mcp": (
        "In ONE response, make exactly these two tool calls in parallel, in this order, "
        "without waiting for either: (1) Bash: `touch s0-probe.txt`; (2) mcp__s0stub__echo "
        "with text 'mcp-ok'. Then summarise both outcomes in one line."
    ),
    "mcp_then_touch": (
        "In ONE response, make exactly these two tool calls in parallel, in this order, "
        "without waiting for either: (1) mcp__s0stub__echo with text 'mcp-ok'; (2) Bash: "
        "`touch s0-probe.txt`. Then summarise both outcomes in one line."
    ),
    "touch_touch": (
        "In ONE response, make exactly these two Bash tool calls in parallel, without waiting "
        "for either: `touch s0-one.txt` and `touch s0-two.txt`. Then say done."
    ),
}


# The §2b claim under test: an MCP question tool that needs no permission
# (pre-allowed) does not queue behind an open permission request.
STUB = os.path.join(os.path.dirname(os.path.abspath(__file__)), "s0_mcpstub.py")
MCP_FLAGS = [
    "--mcp-config", json.dumps({"mcpServers": {"s0stub": {"command": "python3", "args": ["-I", STUB]}}}),
    "--allowedTools", "mcp__s0stub__echo",
]


# The control for the shapes above (A.13, pending): the same MCP call behind a
# pre-allowed, non-concurrency-safe Bash `sleep 5`, with NO permission open.
# If the MCP result still waits for the sleep, what held it in
# `touch_then_mcp` is serial execution of non-concurrent tools, not the open
# permission request.
CONTROLS = {
    "sleep_then_mcp": (
        "In ONE response, make exactly these two tool calls in parallel, in this order, "
        "without waiting for either: (1) Bash: `sleep 5`; (2) mcp__s0stub__echo with text "
        "'mcp-ok'. Then summarise both outcomes in one line.",
        ["--mcp-config", MCP_FLAGS[1], "--allowedTools", "mcp__s0stub__echo", "Bash(sleep:*)"],
    ),
    # `sleep` alone may be classed read-only, and so concurrency-safe: this
    # one is pre-allowed but writes, so it is not
    "slowtouch_then_mcp": (
        "In ONE response, make exactly these two tool calls in parallel, in this order, "
        "without waiting for either: (1) Bash: `sleep 5 && touch s0-slow.txt`; (2) "
        "mcp__s0stub__echo with text 'mcp-ok'. Then summarise both outcomes in one line.",
        ["--mcp-config", MCP_FLAGS[1], "--allowedTools", "mcp__s0stub__echo", "Bash(sleep:*)", "Bash(touch:*)"],
    ),
}


def run_control(shape, n):
    prompt, extra = CONTROLS[shape]
    with L.Session(f"parallel-{shape}-{n}", FLAGS + extra) as sess:
        out = {"shape": shape, "run": n}
        sess.user(prompt)
        pos = 0
        while True:
            e = sess.wait(lambda e: L.is_can_use_tool(e) or L.is_result(e), timeout=120, start=pos)
            if e is None or L.is_result(e):
                res = e
                break
            out.setdefault("unexpected_prompts", []).append(e["request"].get("tool_name"))
            sess.answer(e["request_id"], answer_for(e, "deny"))
            pos = e["_i"] + 1
        evs = sess.since(0)
        uses = L.tool_uses(evs)
        results = {x["tool_use_id"]: x["t"] for x in L.tool_results(evs)}
        out["calls"] = [{"name": u["name"], "msg": u["msg"], "use_t": u["t"], "result_t": results.get(u["id"])} for u in uses]
        sl = next((c for c in out["calls"] if c["name"] == "Bash"), None)
        mc = next((c for c in out["calls"] if c["name"].startswith("mcp__")), None)
        if sl and mc and sl["result_t"] and mc["result_t"]:
            out["one_message"] = sl["msg"] == mc["msg"]
            out["mcp_waited_for_sleep"] = mc["result_t"] >= sl["result_t"]
        out["result"] = None if res is None else {"text": res.get("result"), "usage": L.usage(res)}
        out["exit"] = sess.close()
        return out


# A.13's case left untested by the controls: a concurrency-safe call (Read)
# ordered after a prompting Bash `touch` in the same message. The driver holds
# the `touch` permission open for `hold_s` and records whether the Read result
# arrives before it answers. If it does, an open permission holds only the
# non-concurrency-safe calls after it.
READ_BEHIND = {
    "read_behind_permission": (
        "In ONE response, make exactly these two tool calls in parallel, in this order, "
        "without waiting for either: (1) Bash: `touch s0-probe.txt`; (2) Read: the file "
        "`a.txt` in the working directory. Then summarise both outcomes in one line."
    ),
}


def run_read_behind(shape, n, hold_s):
    with L.Session(f"parallel-{shape}-{n}", FLAGS) as sess:
        out = {"shape": shape, "run": n, "hold_s": hold_s}
        sess.user(READ_BEHIND[shape])
        first = sess.wait(lambda e: L.is_can_use_tool(e) or L.is_result(e), timeout=120)
        if first is None or L.is_result(first):
            out["error"] = "no control request"
            sess.close()
            return out
        out["first_request"] = {"tool": first["request"].get("tool_name"), "tool_use_id": first["request"].get("tool_use_id"), "t": first["_t"]}
        # the Read never prompts in the default mode: does its result arrive
        # while the permission request is open?
        got = sess.wait(lambda e: any(u["name"] == "Read" for u in L.tool_uses([e])), timeout=10)
        rid = None if got is None else [u["id"] for u in L.tool_uses([got]) if u["name"] == "Read"][0]
        res_ev = None if rid is None else sess.wait(
            lambda e: any(x["tool_use_id"] == rid for x in L.tool_results([e])), timeout=hold_s)
        out["read_result_while_permission_open"] = None if res_ev is None else {
            "t": res_ev["_t"], "content": [x["content"] for x in L.tool_results([res_ev]) if x["tool_use_id"] == rid]}
        # any other prompt that arrived while the first one was open
        out["prompts_while_first_open"] = [
            e["request"].get("tool_name") for e in sess.since(first["_i"] + 1) if L.is_can_use_tool(e)]
        answered = {first["request_id"]}
        out["first_answered_at"] = sess.now()
        sess.answer(first["request_id"], answer_for(first, "deny"))
        pos = first["_i"] + 1
        while True:
            e = sess.wait(lambda e: (L.is_can_use_tool(e) and e["request_id"] not in answered) or L.is_result(e), timeout=120, start=pos)
            if e is None or L.is_result(e):
                res = e
                break
            out.setdefault("unexpected_prompts", []).append(e["request"].get("tool_name"))
            answered.add(e["request_id"])
            sess.answer(e["request_id"], answer_for(e, "allow" if e["request"].get("tool_name") == "Read" else "deny"))
            pos = e["_i"] + 1
        evs = sess.since(0)
        uses = L.tool_uses(evs)
        results = {x["tool_use_id"]: x["t"] for x in L.tool_results(evs)}
        out["calls"] = [{"name": u["name"], "msg": u["msg"], "use_t": u["t"], "result_t": results.get(u["id"])} for u in uses]
        bc = next((c for c in out["calls"] if c["name"] == "Bash"), None)
        rc = next((c for c in out["calls"] if c["name"] == "Read"), None)
        if bc and rc:
            out["one_message"] = bc["msg"] == rc["msg"]
        if rc and rc["result_t"] is not None:
            out["read_result_before_permission_answered"] = rc["result_t"] < out["first_answered_at"]
        out["timeline"] = [
            {"t": e["_t"], "ev": f"can_use_tool:{e['request'].get('tool_name')}"} for e in evs if L.is_can_use_tool(e)
        ] + [{"t": x["t"], "ev": f"tool_result:{x['tool_use_id'][-6:]}:{'err' if x['is_error'] else 'ok'}"} for x in L.tool_results(evs)]
        out["timeline"].sort(key=lambda x: x["t"])
        out["result"] = None if res is None else {"text": res.get("result"), "is_error": res.get("is_error"), "num_turns": res.get("num_turns"), "usage": L.usage(res)}
        out["exit"] = sess.close()
        return out


def answer_for(req, label):
    r = req["request"]
    if r.get("tool_name") == "AskUserQuestion":
        qs = r.get("input", {}).get("questions", [])
        return {"behavior": "allow", "updatedInput": {**r.get("input", {}), "answers": {q.get("question"): "blue" for q in qs}}}
    if label == "allow":
        return {"behavior": "allow", "updatedInput": r.get("input", {})}
    return {"behavior": "deny", "message": "denied by the S0 probe"}


def run(shape, n, hold_s):
    with L.Session(f"parallel-{shape}-{n}", FLAGS + (MCP_FLAGS if "mcp" in shape else [])) as sess:
        out = {"shape": shape, "run": n, "hold_s": hold_s}
        sess.user(PROMPTS[shape])
        first = sess.wait(lambda e: L.is_can_use_tool(e) or L.is_result(e), timeout=120)
        if first is None or L.is_result(first):
            out["error"] = "no control request"
            sess.close()
            return out
        uses = L.tool_uses(sess.since(0))
        out["tool_calls_in_first_message"] = [
            {"name": u["name"], "id": u["id"]} for u in uses if u["msg"] == (uses[0]["msg"] if uses else None)
        ]
        out["first_request"] = {"tool": first["request"].get("tool_name"), "tool_use_id": first["request"].get("tool_use_id"), "t": first["_t"]}
        if "mcp" in shape:
            # the MCP call never prompts: did its result arrive while the
            # permission request is open?
            got = sess.wait(lambda e: any(u["name"].startswith("mcp__") for u in L.tool_uses([e])), timeout=10)
            mid = None if got is None else [u["id"] for u in L.tool_uses([got]) if u["name"].startswith("mcp__")][0]
            res_ev = None if mid is None else sess.wait(
                lambda e: any(x["tool_use_id"] == mid for x in L.tool_results([e])), timeout=hold_s)
            out["mcp_result_while_permission_open"] = None if res_ev is None else {
                "t": res_ev["_t"], "content": [x["content"] for x in L.tool_results([res_ev]) if x["tool_use_id"] == mid]}
            mres = [x for x in L.tool_results(sess.since(0)) if x["tool_use_id"] == mid]
            out["mcp_result_before_permission"] = bool(mres) and mres[0]["t"] < first["_t"]
        second = sess.wait(L.is_can_use_tool, timeout=0 if "mcp" in shape else hold_s, start=first["_i"] + 1)
        out["second_while_first_open"] = None if second is None else {
            "tool": second["request"].get("tool_name"), "tool_use_id": second["request"].get("tool_use_id"), "t": second["_t"],
            "gap_s": round(second["_t"] - first["_t"], 3),
        }
        answered = set()

        def reply(req, how="deny"):
            answered.add(req["request_id"])
            sess.answer(req["request_id"], answer_for(req, how))

        if second is not None:
            reply(second)
            tid = second["request"].get("tool_use_id")
            got = sess.wait(lambda e: any(x["tool_use_id"] == tid for x in L.tool_results([e])), timeout=hold_s, start=second["_i"] + 1)
            out["second_result_while_first_open"] = None if got is None else {"t": got["_t"], "content": [x for x in L.tool_results([got]) if x["tool_use_id"] == tid]}
            out["first_answered_at"] = sess.now()
            reply(first)
        else:
            out["first_answered_at"] = sess.now()
            reply(first)
        pos = first["_i"] + 1
        while True:
            e = sess.wait(lambda e: (L.is_can_use_tool(e) and e["request_id"] not in answered) or L.is_result(e), timeout=120, start=pos)
            if e is None or L.is_result(e):
                res = e
                break
            if second is None and "second_after_first_answered" not in out:
                out["second_after_first_answered"] = {"tool": e["request"].get("tool_name"), "t": e["_t"],
                                                      "gap_after_answer_s": round(e["_t"] - out["first_answered_at"], 3)}
            reply(e)
            pos = e["_i"] + 1
        evs = sess.since(0)
        out["timeline"] = [
            {"t": e["_t"], "ev": f"can_use_tool:{e['request'].get('tool_name')}"} for e in evs if L.is_can_use_tool(e)
        ] + [{"t": x["t"], "ev": f"tool_result:{x['tool_use_id'][-6:]}:{'err' if x['is_error'] else 'ok'}"} for x in L.tool_results(evs)]
        out["timeline"].sort(key=lambda x: x["t"])
        out["result"] = None if res is None else {"text": res.get("result"), "is_error": res.get("is_error"), "num_turns": res.get("num_turns"), "usage": L.usage(res)}
        out["exit"] = sess.close()
        return out


def main():
    runs = int(sys.argv[1]) if len(sys.argv) > 1 else 2
    hold_s = float(sys.argv[2]) if len(sys.argv) > 2 else 10
    shapes = sys.argv[3:] or list(PROMPTS) + list(CONTROLS) + list(READ_BEHIND)

    def one(s, n):
        if s in CONTROLS:
            return run_control(s, n)
        if s in READ_BEHIND:
            return run_read_behind(s, n, hold_s)
        return run(s, n, hold_s)

    L.emit([one(s, i + 1) for i in range(runs) for s in shapes])


if __name__ == "__main__":
    main()
