#!/usr/bin/env python3
"""Measurement runbook behind ADR-0028 and ADR-0030 (#1001).

Run by hand, never by `just check`. Every subcommand except `event-volume`
drives an ISOLATED flk: a copied binary and a short sandbox XDG directory, as
README.md in this directory describes. Never point it at a live server.

Setup (once per run):

    SB=/tmp/fw1   # keep it short: sun_path caps near 104 bytes
    mkdir -p $SB/bin $SB/x/flock $SB/work
    cp "$(readlink -f "$(command -v flk)")" $SB/bin/flk
    printf 'onboarding = false\\n' > $SB/x/flock/config.toml
    env -u FLOCK_ENV -u FLOCK_SOCKET_PATH -u FLOCK_PANE_ID -u FLOCK_BIN \\
        XDG_CONFIG_HOME=$SB/x XDG_STATE_HOME=$SB/s $SB/bin/flk server &
    # start a throwaway agent as pane w1:p1, accept its trust dialog, then:
    python3 -I tests/probes/agent_wake.py --sandbox $SB idle-wake 5

Subcommands:

    idle-wake N     needs-reply message to an idle agent; send -> turn start
    busy N          message sent mid-turn; when it is read, and whether the
                    agent was ever seen idle in between
    background      agent starts a blocking read of a fifo as a harness
                    background task and ends its turn; status timeline, a
                    wake while the task runs, and fifo write -> turn start
    event-volume P  rates per hour from an event-log.jsonl (read-only)
    judge-latency   System One latency for a question set (needs
                    SYSTEMONE_URL, --questions and --states)

The samples behind the ADRs are small (5 idle wakes, 3 busy runs, 1
background run, 30 sequential and 9 concurrent judge calls). Treat them as
orders of magnitude, and rerun before relying on a tail figure.
"""
import argparse
import bisect
import collections
import concurrent.futures
import json
import os
import statistics
import subprocess
import sys
import time
import urllib.request

ARGS = None


def flk(*argv):
    env = {k: v for k, v in os.environ.items() if not k.startswith("FLOCK_")}
    env["XDG_CONFIG_HOME"] = os.path.join(ARGS.sandbox, "x")
    env["XDG_STATE_HOME"] = os.path.join(ARGS.sandbox, "s")
    binary = os.path.join(ARGS.sandbox, "bin", "flk")
    out = subprocess.run([binary, *argv], capture_output=True, text=True, env=env, timeout=30)
    return out.stdout or out.stderr


def status():
    try:
        agent = json.loads(flk("agent", "get", ARGS.pane))["result"]["agent"]
        return agent["agent_status"], agent.get("settled_with_background_shells"), agent["status_age_secs"]
    except (ValueError, KeyError):
        return "err", None, 0


def wait_for(pred, timeout):
    start = time.monotonic()
    while time.monotonic() - start < timeout:
        sample = status()
        if pred(sample):
            return time.monotonic(), sample
        time.sleep(0.1)
    return None, status()


def until_idle(min_age=3, timeout=180):
    return wait_for(lambda s: s[0] in ("idle", "done") and s[2] >= min_age, timeout)


def msg_state(cid):
    try:
        return json.loads(flk("msg", "status", cid))["result"]["state"]
    except (ValueError, KeyError):
        return "?"


def send_msg(text):
    out = json.loads(flk("msg", "send", ARGS.pane, text, "--intent", "needs-reply"))
    return out["result"]["correlation_id"]


def submit(text):
    for _ in range(10):
        out = flk("agent", "send", "--submit", ARGS.pane, text)
        print("submit:", out.strip()[:300], flush=True)
        if '"outcome":"accepted"' in out or "observed_accepted" in out:
            return True
        time.sleep(2)
    return False


def timeline(secs, extra=lambda: None):
    start = time.monotonic()
    points = []
    while time.monotonic() - start < secs:
        sample = status()
        point = (sample[0], sample[1], extra())
        if not points or points[-1][1] != point:
            points.append((round(time.monotonic() - start, 1), point))
        time.sleep(0.2)
    return points


def idle_wake(runs):
    for i in range(runs):
        until_idle(20)
        start = time.monotonic()
        send_msg(f"probe {i}: do not use tools; print the single word ok")
        hit, sample = wait_for(lambda s: s[0] == "working", 60)
        took = f"{(hit - start) * 1000:.0f} ms" if hit else "no wake in 60 s"
        print(f"idle-wake {i}: send -> working {took} {sample}", flush=True)
        until_idle(3)


def busy(runs):
    for i in range(runs):
        until_idle(20)
        if not submit("Run the shell command `sleep 12` in the foreground, then print done."):
            sys.exit("submit refused")
        wait_for(lambda s: s[0] == "working", 30)
        time.sleep(3)
        cid = send_msg(f"busy probe {i}: print ok, nothing else")
        points = timeline(60, lambda: msg_state(cid))
        print(f"busy {i}: (seconds after send, (status, bg, msg state)) {points}", flush=True)
        until_idle(3)


def background():
    fifo = os.path.join(ARGS.sandbox, "fifo")
    if not os.path.exists(fifo):
        os.mkfifo(fifo)
    until_idle(20)
    prompt = (f"Start the shell command  cat {fifo}  with your Bash tool using "
              "run_in_background=true, then end your turn at once. When told it finished, print woke.")
    if not submit(prompt):
        sys.exit("submit refused")
    wait_for(lambda s: s[0] == "working", 30)
    print("A: after start (status, bg):", timeline(40), flush=True)
    cid = send_msg("bg probe: print ok, nothing else")
    print("B: wake while the task runs (status, bg, msg):", timeline(30, lambda: msg_state(cid)), flush=True)
    start = time.monotonic()
    fd = os.open(fifo, os.O_WRONLY | os.O_NONBLOCK)
    os.write(fd, b"go\n")
    os.close(fd)
    hit, sample = wait_for(lambda s: s[0] == "working", 90)
    took = f"{(hit - start) * 1000:.0f} ms" if hit else "no turn in 90 s"
    print(f"C: fifo written -> working {took} {sample}", flush=True)


def event_volume(path, days):
    rows = []
    with open(path) as log:
        for line in log:
            try:
                row = json.loads(line)
            except ValueError:
                continue
            rows.append((row["ts_ms"], row["envelope"]["event"], row["envelope"].get("data", {})))
    end = rows[-1][0]
    rows = [r for r in rows if r[0] >= end - days * 86400e3]
    hours = days * 24
    print(f"{len(rows)} events in {days} d: {len(rows) / hours:.0f}/h")
    for kind, count in collections.Counter(r[1] for r in rows).most_common(12):
        print(f"  {kind:36s} {count / hours:8.1f}/h")
    by_pane = collections.defaultdict(list)
    for ts, kind, data in rows:
        if kind == "pane_agent_status_changed":
            by_pane[data.get("pane_id")].append((ts, data.get("agent_status"), data.get("agent")))
    for window in (0, 5, 20, 60):
        kept = 0
        for seq in by_pane.values():
            for i, (ts, _, _) in enumerate(seq):
                nxt = seq[i + 1][0] if i + 1 < len(seq) else end
                kept += (nxt - ts) >= window * 1000
        print(f"  status changes surviving a {window:2d} s settle: {kept / hours:.1f}/h")
    short = collections.Counter()
    total = collections.Counter()
    for seq in by_pane.values():
        for i in range(len(seq) - 1):
            total[seq[i][2]] += 1
            short[seq[i][2]] += (seq[i + 1][0] - seq[i][0]) < 5000
    for harness in total:
        print(f"  {harness}: {100 * short[harness] / total[harness]:.0f} % of transitions last < 5 s")
    stamps = [r[0] for r in rows]
    densest = max(bisect.bisect_right(stamps, t + 1000) - i for i, t in enumerate(stamps))
    print(f"  densest second: {densest} events")


def judge_latency(questions_path, states_path, sequential):
    questions = json.load(open(questions_path))
    states = [json.loads(line)["text"] for line in open(states_path)]
    url = os.environ["SYSTEMONE_URL"]

    def call(state):
        body = json.dumps({"state": state, "questions": questions}).encode()
        request = urllib.request.Request(url, data=body, headers={"content-type": "application/json"})
        start = time.monotonic()
        with urllib.request.urlopen(request, timeout=30) as response:
            answers = json.load(response)["answers"]
        return (time.monotonic() - start) * 1000, answers

    times = sorted(call(s)[0] for s in states[:sequential])
    print(f"sequential n={len(times)} p50 {statistics.median(times):.0f} ms "
          f"p95 {times[int(0.95 * (len(times) - 1))]:.0f} ms max {times[-1]:.0f} ms")
    repeat = [call(states[0]) for _ in range(5)]
    print("same state x5 ms:", [round(r[0]) for r in repeat],
          "identical answers:", all(r[1] == repeat[0][1] for r in repeat))
    for width in (3, 6):
        batch = states[sequential:sequential + width]
        with concurrent.futures.ThreadPoolExecutor(width) as pool:
            done = sorted(round(r[0]) for r in pool.map(call, batch))
        print(f"{width} concurrent ms: {done}")


def main():
    global ARGS
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--sandbox", default=os.environ.get("FLK_PROBE_SANDBOX", ""))
    parser.add_argument("--pane", default="w1:p1")
    sub = parser.add_subparsers(dest="cmd", required=True)
    sub.add_parser("idle-wake").add_argument("runs", type=int)
    sub.add_parser("busy").add_argument("runs", type=int)
    sub.add_parser("background")
    volume = sub.add_parser("event-volume")
    volume.add_argument("path")
    volume.add_argument("--days", type=float, default=7)
    judge = sub.add_parser("judge-latency")
    judge.add_argument("--questions", required=True, help="JSON object of System One questions")
    judge.add_argument("--states", required=True, help="JSONL with a 'text' field per line")
    judge.add_argument("--sequential", type=int, default=30)
    ARGS = parser.parse_args()
    if ARGS.cmd in ("idle-wake", "busy", "background") and not ARGS.sandbox:
        sys.exit("--sandbox is required: this probe never talks to a live server")
    if ARGS.cmd == "idle-wake":
        idle_wake(ARGS.runs)
    elif ARGS.cmd == "busy":
        busy(ARGS.runs)
    elif ARGS.cmd == "background":
        background()
    elif ARGS.cmd == "event-volume":
        event_volume(ARGS.path, ARGS.days)
    else:
        judge_latency(ARGS.questions, ARGS.states, ARGS.sequential)


if __name__ == "__main__":
    main()
