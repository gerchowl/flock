#!/usr/bin/env python3
"""S0 probe 2 (ADR-0036 §2a): the carrier of the deferral receipt for a deferred
AskUserQuestion, `deny` versus `allow`, and whether the model keeps working.

The prompt gives the model two independent tasks: ask the user a question
with AskUserQuestion, and count the files with `ls`. The driver answers the
AskUserQuestion control request with the §2a deferral receipt

    <question-id> submitted for user feedback - the response will be relayed to you

carried either as `deny` with that message or as `allow` with every
question's answer set to that text. It records whether the model went on to
run `ls` in the same turn, whether the turn ended with one `result` and no
further control request, the tool result the model saw, any
`permission_denied` / `permission_denials`, and the final text. Then it
delivers the answer as the §2a `reply` item (`Answer to <id>: blue`) in a
second turn and records whether the model used it.

    CLAUDE_PROFILE_DIR=~/.claude-profiles/work S0_OUT=/tmp/s0 \\
        python3 -I docs/adr/0036-probes/s0_carrier.py [runs-per-carrier]
"""

import hashlib
import json
import os
import re
import sys

sys.dont_write_bytecode = True  # importing s0lib must not leave a __pycache__ here
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import s0lib as L  # noqa: E402

FLAGS = ["--tools", "Bash,Read,AskUserQuestion", "--permission-prompt-tool", "stdio"]

PROMPT = (
    "You have two independent tasks. Task A: use the AskUserQuestion tool to ask me "
    "which colour I prefer for the report, red or blue. Task B: run `ls` in the current "
    "directory and count the files. Do task A first, then task B. Finish with one line "
    "per task saying what you found."
)


def norm(s):
    return re.sub(r"\s+", " ", str(s).strip())


def question_id(agent, q):
    """§2a: q- + 12 hex of sha256 over canonical JSON of the question."""
    body = {
        "agent": agent,
        "prompt": norm(q.get("question", "")),
        "options": [norm(o.get("label", "")) for o in q.get("options", [])],
        "multi_select": bool(q.get("multiSelect", False)),
    }
    canon = json.dumps(body, sort_keys=True, separators=(",", ":"))
    return "q-" + hashlib.sha256(canon.encode()).hexdigest()[:12]


def receipt(qid):
    return f"{qid} submitted for user feedback - the response will be relayed to you"


def run(carrier, n):
    sess = L.Session(f"carrier-{carrier}-{n}", FLAGS)
    out = {"carrier": carrier, "run": n}
    sess.user(PROMPT)
    init = sess.wait(L.is_type("system", "init"), timeout=60)
    out["ask_tool_listed"] = init is not None and "AskUserQuestion" in (init.get("tools") or [])
    req = sess.wait(lambda e: L.is_can_use_tool(e) or L.is_result(e), timeout=120)
    if req is None or L.is_result(req):
        out["error"] = "no AskUserQuestion control request"
        out["result"] = None if req is None else req.get("result")
        sess.close()
        return out
    r = req["request"]
    out["control_request"] = {
        "tool_name": r.get("tool_name"),
        "requires_user_interaction": r.get("requires_user_interaction"),
        "tool_use_id": r.get("tool_use_id"),
        "t": req["_t"],
    }
    if r.get("tool_name") != "AskUserQuestion":
        out["error"] = f"first control request was for {r.get('tool_name')}"
        sess.answer(req["request_id"], {"behavior": "deny", "message": "not part of this probe"})
        sess.close()
        return out
    qs = r.get("input", {}).get("questions", [])
    qid = question_id("s0-probe", qs[0]) if qs else "q-000000000000"
    text = receipt(qid)
    out["question_id"] = qid
    out["questions"] = [q.get("question") for q in qs]
    mark = sess.mark()
    if carrier == "deny":
        sess.answer(req["request_id"], {"behavior": "deny", "message": text})
    else:
        answers = {q.get("question"): text for q in qs}
        sess.answer(req["request_id"], {"behavior": "allow", "updatedInput": {**r.get("input", {}), "answers": answers}})
    res = sess.wait(lambda e: L.is_result(e) or L.is_can_use_tool(e), timeout=180, start=mark)
    evs = sess.since(mark)
    later_requests = [e for e in evs if L.is_can_use_tool(e)]
    while res is not None and L.is_can_use_tool(res):
        # anything else that asks after the carrier: refuse and keep going
        sess.answer(res["request_id"], {"behavior": "deny", "message": "not part of this probe"})
        res = sess.wait(lambda e: L.is_result(e) or L.is_can_use_tool(e), timeout=180, start=res["_i"] + 1)
    evs = sess.since(mark)
    uses = L.tool_uses(evs)
    out["after_carrier"] = {
        "tool_result_seen": [x for x in L.tool_results(evs) if x["tool_use_id"] == r.get("tool_use_id")],
        "later_tool_calls": [{"name": u["name"], "input": u["input"]} for u in uses],
        "ran_ls": any(u["name"] == "Bash" and "ls" in json.dumps(u["input"]) for u in uses),
        "re_asked": any(u["name"] == "AskUserQuestion" for u in uses),
        "later_control_requests": [e["request"].get("tool_name") for e in later_requests],
        "permission_denied_events": [
            {k: e.get(k) for k in ("tool_name", "tool_use_id", "decision_reason_type", "message")}
            for e in evs if e.get("type") == "system" and e.get("subtype") == "permission_denied"
        ],
    }
    if res is not None:
        out["result"] = {
            "subtype": res.get("subtype"),
            "is_error": res.get("is_error"),
            "stop_reason": res.get("stop_reason"),
            "terminal_reason": res.get("terminal_reason"),
            "num_turns": res.get("num_turns"),
            "permission_denials": res.get("permission_denials"),
            "text": res.get("result"),
            "usage": L.usage(res),
        }
    # the §2a reply item, delivered at idle as its own turn
    mark = sess.mark()
    sess.user(f"Answer to {qid}: blue")
    res2 = sess.wait(L.is_result, timeout=120, start=mark)
    out["reply_turn"] = None if res2 is None else {"text": res2.get("result"), "uses_blue": "blue" in str(res2.get("result")).lower(), "usage": L.usage(res2)}
    out["exit"] = sess.close()
    return out


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 3
    results = []
    for i in range(n):
        for carrier in ("deny", "allow"):
            results.append(run(carrier, i + 1))
    L.emit(results)


if __name__ == "__main__":
    main()
