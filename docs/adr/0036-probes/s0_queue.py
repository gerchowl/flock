#!/usr/bin/env python3
"""S0 probe 4 (ADR-0036 §3, owner decision 4): the harness's own queue in
`-p`: take-back, edit and send-now behind `interrupt_cancel_queued_v1` /
`interrupt_send_now_v1`.

Each scenario starts a turn that runs `sleep 8` through Bash, writes a
second user message M2 (flk-chosen uuid) while the sleep runs, so the
harness reports M2 `queued`, and then sends one control request:

- `takeback`: `cancel_async_message {message_uuid: M2}`,
- `takeback_late`: the same, but only after M2 reported `started`,
- `cancel_queued`: `interrupt {cancel_queued: true}`,
- `send_now`: `interrupt {send_now: true, message_uuid: M2}`,
- `plain`: none (the control: when does a queued M2 start on its own),
- `edit`: an `edit_queued_message` request, a subtype the binary does not
  carry (there is no edit verb in 2.1.296), to record how an unknown
  control subtype is answered.

It records the control response, every `command_lifecycle` of both inputs
with times, the tool result of the sleep, and each `result`.

    S0_OAUTH_TOKEN_FILE=<token file> S0_OUT=/tmp/s0 \\
        python3 -I docs/adr/0036-probes/s0_queue.py [scenario ...]
"""

import os
import sys
import uuid

sys.dont_write_bytecode = True  # importing s0lib must not leave a __pycache__ here
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import s0lib as L  # noqa: E402

FLAGS = ["--tools", "Bash", "--allowedTools", "Bash(sleep:*)", "--permission-prompt-tool", "stdio"]
SLEEP_S = 8


def is_sleep_use(e):
    return any(u["name"] == "Bash" and "sleep" in str(u["input"]) for u in L.tool_uses([e]))


def run(scn):
    with L.Session(f"queue-{scn}", FLAGS) as sess:
        out = {"scenario": scn}
        u1 = str(uuid.uuid4())
        u2 = str(uuid.uuid4())
        sess.user(f"Run `sleep {SLEEP_S}` with Bash, then reply with exactly: DONE-1", uid=u1)
        use = sess.wait(is_sleep_use, timeout=60)
        if use is None:
            out["error"] = "no sleep tool call"
            sess.close()
            return out
        out["sleep_called_at"] = use["_t"]
        sess.wait(lambda e: False, timeout=1.0)
        sess.user("Reply with exactly: M2-ANSWERED", uid=u2)
        q = sess.wait(L.lifecycle(u2, "queued"), timeout=10)
        out["m2_written_at"] = round(use["_t"] + 1.0, 3)
        out["m2_queued_at"] = None if q is None else q["_t"]
        rid = None
        if scn == "takeback":
            rid = sess.control("cancel_async_message", message_uuid=u2)
        elif scn == "takeback_late":
            st = sess.wait(L.lifecycle(u2, "started"), timeout=60)
            out["m2_started_before_takeback"] = None if st is None else st["_t"]
            rid = sess.control("cancel_async_message", message_uuid=u2)
        elif scn == "cancel_queued":
            rid = sess.control("interrupt", cancel_queued=True)
        elif scn == "send_now":
            rid = sess.control("interrupt", send_now=True, message_uuid=u2)
        elif scn == "edit":
            rid = sess.control("edit_queued_message", message_uuid=u2,
                               message={"role": "user", "content": [{"type": "text", "text": "Reply with exactly: M2-EDITED"}]})
        if rid is not None:
            out["control_sent_at"] = sess.now()
            resp = sess.wait(L.is_control_response(rid), timeout=20)
            out["control_response"] = None if resp is None else {"t": resp["_t"], "response": resp.get("response")}
        # collect until both inputs are settled or the session goes quiet
        results = []
        pos = 0
        while True:
            e = sess.wait(L.is_result, timeout=40 if not results else 20, start=pos)
            if e is None:
                break
            results.append(e)
            pos = e["_i"] + 1
            evs = sess.since(0)
            settled = {
                x.get("command_uuid") for x in evs
                if x.get("type") == "command_lifecycle" and x.get("state") in ("completed", "cancelled", "discarded")
            }
            if {u1, u2} <= settled:
                break
        evs = sess.since(0)
        out["lifecycle"] = [
            {"t": x["_t"], "input": "M1" if x.get("command_uuid") == u1 else "M2" if x.get("command_uuid") == u2 else x.get("command_uuid"), "state": x.get("state")}
            for x in evs if x.get("type") == "command_lifecycle"
        ]
        out["sleep_result"] = [r for r in L.tool_results(evs) if r["tool_use_id"] == L.tool_uses([use])[0]["id"]]
        out["results"] = [
            {"t": r["_t"], "subtype": r.get("subtype"), "terminal_reason": r.get("terminal_reason"), "text": r.get("result"), "queued_turn_count": r.get("queued_turn_count")}
            for r in results
        ]
        out["exit"] = sess.close()
        return out


def main():
    names = sys.argv[1:] or ["plain", "takeback", "takeback_late", "cancel_queued", "send_now", "edit"]
    L.emit([run(n) for n in names])


if __name__ == "__main__":
    main()
