#!/usr/bin/env python3
"""Read-only survey of agent-toolchain usage on one host (issue #987).

Walks the local Claude Code transcripts (every profile), Codex and OpenCode
session logs, flk server logs and state, and goal-s1 / watcher-s1 state for a
date window, and prints ONE aggregated, redacted JSON document on stdout.
Raw transcript text never leaves the host: only counters, short redacted
examples (<= 200 chars) and owner quotes that mention the toolchain.

Usage (stdlib only, run isolated):

    python3 -I toolchain_survey.py [--days 21] [--claude-root GLOB]...
        [--scripts-glob GLOB]... [--extra-word WORD]...

Claude Code roots default to ~/.claude/projects and $CLAUDE_CONFIG_DIR/projects;
add more (for example per-profile config dirs) with --claude-root or
SURVEY_CLAUDE_ROOTS (os.pathsep-separated globs). Orchestration script dirs
are only counted when given with --scripts-glob or SURVEY_SCRIPT_GLOBS.
--extra-word / SURVEY_EXTRA_WORDS add site-specific names (agent nicknames,
model aliases) to the words that mark an owner message as toolchain-related.

It opens nothing for writing: SQLite databases are opened with mode=ro, and
the only output is stdout.
"""

import argparse
import collections
import datetime as dt
import glob
import json
import os
import re
import socket
import sqlite3
import sys
import time

HOME = os.path.expanduser("~")
MAX_EXAMPLES = 3
MAX_QUOTES = 150
OWNER_MAX_CHARS = 700  # longer typed prompts are pasted briefs or handoffs
QUOTE_CHARS = 200

# ---------------------------------------------------------------- redaction

_REDACTIONS = [
    (re.compile(r"(?i)\b(sk|pk|rk)-[a-z0-9_-]{16,}"), "<key>"),
    (re.compile(r"\b(ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}"), "<gh-token>"),
    (re.compile(r"\bgithub_pat_[A-Za-z0-9_]{20,}"), "<gh-token>"),
    (re.compile(r"\bxox[abprs]-[A-Za-z0-9-]{10,}"), "<slack-token>"),
    (re.compile(r"\bAKIA[0-9A-Z]{16}\b"), "<aws-key>"),
    (re.compile(r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{5,}"), "<jwt>"),
    (re.compile(r"(?i)\b(bearer|token|password|passwd|secret|api[_-]?key)(\s*[:=]\s*|\s+)\S+"), r"\1=<redacted>"),
    (re.compile(r"(?i)\b([a-z][a-z0-9+.-]*://)[^/\s:@]+:[^/\s@]+@"), r"\1<creds>@"),
    (re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}"), "<email>"),
    (re.compile(r"\b(tskey|tsauth)-[A-Za-z0-9-]{8,}"), "<ts-key>"),
    (re.compile(r"\b[A-Za-z0-9+/]{40,}={0,2}"), "<blob>"),
    (re.compile(r"\b[0-9a-f]{32,}\b"), "<hex>"),
    (re.compile(r"/(?:Users|home)/[^/\s\"']+"), "~"),
]


def redact(text, limit=QUOTE_CHARS):
    if not isinstance(text, str):
        text = json.dumps(text, ensure_ascii=False)
    text = re.sub(r"\s+", " ", text).strip()
    for pat, rep in _REDACTIONS:
        text = pat.sub(rep, text)
    if len(text) > limit:
        text = text[: limit - 1] + "…"
    return text


# ---------------------------------------------------------------- classifiers

FLK_GROUPS = {
    "agent", "delegate", "pane", "msg", "worktree", "workspace", "tab", "wait",
    "session", "server", "integration", "notification", "config", "channel",
    "report",
}
FLK_LEAVES = {"status", "update", "web"}
FLK_RE = re.compile(
    r"(?<![\w.-])(?:flk|flock)((?:\s+--?[a-z][a-z-]*(?:[ =](?!-)[^\s;|&]+)?)*)\s+([a-z][a-z-]*)(?:\s+([a-z][a-z-]*))?"
)
GOAL_RE = re.compile(
    r"(?<![\w.-])goal-s1\s+(arm|status|stop|pause|resume|park|unpark|build|export|import|judge|compact-driver|park-poller|hook-[a-z-]+)\b"
)
WATCHER_RE = re.compile(r"(?<![\w.-])watcher-s1\s+(judge|config|guide|follow|mcp|-)")
# Segments that only mention a command (searches, commits, docs written via
# heredoc) are not invocations; drop them before matching.
HEREDOC_RE = re.compile(r"<<-?\s*['\"]?(\w+)['\"]?.*?\n.*?\n\1\b", re.S)
NON_INVOKING = re.compile(
    r"^\s*(?:\w+=\S+\s+)*(?:rg|grep|egrep|git|sed|awk|echo|printf|cat|gh|jq|perl|head|tail|find|less|wc|"
    r"python3?|cargo|just|nix|rustfmt|diff|sort|uniq|tee|cp|mv|ls|xargs)\b"
)

KNOWN_CODES = [
    "input_not_empty", "not_settled", "unconfirmed", "abandoned", "handoff_failed",
    "session_mismatch", "pane_not_found", "agent_not_found", "mesh_delivery_refused",
    "invalid_request", "timeout", "timed_out", "not_found", "ambiguous", "busy",
    "already_exists", "server_restarted", "unreachable", "permission_denied",
    "connection refused", "no such file", "not running", "stale", "rate_limit",
    "usage limit", "worktree path must be absolute", "dirty", "conflict",
]
CODE_JSON_RE = re.compile(r'\\?"(?:code|error_code|status|outcome|state)\\?"\s*:\s*\\?"([a-z][a-z0-9_]{2,40})\\?"')
EXIT_RE = re.compile(r"^Exit code (\d+)")
TOOL_USE_ERROR_RE = re.compile(r"<tool_use_error>(.{0,160})", re.S)
JSON_PARSE_FAIL_RE = re.compile(r"JSONDecodeError|jq: error|parse error: Invalid")
CODEX_EXIT_RE = re.compile(r"(?:exit_code\\?\"?\s*[:=]\s*|exited with code |Exit code:? )(-?\d+)")

WORKAROUND_PATTERNS = {
    "tmux_send_keys": re.compile(r"\btmux\s+send-keys"),
    "sleep_poll_loop": re.compile(r"\b(until|while)\b[^\n]*;\s*do\b[^\n]*\bsleep\b|\bsleep\s+\d+\s*;\s*(flk|flock|gh|cat|tail)\b"),
    "bare_sleep": re.compile(r"(?:^|[;&|]\s*)sleep\s+\d{2,}"),
    "gh_watch": re.compile(r"\bgh\s+(pr\s+checks\s+[^\n]*--watch|run\s+watch)"),
    "sqlite_on_flock_state": re.compile(r"sqlite3[^\n]*(flock|mesh-mail|goal)"),
    "kill_or_pkill": re.compile(r"\b(pkill|killall)\b|\bkill\s+-(9|KILL|TERM|15)\b"),
    "ssh_remote_flk": re.compile(r"\bssh\b[^\n]*\b(flk|flock)\s+[a-z]"),
    "pane_read_scrape": re.compile(r"\b(flk|flock)\s+pane\s+read\b"),
    "osascript_or_keystroke": re.compile(r"\bosascript\b"),
    "flk_json_into_python_or_jq": re.compile(r"\b(flk|flock)\s+[a-z][^|\n]*\|\s*(python3?|jq)\b"),
    "send_keys_enter": re.compile(r"\b(flk|flock)\s+pane\s+send-keys\b[^\n;|&]*\b(Enter|C-m|Return)\b"),
    "agent_send_with_submit": re.compile(r"\b(flk|flock)\s+agent\s+send\s+(?:[^\n]*\s)?--submit\b"),
    "agent_send_then_send_keys": re.compile(r"\b(flk|flock)\s+agent\s+send\b[\s\S]*\bpane\s+send-keys\b"),
    "git_stash": re.compile(r"\bgit\s+stash\b"),
    "hand_rolled_fleet_script": re.compile(r"\.(fleet|architect)/[\w.-]+\.(sh|py)\b"),
    "claude_p_subprocess": re.compile(r"\bclaude\s+(-p|--print)\b"),
    "codex_exec_subprocess": re.compile(r"\bcodex\s+exec\b"),
}

# Patterns that must see git invocations or pipes, which invoking_text() drops.
WORKAROUNDS_ON_FULL_COMMAND = {"git_stash", "flk_json_into_python_or_jq"}

INJECTED_PREFIXES = re.compile(
    r"^\s*(read (and do )?/|read /|goal_s1 nudge|\[[a-z0-9 _-]+\]|<pasted_content|you are (picking up|fixing|resuming|working|an?)\b|"
    r"pr #\d+ (re-)?review|<command-|<local-command|<task-notification|<bash-|caveat:|this session is being continued|"
    r"\[request interrupted|stop hook feedback|<system-reminder>|# task|base directory for this skill|you are |/|#\d+ landed|new task)",
    re.I,
)
TOOLCHAIN_WORDS = re.compile(
    r"\b(flk|flock|goal|goal_s1|goal-s1|watcher|watcher-s1|watch-job|s1|kev|clef|delegates?|agents?|panes?|compact|hooks?|"
    r"codex|claude|architect|inbox|msg|subagents?|reviewers?|usage limit|stall(ed|s)?|stuck|nudges?|mesh|handoff|"
    r"opencode|sonnet|opus|haiku|mcp|reap|idle)\b",
    re.I,
)
EXTRA_WORDS_RE = None  # set from --extra-word / SURVEY_EXTRA_WORDS in main()
OWNER_CATEGORIES = {
    "correction": re.compile(r"(^|\b)(no[,.!]? |nope|don'?t|do not|stop\b|wrong|not what|instead|i said|i told|why did|why do you|why are you|you should(n'?t)? have|again\?|revert|undo)", re.I),
    "complaint": re.compile(r"\b(annoying|broken|still (not|doesn|isn|fails|stuck)|doesn'?t work|not working|stuck|hangs?|hanging|slow|wtf|ugh|frustrat|keeps? (on )?|every time|useless|never works|again and again|why is)", re.I),
    "feature_ask": re.compile(r"\b(can we|could we|can you make|should (be able|have|just)|i want|we need|would be (nice|good|great)|feature|it would|make it|add (a|an|support)|automatic(ally)?|why (can'?t|doesn'?t) (it|flk|flock))", re.I),
}


def invoking_text(cmd):
    cmd = HEREDOC_RE.sub(" ", cmd)
    segs = re.split(r"\n|;|&&|\|\||\|", cmd)
    return "\n".join(seg for seg in segs if not NON_INVOKING.match(seg))


def flk_keys(cmd):
    cmd = invoking_text(cmd)
    keys = []
    for m in FLK_RE.finditer(cmd):
        top, sub = m.group(2), m.group(3)
        if top in FLK_GROUPS:
            keys.append(f"flk {top} {sub}" if sub else f"flk {top}")
        elif top in FLK_LEAVES:
            keys.append(f"flk {top}")
    keys += [f"goal-s1 {m.group(1)}" for m in GOAL_RE.finditer(cmd)]
    keys += [f"watcher-s1 {'run' if m.group(1) == '-' else m.group(1)}" for m in WATCHER_RE.finditer(cmd)]
    return list(dict.fromkeys(keys))


def error_codes(text):
    found = collections.Counter()
    low = text.lower()
    for code in KNOWN_CODES:
        if code in low:
            found[code] += 1
    for m in CODE_JSON_RE.finditer(text):
        v = m.group(1)
        if v not in ("ok", "success", "done", "idle", "working", "accepted", "delivered", "settled", "completed", "running",
                     "fulfilled", "passed", "rejected", "open", "closed", "merged", "success", "pending"):
            found[v] += 1
    return found


def text_of(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        out = []
        for b in content:
            if isinstance(b, dict):
                if b.get("type") == "text":
                    out.append(b.get("text", ""))
                elif b.get("type") == "tool_result":
                    out.append(text_of(b.get("content")))
                elif "text" in b:
                    out.append(str(b["text"]))
            elif isinstance(b, str):
                out.append(b)
        return "\n".join(out)
    return ""


def parse_ts(s):
    if not s:
        return None
    try:
        return dt.datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()
    except ValueError:
        return None


# ---------------------------------------------------------------- aggregate


def new_tool():
    return {
        "calls": 0, "errors": 0, "exit_codes": collections.Counter(),
        "error_codes": collections.Counter(), "by_harness": collections.Counter(),
        "wall_s": 0.0, "max_wall_s": 0.0, "retries_after_error": 0,
        "json_parse_failures": 0, "blocked_by_harness": 0,
        "examples": [],
    }


class Survey:
    def __init__(self, days, claude_roots=(), script_globs=()):
        self.days = days
        self.claude_roots = list(claude_roots)
        self.script_globs = list(script_globs)
        self.cutoff = time.time() - days * 86400
        self.tools = collections.defaultdict(new_tool)  # flk/goal-s1/watcher-s1 keys
        self.mcp = collections.defaultdict(new_tool)  # mcp__* tool names
        self.builtin = collections.Counter()  # Bash/Read/Agent/... call counts
        self.skills = collections.Counter()
        self.subagents = collections.Counter()
        self.hooks = collections.defaultdict(lambda: {"count": 0, "examples": []})
        self.stop_hook_summaries = collections.Counter()
        self.workarounds = collections.Counter()
        self.workaround_examples = collections.defaultdict(list)
        self.sleep_seconds = 0
        self.api_errors = collections.Counter()
        self.limit_stalls = []  # minutes
        self.limit_sessions = set()
        self.owner = {"messages": 0, "toolchain_messages": 0, "categories": collections.Counter(), "quotes": []}
        self.injected = collections.Counter()
        self.compactions = collections.Counter()
        self.turns = {"count": 0, "total_s": 0.0}
        self.coverage = collections.defaultdict(lambda: collections.Counter())
        self.fallback_seq = collections.Counter()
        self.harness_blocks = collections.Counter()

    # -- helpers
    def add_example(self, lst, item, cap=MAX_EXAMPLES):
        if len(lst) < cap and item not in lst:
            lst.append(item)

    def record_cmd(self, harness, cmd, out, is_error, exit_code, wall, prev):
        keys = flk_keys(cmd)
        body = HEREDOC_RE.sub(" ", cmd)
        live = invoking_text(cmd)
        for name, pat in WORKAROUND_PATTERNS.items():
            if pat.search(body if name in WORKAROUNDS_ON_FULL_COMMAND else live):
                self.workarounds[name] += 1
                self.add_example(self.workaround_examples[name], redact(cmd, 160), 2)
        for m in re.finditer(r"(?:^|[;&|]\s*)sleep\s+(\d+)", cmd):
            self.sleep_seconds += int(m.group(1))
        failed = bool(is_error) or (exit_code not in (None, 0))
        blocked = TOOL_USE_ERROR_RE.search(out or "")
        if blocked:
            self.harness_blocks[normalize_block(blocked.group(1))] += 1
        codes = error_codes(out) if keys else collections.Counter()
        for k in keys:
            t = self.tools[k]
            t["calls"] += 1
            t["by_harness"][harness] += 1
            if wall is not None and 0 <= wall < 6 * 3600:
                t["wall_s"] += wall
                t["max_wall_s"] = max(t["max_wall_s"], wall)
            if exit_code is not None:
                t["exit_codes"][str(exit_code)] += 1
            if failed:
                t["errors"] += 1
                t["error_codes"].update(codes)
                self.add_example(t["examples"], {"cmd": redact(cmd, 160), "out": redact(out[-400:], 200)})
            if JSON_PARSE_FAIL_RE.search(out or ""):
                t["json_parse_failures"] += 1
            if blocked:
                t["blocked_by_harness"] += 1
            if prev and prev["failed"] and k in prev["keys"]:
                t["retries_after_error"] += 1
        # fallback: agent/delegate send failed, next call is a pane send-text/run/send-keys
        if prev and prev["failed"] and any(k.startswith(("flk agent send", "flk delegate send", "flk msg")) for k in prev["keys"]):
            for k in keys:
                if k.startswith("flk pane"):
                    self.fallback_seq[f"{prev['keys'][0]} -> {k}"] += 1
        return {"keys": keys, "failed": failed}

    # -- Claude Code
    def claude(self):
        roots = [os.path.join(HOME, ".claude", "projects")]
        for pattern in self.claude_roots:
            roots += glob.glob(os.path.expanduser(pattern))
        if os.environ.get("CLAUDE_CONFIG_DIR"):
            roots.append(os.path.join(os.environ["CLAUDE_CONFIG_DIR"], "projects"))
        seen_roots, seen_files = set(), set()
        for root in roots:
            real = os.path.realpath(root)
            if not os.path.isdir(real) or real in seen_roots:
                continue
            seen_roots.add(real)
            self.coverage["claude"]["roots"] += 1
            for dirpath, _dirs, files in os.walk(real, followlinks=True):
                for fn in files:
                    if not fn.endswith(".jsonl"):
                        continue
                    p = os.path.realpath(os.path.join(dirpath, fn))
                    if p in seen_files:
                        continue
                    seen_files.add(p)
                    try:
                        if os.path.getmtime(p) < self.cutoff:
                            continue
                    except OSError:
                        continue
                    self.coverage["claude"]["files"] += 1
                    if "/subagents/" in p:
                        self.coverage["claude"]["subagent_files"] += 1
                    self.coverage["claude"]["bytes"] += os.path.getsize(p)
                    self.claude_file(p)

    def claude_file(self, path):
        pending = {}  # tool_use id -> (name, input, ts)
        prev = None
        limit_open = None
        for line in open(path, encoding="utf-8", errors="replace"):
            try:
                d = json.loads(line)
            except ValueError:
                continue
            ts = parse_ts(d.get("timestamp"))
            if ts is not None and ts < self.cutoff:
                continue
            typ = d.get("type")
            if typ == "assistant":
                msg = d.get("message") or {}
                if d.get("isApiErrorMessage"):
                    err = d.get("error") or "unknown"
                    self.api_errors[err] += 1
                    if err == "rate_limit" or "limit" in text_of(msg.get("content")).lower():
                        self.limit_sessions.add(d.get("sessionId"))
                        if limit_open is None:
                            limit_open = ts
                    continue
                if limit_open is not None and ts is not None:
                    # past 12 h the session was abandoned and resumed, not stalled
                    gap = (ts - limit_open) / 60
                    self.limit_stalls.append(round(gap, 1) if gap <= 720 else 720)
                    limit_open = None
                for b in msg.get("content") or []:
                    if isinstance(b, dict) and b.get("type") == "tool_use":
                        pending[b.get("id")] = (b.get("name"), b.get("input") or {}, ts)
                        self.claude_tool_use(b.get("name"), b.get("input") or {})
            elif typ == "user":
                msg = d.get("message") or {}
                content = msg.get("content")
                if isinstance(content, list) and any(isinstance(b, dict) and b.get("type") == "tool_result" for b in content):
                    for b in content:
                        if not (isinstance(b, dict) and b.get("type") == "tool_result"):
                            continue
                        name, inp, ts0 = pending.pop(b.get("tool_use_id"), (None, {}, None))
                        out = text_of(b.get("content"))
                        wall = (ts - ts0) if (ts and ts0) else None
                        if name == "Bash":
                            m = EXIT_RE.match(out or "")
                            code = int(m.group(1)) if m else (0 if not b.get("is_error") else None)
                            prev = self.record_cmd("claude", inp.get("command", ""), out, b.get("is_error"), code, wall, prev)
                        elif name and name.startswith("mcp__"):
                            self.record_mcp("claude", name, inp, out, b.get("is_error"), wall)
                else:
                    self.claude_user_text(d, text_of(content))
            elif typ == "attachment":
                self.claude_attachment(d.get("attachment") or {})
            elif typ == "system":
                st = d.get("subtype")
                if st == "stop_hook_summary":
                    self.stop_hook_summaries["summaries"] += 1
                    if d.get("hookErrors"):
                        self.stop_hook_summaries["with_blocking_feedback"] += 1
                    if d.get("preventedContinuation"):
                        self.stop_hook_summaries["prevented_continuation"] += 1
                elif st == "compact_boundary":
                    trig = (d.get("compactMetadata") or {}).get("trigger") or "unknown"
                    self.compactions[trig] += 1
                elif st == "turn_duration":
                    self.turns["count"] += 1
                    self.turns["total_s"] += (d.get("durationMs") or 0) / 1000
                elif st == "informational":
                    c = text_of(d.get("content"))
                    if "limit" in c.lower():
                        self.api_errors["informational_limit_notice"] += 1

    def claude_tool_use(self, name, inp):
        self.builtin[name if not name.startswith("mcp__") else "mcp"] += 1
        if name == "Skill":
            self.skills[str(inp.get("skill"))] += 1
        elif name in ("Agent", "Task"):
            self.subagents[f"{inp.get('subagent_type') or 'default'}/{inp.get('model') or 'inherit'}"] += 1

    def record_mcp(self, harness, name, inp, out, is_error, wall):
        t = self.mcp[name]
        t["calls"] += 1
        t["by_harness"][harness] += 1
        if wall is not None and 0 <= wall < 6 * 3600:
            t["wall_s"] += wall
            t["max_wall_s"] = max(t["max_wall_s"], wall)
        codes = error_codes(out or "")
        looks_err = bool(is_error) or bool(re.search(r'"ok"\s*:\s*false|"error"\s*:', out or ""))
        if looks_err:
            t["errors"] += 1
            t["error_codes"].update(codes)
            self.add_example(t["examples"], {"in": redact(inp, 120), "out": redact((out or "")[-400:], 200)})

    def claude_user_text(self, d, text):
        if d.get("isSidechain"):
            self.injected["subagent_prompt"] += 1
            return
        if not text or d.get("isMeta") or d.get("isCompactSummary"):
            if d.get("isCompactSummary"):
                self.compactions["summary_injected"] += 1
            return
        origin = (d.get("origin") or {}).get("kind")
        source = d.get("promptSource")
        if origin and origin != "human":
            self.injected[f"origin:{origin}"] += 1
            return
        if source and source not in ("typed", "queued", "suggestion_accepted"):
            self.injected[f"source:{source}"] += 1
            return
        low = text.strip()
        if INJECTED_PREFIXES.match(low) or len(low) > OWNER_MAX_CHARS or re.search(r"(?m)^(worktree:|## |context - )", low, re.I):
            key = re.sub(r"[^a-z_\[\] ]", "", low.lower())[:24].strip()
            self.injected[f"typed:{key}"] += 1
            if low.lower().startswith(("goal_s1 nudge", "[goal]")):
                self.hooks["goal-s1/injected-nudge"]["count"] += 1
            return
        self.owner["messages"] += 1
        if not (TOOLCHAIN_WORDS.search(low) or (EXTRA_WORDS_RE and EXTRA_WORDS_RE.search(low))):
            return
        self.owner["toolchain_messages"] += 1
        cats = [c for c, pat in OWNER_CATEGORIES.items() if pat.search(low)]
        for c in cats:
            self.owner["categories"][c] += 1
        if cats and len(self.owner["quotes"]) < MAX_QUOTES and len(low) >= 12:
            day = (d.get("timestamp") or "")[:10]
            self.owner["quotes"].append({"cats": cats, "day": day, "harness": "claude", "text": redact(low)})

    def claude_attachment(self, a):
        at = a.get("type")
        if at in ("hook_blocking_error", "hook_additional_context", "hook_system_message", "hook_cancelled"):
            be = a.get("blockingError") or {}
            cmd = be.get("command") or a.get("command") or a.get("hookName") or ""
            text = be.get("blockingError") or a.get("content") or a.get("message") or ""
            if isinstance(text, list):
                text = " ".join(map(str, text))
            src = "goal-s1" if "goal-s1" in cmd else "watch-nudge" if "watch-nudge" in cmd else \
                "flock-agent-state" if "flock-agent-state" in cmd else "watcher-s1" if "watcher" in cmd else \
                (os.path.basename(cmd.split()[0]) if cmd.split() else "unknown")
            kind = classify_hook_text(str(text))
            key = f"{src}/{a.get('hookName') or a.get('hookEvent') or at}/{kind}"
            h = self.hooks[key]
            h["count"] += 1
            self.add_example(h["examples"], redact(str(text), 160), 2)
        elif at == "goal_status":
            self.hooks["claude-goal_status"]["count"] += 1
        elif at == "queued_command":
            self.injected["queued_command"] += 1

    # -- Codex
    def codex(self):
        base = os.path.join(os.environ.get("CODEX_HOME", os.path.join(HOME, ".codex")), "sessions")
        for p in glob.glob(os.path.join(base, "**", "*.jsonl"), recursive=True):
            try:
                if os.path.getmtime(p) < self.cutoff:
                    continue
            except OSError:
                continue
            self.coverage["codex"]["files"] += 1
            self.coverage["codex"]["bytes"] += os.path.getsize(p)
            self.codex_file(p)

    def codex_file(self, path):
        pending = {}
        prev = None
        for line in open(path, encoding="utf-8", errors="replace"):
            try:
                d = json.loads(line)
            except ValueError:
                continue
            p = d.get("payload") or {}
            pt = p.get("type")
            ts = parse_ts(d.get("timestamp"))
            if pt in ("function_call", "custom_tool_call"):
                raw = p.get("arguments") or p.get("input") or ""
                pending[p.get("call_id")] = (p.get("name"), raw, ts)
                self.builtin[f"codex:{p.get('name')}"] += 1
            elif pt in ("function_call_output", "custom_tool_call_output"):
                name, raw, ts0 = pending.pop(p.get("call_id"), (None, "", None))
                out = p.get("output")
                out = text_of(out) if not isinstance(out, str) else out
                if name and name.startswith("mcp__") or (name and "flock_" in name):
                    self.record_mcp("codex", name, raw, out, None, (ts - ts0) if ts and ts0 else None)
                    continue
                cmd = codex_cmd(raw)
                if not cmd:
                    continue
                codes = [int(x) for x in CODEX_EXIT_RE.findall(out or "")]
                code = next((c for c in codes if c != 0), codes[0] if codes else None)
                prev = self.record_cmd("codex", cmd, out or "", None, code, (ts - ts0) if ts and ts0 else None, prev)
            elif pt == "mcp_tool_call_end" or d.get("type") == "event_msg" and pt == "mcp_tool_call_end":
                inv = p.get("invocation") or {}
                name = f"mcp__{inv.get('server')}__{inv.get('tool')}"
                res = p.get("result") or {}
                self.record_mcp("codex", name, inv.get("arguments"), json.dumps(res)[:2000], "Err" in res, None)
            elif pt == "message" and p.get("role") == "user":
                text = text_of(p.get("content"))
                if text.startswith(("<environment_context", "<user_instructions", "# AGENTS.md", "<permissions")):
                    continue
                # Codex records no prompt origin, and its user turns are often
                # prompts typed by another agent, so count them, never quote.
                self.injected["codex_user_turn"] += 1
            elif pt == "error" or (pt and "rate_limit" in pt):
                msg = str(p.get("message") or "")
                self.api_errors["codex:" + ("usage_limit" if "limit" in msg.lower() else "error")] += 1

    # -- OpenCode
    def opencode(self):
        db = os.path.join(HOME, ".local", "share", "opencode", "opencode.db")
        if not os.path.exists(db):
            return
        try:
            con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
            cut = int(self.cutoff * 1000)
            self.coverage["opencode"]["sessions"] = con.execute("select count(*) from session where time_created>=?", (cut,)).fetchone()[0]
            prev = None
            for (raw, created, updated) in con.execute("select data,time_created,time_updated from part where time_created>=? order by session_id,time_created", (cut,)):
                try:
                    d = json.loads(raw)
                except ValueError:
                    continue
                self.coverage["opencode"]["parts"] += 1
                if d.get("type") != "tool":
                    continue
                st = d.get("state") or {}
                tool = d.get("tool")
                wall = ((st.get("time") or {}).get("end", 0) - (st.get("time") or {}).get("start", 0)) / 1000 if st.get("time") else None
                if tool == "bash":
                    meta = st.get("metadata") or {}
                    code = meta.get("exit")
                    out = st.get("output") or st.get("error") or ""
                    prev = self.record_cmd("opencode", (st.get("input") or {}).get("command", ""), out, st.get("status") == "error", code, wall, prev)
                elif tool and ("flock" in tool or "_" in tool):
                    self.record_mcp("opencode", f"opencode:{tool}", st.get("input"), str(st.get("output") or st.get("error") or ""), st.get("status") == "error", wall)
                self.builtin[f"opencode:{tool}"] += 1
            for (raw,) in con.execute("select data from message where time_created>=?", (cut,)):
                try:
                    d = json.loads(raw)
                except ValueError:
                    continue
                err = d.get("error")
                if err:
                    name = err.get("name") if isinstance(err, dict) else str(err)
                    self.api_errors[f"opencode:{name}"] += 1
        except sqlite3.Error as e:
            self.coverage["opencode"]["error:" + type(e).__name__] += 1

    # -- flk + s1 state
    def state(self):
        st = os.path.join(HOME, ".local", "state")
        out = {}
        # goal-s1 judge log
        dec = os.path.join(st, "goal-s1", "decisions.jsonl")
        g = {"decisions": collections.Counter(), "kev_err": collections.Counter(), "judge": collections.Counter(),
             "kev_ms": [], "sessions": set(), "bg_waited_min": [], "park_waited_min": []}
        if os.path.exists(dec):
            for line in open(dec, encoding="utf-8", errors="replace"):
                try:
                    d = json.loads(line)
                except ValueError:
                    continue
                if (d.get("ts") or 0) < self.cutoff:
                    continue
                g["decisions"][f"{d.get('event')}|{d.get('action')}|{d.get('why')}"] += 1
                if d.get("kev_err"):
                    g["kev_err"][redact(str(d["kev_err"]), 60)] += 1
                if d.get("judge"):
                    g["judge"][d["judge"]] += 1
                if d.get("kev_ms") is not None:
                    g["kev_ms"].append(d["kev_ms"])
                if d.get("session"):
                    g["sessions"].add(d["session"])
                if d.get("bg_waited_min") is not None:
                    g["bg_waited_min"].append(d["bg_waited_min"])
                if d.get("waited_min") is not None:
                    g["park_waited_min"].append(d["waited_min"])
        g["sessions"] = len(g["sessions"])
        g["kev_ms"] = summarize(g["kev_ms"])
        g["bg_waited_min"] = summarize(g["bg_waited_min"])
        g["park_waited_min"] = summarize(g["park_waited_min"])
        drv = os.path.join(st, "goal-s1", "drivers.log")
        if os.path.exists(drv):
            errs = collections.Counter()
            for line in open(drv, encoding="utf-8", errors="replace"):
                if re.search(r"Error|error|Traceback|failed", line):
                    errs[redact(line, 90)] += 1
            g["driver_log_errors"] = dict(errs.most_common(8))
        for name in ("breaker.json",):
            p = os.path.join(st, "goal-s1", name)
            if os.path.exists(p):
                g["breaker"] = redact(open(p).read(), 200)
        out["goal_s1"] = g
        # legacy goal_s1 (underscore) state dir
        legacy = os.path.join(st, "goal_s1")
        if os.path.isdir(legacy):
            out["goal_s1_legacy_dir"] = {
                "recent_files": sum(1 for p in glob.glob(os.path.join(legacy, "**"), recursive=True) if os.path.isfile(p) and os.path.getmtime(p) >= self.cutoff),
                "top_level": sorted(os.listdir(legacy))[:40],
            }
        # watcher-s1
        w = {"runs": 0, "states": collections.Counter(), "cmd_heads": collections.Counter()}
        for p in glob.glob(os.path.join(st, "watcher-s1", "*.jsonl")) + glob.glob(os.path.join(st, "watch-job", "*.events")):
            if os.path.getmtime(p) < self.cutoff:
                continue
            w["runs"] += 1
            for line in open(p, encoding="utf-8", errors="replace"):
                try:
                    d = json.loads(line)
                except ValueError:
                    continue
                w["states"][f"{d.get('state')}|{d.get('reason')}"] += 1
                if d.get("cmd"):
                    w["cmd_heads"][redact(str(d["cmd"]).split()[0] if str(d["cmd"]).split() else "", 40)] += 1
        nud = os.path.join(st, "watch-nudge")
        w["watch_nudge_sessions"] = sum(1 for p in glob.glob(os.path.join(nud, "*")) if os.path.getmtime(p) >= self.cutoff)
        out["watcher_s1"] = w
        # flk state
        fs = os.path.join(st, "flock")
        f = {}
        mesh = os.path.join(fs, "mesh-mail.sqlite")
        if os.path.exists(mesh):
            try:
                con = sqlite3.connect(f"file:{mesh}?mode=ro", uri=True)
                f["envelope_states"] = dict(con.execute("select state,count(*) from envelopes group by state").fetchall())
                f["delivery_attempt_states"] = dict(con.execute("select state,count(*) from delivery_attempts group by state").fetchall())
                f["tombstone_reasons"] = dict(con.execute("select reason,count(*) from agent_tombstones group by reason").fetchall())
            except sqlite3.Error as e:
                f["mesh_error"] = type(e).__name__
        verdicts = collections.Counter()
        for p in glob.glob(os.path.join(fs, "delegates", "*", "*.verdict.json")):
            try:
                v = json.load(open(p))
                verdicts[str((v.get("observation") or {}).get("reason") or v.get("verdict") or "?")] += 1
            except (ValueError, OSError):
                pass
        f["delegate_verdict_reasons"] = dict(verdicts)
        f["delegate_records"] = len(glob.glob(os.path.join(fs, "delegates", "*", "*.json")))
        f["restart_checkpoints"] = sum(1 for p in glob.glob(os.path.join(fs, "restart-checkpoints", "*")) if os.path.getmtime(p) >= self.cutoff)
        out["flk_state"] = f
        # flk server/client logs
        logs = {}
        for p in glob.glob(os.path.join(HOME, ".config", "flock", "*.log")) + glob.glob(os.path.join(st, "flock", "*.log")):
            c = collections.Counter()
            n = 0
            for line in open(p, encoding="utf-8", errors="replace"):
                try:
                    d = json.loads(line)
                except ValueError:
                    continue
                t = parse_ts(d.get("timestamp"))
                if t is not None and t < self.cutoff:
                    continue
                n += 1
                lvl = d.get("level")
                if lvl in ("WARN", "ERROR"):
                    msg = re.sub(r"\d+", "N", str(d.get("message")))
                    c[f"{lvl} {d.get('target')}: {redact(msg, 100)}"] += 1
            logs[os.path.basename(p)] = {"lines": n, "top_warn_error": dict(c.most_common(15))}
        out["flk_logs"] = logs
        # hand-rolled orchestration scripts
        scripts = collections.Counter()
        dirs = [d for pattern in self.script_globs for d in glob.glob(os.path.expanduser(pattern))]
        for dpath in dirs:
            for p in glob.glob(os.path.join(dpath, "*.sh")) + glob.glob(os.path.join(dpath, "*.py")) + glob.glob(os.path.join(dpath, "bin", "*")):
                if os.path.isfile(p) and not p.endswith(".bak"):
                    try:
                        n = sum(1 for _ in open(p, errors="replace"))
                    except OSError:
                        n = 0
                    scripts[f"{os.path.basename(os.path.dirname(p)) if os.path.basename(os.path.dirname(p)) != 'bin' else 'bin'}/{os.path.basename(p)}"] += n
        out["orchestration_scripts_loc"] = dict(scripts.most_common(40))
        return out

    def result(self):
        def fin(d):
            res = {}
            for k, v in sorted(d.items(), key=lambda kv: -kv[1]["calls"]):
                v = dict(v)
                for f in ("exit_codes", "error_codes", "by_harness"):
                    v[f] = dict(v[f].most_common(12))
                v["wall_s"] = round(v["wall_s"])
                v["max_wall_s"] = round(v["max_wall_s"])
                res[k] = v
            return res

        stalls = sorted(self.limit_stalls)
        return {
            "host": socket.gethostname(),
            "generated_at": dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds"),
            "window_days": self.days,
            "coverage": {k: dict(v) for k, v in self.coverage.items()},
            "toolchain_cli": fin(self.tools),
            "mcp_tools": fin(self.mcp),
            "builtin_tool_calls": dict(self.builtin.most_common(40)),
            "skills": dict(self.skills.most_common(30)),
            "subagents": dict(self.subagents.most_common(20)),
            "hooks": {k: v for k, v in sorted(self.hooks.items(), key=lambda kv: -kv[1]["count"])},
            "stop_hook_summaries": dict(self.stop_hook_summaries),
            "workarounds": dict(self.workarounds.most_common()),
            "workaround_examples": dict(self.workaround_examples),
            "fallback_sequences": dict(self.fallback_seq.most_common(10)),
            "harness_blocks": dict(self.harness_blocks.most_common(15)),
            "sleep_seconds_in_shell": self.sleep_seconds,
            "api_errors": dict(self.api_errors.most_common()),
            "usage_limit": {"sessions": len(self.limit_sessions), "stall_minutes": summarize(stalls)},
            "compactions": dict(self.compactions),
            "turns": {"count": self.turns["count"], "total_h": round(self.turns["total_s"] / 3600, 1)},
            "owner": {"messages": self.owner["messages"], "toolchain_messages": self.owner["toolchain_messages"],
                      "categories": dict(self.owner["categories"]), "quotes": self.owner["quotes"]},
            "injected_prompts": dict(self.injected.most_common(25)),
        }


def normalize_block(t):
    t = t.lower()
    if t.startswith("blocked: sleep"):
        return "blocked: sleep followed by a command (use Monitor)"
    if "refusing to run it" in t or "too complex to verify" in t:
        return "refused: construct too complex to verify (worktree git guard)"
    return redact(re.sub(r"\d+", "N", t), 70)


def classify_hook_text(t):
    low = t.lower()
    for kind, pat in (
        ("recap-sentinel", r"recap:"),
        ("routine-ask", r"routine|don'?t ask|do not ask|just (do|proceed)|proceed without"),
        ("bg-checkin", r"background|check[- ]?in|still running"),
        ("park", r"\bpark"),
        ("compact/handoff", r"handoff\.md|compact"),
        ("continue", r"continue|not done|keep (going|working)|goal"),
        ("watch-nudge", r"watch-job|watcher|watch "),
        ("review", r"review"),
    ):
        if re.search(pat, low):
            return kind
    return "other"


def codex_cmd(raw):
    if isinstance(raw, dict):
        c = raw.get("cmd") or raw.get("command")
        return " ".join(c) if isinstance(c, list) else str(c or "")
    raw = str(raw)
    try:
        j = json.loads(raw)
        if isinstance(j, dict):
            return codex_cmd(j)
    except ValueError:
        pass
    cmds = re.findall(r'cmd\s*:\s*"((?:[^"\\]|\\.)*)"', raw)
    if cmds:
        out = []
        for c in cmds:
            try:
                out.append(json.loads(f'"{c}"'))
            except ValueError:
                out.append(c)
        return "\n".join(out)
    return raw if re.search(r"\b(flk|flock|goal-s1|watcher-s1|sleep|tmux|gh)\b", raw) else ""


def summarize(xs):
    xs = sorted(x for x in xs if isinstance(x, (int, float)))
    if not xs:
        return {"n": 0}
    return {"n": len(xs), "sum": round(sum(xs), 1), "p50": xs[len(xs) // 2], "p90": xs[int(len(xs) * 0.9)], "max": xs[-1]}


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--days", type=int, default=21)
    ap.add_argument("--claude-root", action="append", default=[], help="extra Claude Code projects dir (glob)")
    ap.add_argument("--scripts-glob", action="append", default=[], help="orchestration script dir to count (glob)")
    ap.add_argument("--extra-word", action="append", default=[], help="extra toolchain word for owner messages")
    args = ap.parse_args()

    def env_list(name):
        return [x for x in os.environ.get(name, "").split(os.pathsep) if x]

    global EXTRA_WORDS_RE
    words = args.extra_word + env_list("SURVEY_EXTRA_WORDS")
    if words:
        EXTRA_WORDS_RE = re.compile(r"\b(" + "|".join(map(re.escape, words)) + r")\b", re.I)
    s = Survey(args.days, args.claude_root + env_list("SURVEY_CLAUDE_ROOTS"), args.scripts_glob + env_list("SURVEY_SCRIPT_GLOBS"))
    for step in (s.claude, s.codex, s.opencode):
        try:
            step()
        except Exception as e:  # keep going: one broken source must not hide the others
            s.coverage[step.__name__]["error:" + type(e).__name__] += 1
    res = s.result()
    try:
        res["state"] = s.state()
    except Exception as e:
        res["state"] = {"error": type(e).__name__}
    json.dump(res, sys.stdout, indent=1, ensure_ascii=False, default=lambda o: sorted(o) if isinstance(o, set) else str(o))
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
