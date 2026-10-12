#!/usr/bin/env python3
"""Prompt-cache usage around resumes and spawns in Claude Code transcripts (issue #1042).

Read-only. For each compaction boundary in a transcript (``compact_boundary``),
or from the start of a transcript (a spawn), it prints the first N API calls:
``cache_read_input_tokens``, ``cache_creation_input_tokens`` and uncached
``input_tokens``, plus what the agent loaded in those turns (Read paths and
shell commands that print files). Output is JSON on stdout. Raw transcript
text is never printed beyond a 160-character command preview.

    python3 -I cache_usage.py resume <transcript.jsonl> [--turns 12]
    python3 -I cache_usage.py spawn <transcript.jsonl>... [--turns 6]
"""

import argparse
import json
import re
import sys

LOAD_CMD = re.compile(r"\b(cat|tail|head|sed -n|less|gh issue view|gh pr view|jq)\b")


def records(path):
    with open(path, encoding="utf-8") as fh:
        for line in fh:
            try:
                yield json.loads(line)
            except ValueError:
                continue


def calls_after(recs, start, limit):
    """The first ``limit`` distinct API calls at or after index ``start``."""
    out, seen = [], {}
    for rec in recs[start:]:
        if rec.get("type") == "system" and rec.get("subtype") == "compact_boundary" and out:
            break
        msg = rec.get("message") if isinstance(rec.get("message"), dict) else None
        if rec.get("type") != "assistant" or not msg:
            continue
        key = rec.get("requestId") or msg.get("id")
        if key not in seen:
            if len(out) == limit:
                break
            usage = msg.get("usage") or {}
            seen[key] = {
                "ts": rec.get("timestamp"),
                "model": msg.get("model"),
                "cache_read": usage.get("cache_read_input_tokens", 0),
                "cache_creation": usage.get("cache_creation_input_tokens", 0),
                "input": usage.get("input_tokens", 0),
                "loads": [],
            }
            out.append(seen[key])
        for part in msg.get("content") or []:
            if not isinstance(part, dict) or part.get("type") != "tool_use":
                continue
            inp = part.get("input") or {}
            if part.get("name") == "Read":
                seen[key]["loads"].append("Read " + str(inp.get("file_path")))
            elif part.get("name") == "Bash" and LOAD_CMD.search(inp.get("command", "")):
                seen[key]["loads"].append("Bash " + inp.get("command", "")[:160].replace("\n", " "))
            elif part.get("name", "").startswith("mcp__") or part.get("name") == "Skill":
                seen[key]["loads"].append(part["name"])
    return out


def summarize(calls):
    read = sum(c["cache_read"] for c in calls)
    created = sum(c["cache_creation"] for c in calls)
    uncached = sum(c["input"] for c in calls)
    total = read + created + uncached
    return {
        "calls": len(calls),
        "cache_read": read,
        "cache_creation": created,
        "input_uncached": uncached,
        "hit_ratio": round(read / total, 3) if total else None,
        "first_call": {k: calls[0][k] for k in ("cache_read", "cache_creation", "input")} if calls else None,
    }


def resume(args):
    recs = list(records(args.transcript))
    rows = []
    for i, rec in enumerate(recs):
        if rec.get("type") == "system" and rec.get("subtype") == "compact_boundary":
            meta = rec.get("compactMetadata") or {}
            calls = calls_after(recs, i + 1, args.turns)
            rows.append({
                "boundary_ts": rec.get("timestamp"),
                "trigger": meta.get("trigger"),
                "pre_tokens": meta.get("preTokens"),
                "post_tokens": meta.get("postTokens"),
                "summary": summarize(calls),
                "calls": calls if args.verbose else None,
            })
    json.dump(rows, sys.stdout, indent=1)
    print()


def spawn(args):
    rows = []
    for path in args.transcripts:
        calls = calls_after(list(records(path)), 0, args.turns)
        rows.append({"transcript": path.rsplit("/", 1)[-1], "summary": summarize(calls),
                     "calls": calls if args.verbose else None})
    json.dump(rows, sys.stdout, indent=1)
    print()


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="mode", required=True)
    r = sub.add_parser("resume")
    r.add_argument("transcript")
    r.add_argument("--turns", type=int, default=12)
    r.add_argument("-v", "--verbose", action="store_true")
    r.set_defaults(fn=resume)
    s = sub.add_parser("spawn")
    s.add_argument("transcripts", nargs="+")
    s.add_argument("--turns", type=int, default=6)
    s.add_argument("-v", "--verbose", action="store_true")
    s.set_defaults(fn=spawn)
    args = ap.parse_args()
    args.fn(args)


if __name__ == "__main__":
    main()
