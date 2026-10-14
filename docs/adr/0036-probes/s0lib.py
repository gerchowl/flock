"""Shared driver for the ADR-0036 S0 probes (#1307).

Spawns one throwaway `claude -p` stream-json session, records every stdout
line with a monotonic timestamp, and lets a scenario write user messages and
control requests and answer control requests. Stdlib only; run with
`python3 -I`.

Isolation, as Appendix A of ADR-0036 requires:

- every `FLOCK_*`, `CLAUDE_CODE_*`, `CLAUDECODE`, `CLAUDE_PID` and
  `CLAUDE_EFFORT` variable is removed from the child's environment, so the
  session cannot reach a live flk server, pane or the calling Claude session,
- `CLAUDE_CONFIG_DIR` is a fresh temporary directory per session (mode
  0700), removed by `Session.close()`, so nothing the session writes
  (session registry, inbox key, `session-env/`, `projects/`, `plans/`)
  lands in a real profile,
- the login is passed in as `CLAUDE_CODE_OAUTH_TOKEN`, the token
  `claude setup-token` prints (an empty config dir is not logged in, E6).
  It is read from the file named by `S0_OAUTH_TOKEN_FILE`, or from
  `CLAUDE_CODE_OAUTH_TOKEN` in the driver's own environment. It is never
  printed or written to a capture,
- `--setting-sources local --strict-mcp-config --no-session-persistence`
  as in Appendix A,
- the working directory is a fresh temporary directory with two small files,
  deleted on exit.

Raw captures go to `$S0_OUT` (default: a temporary directory). They hold
account rate-limit state, so they are not committed.
"""

import atexit
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import uuid

CLAUDE = os.environ.get("S0_CLAUDE", "claude")
MODEL = os.environ.get("S0_MODEL", "haiku")

BASE_FLAGS = [
    "-p",
    "--input-format", "stream-json",
    "--output-format", "stream-json",
    "--verbose",
    "--replay-user-messages",
    "--include-hook-events",
    "--setting-sources", "local",
    "--strict-mcp-config",
    "--no-session-persistence",
    "--model", MODEL,
]


def _oauth_token():
    path = os.environ.get("S0_OAUTH_TOKEN_FILE")
    if path:
        with open(os.path.expanduser(path)) as f:
            return f.read().strip()
    return os.environ.get("CLAUDE_CODE_OAUTH_TOKEN", "").strip()


# read before clean_env strips every CLAUDE_CODE_* variable
OAUTH_TOKEN = _oauth_token()


def require_login():
    if not OAUTH_TOKEN:
        raise SystemExit(
            "s0lib: no login. Run `claude setup-token` once, save the token to a "
            "0600 file and set S0_OAUTH_TOKEN_FILE to it (or export "
            "CLAUDE_CODE_OAUTH_TOKEN). The probes never use a real profile."
        )


def clean_env(config_dir, extra=None):
    env = {
        k: v
        for k, v in os.environ.items()
        if not (
            k.startswith("FLOCK_")
            or k.startswith("CLAUDE_CODE_")
            or k in ("CLAUDECODE", "CLAUDE_PID", "CLAUDE_EFFORT")
        )
    }
    env["CLAUDE_CONFIG_DIR"] = config_dir
    env["CLAUDE_CODE_OAUTH_TOKEN"] = OAUTH_TOKEN
    if extra:
        env.update(extra)
    return env


def out_dir():
    d = os.environ.get("S0_OUT") or tempfile.mkdtemp(prefix="s0-out-")
    os.makedirs(d, exist_ok=True)
    return d


# Every temporary cwd and config dir a Session makes is registered here and
# removed at interpreter exit if close() did not get to it (an exception
# outside a `with`, Ctrl-C between sessions). SIGKILL still leaks them.
_LIVE_DIRS = set()


def _mkdtemp(prefix):
    d = tempfile.mkdtemp(prefix=prefix)
    _LIVE_DIRS.add(d)
    return d


def _rmdir(d):
    shutil.rmtree(d, ignore_errors=True)
    _LIVE_DIRS.discard(d)


@atexit.register
def _remove_leftover_dirs():
    for d in list(_LIVE_DIRS):
        _rmdir(d)


class Session:
    """One throwaway session. Use it as a context manager:

        with s0lib.Session(name, flags) as sess:
            ...

    Leaving the block (normally, by an exception or by Ctrl-C) closes the
    session and removes its temporary dirs.
    """

    def __init__(self, name, flags, env_extra=None, cwd_files=None):
        require_login()
        self.name = name
        self.t0 = time.monotonic()
        self.proc = None
        self.log = None
        self._closed = False
        self._returncode = None
        self.cwd = _mkdtemp("s0-cwd-")
        self.config_dir = _mkdtemp("s0-cfg-")
        try:
            for fname, body in (cwd_files or {"a.txt": "alpha\n", "b.txt": "beta\n"}).items():
                with open(os.path.join(self.cwd, fname), "w") as f:
                    f.write(body)
            self.events = []
            self.cond = threading.Condition()
            self.out_path = os.path.join(out_dir(), f"{name}.jsonl")
            self.log = open(self.out_path, "w")
            self.argv = [CLAUDE] + BASE_FLAGS + list(flags)
            self.log.write(json.dumps({"_argv": self.argv, "_cwd": self.cwd}) + "\n")
            self.proc = subprocess.Popen(
                self.argv,
                cwd=self.cwd,
                env=clean_env(self.config_dir, env_extra),
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                bufsize=1,
            )
        except BaseException:
            if self.log is not None:
                self.log.close()
            _rmdir(self.cwd)
            _rmdir(self.config_dir)
            raise
        self.stderr = []
        threading.Thread(target=self._pump, daemon=True).start()
        threading.Thread(target=self._pump_err, daemon=True).start()

    def now(self):
        return round(time.monotonic() - self.t0, 3)

    def _pump(self):
        for line in self.proc.stdout:
            t = self.now()
            try:
                ev = json.loads(line)
            except ValueError:
                ev = {"type": "_unparsed", "raw": line}
            ev["_t"] = t
            with self.cond:
                ev["_i"] = len(self.events)
            self.log.write(json.dumps(ev) + "\n")
            self.log.flush()
            with self.cond:
                self.events.append(ev)
                self.cond.notify_all()
        with self.cond:
            self.events.append({"type": "_eof", "_t": self.now()})
            self.cond.notify_all()

    def _pump_err(self):
        for line in self.proc.stderr:
            self.stderr.append(line)

    def write(self, obj):
        obj = dict(obj)
        self.log.write(json.dumps({"_sent": obj, "_t": self.now()}) + "\n")
        self.log.flush()
        self.proc.stdin.write(json.dumps(obj) + "\n")
        self.proc.stdin.flush()

    def user(self, text, uid=None):
        uid = uid or str(uuid.uuid4())
        self.write({
            "type": "user",
            "uuid": uid,
            "session_id": "",
            "parent_tool_use_id": None,
            "message": {"role": "user", "content": [{"type": "text", "text": text}]},
        })
        return uid

    def control(self, subtype, **fields):
        rid = "s0-" + uuid.uuid4().hex[:8]
        req = {"subtype": subtype}
        req.update(fields)
        self.write({"type": "control_request", "request_id": rid, "request": req})
        return rid

    def answer(self, request_id, response):
        self.write({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": request_id, "response": response},
        })

    def wait(self, pred, timeout=120, start=0):
        """Return the first event at index >= start matching pred, or None."""
        deadline = time.monotonic() + timeout
        with self.cond:
            i = start
            while True:
                while i < len(self.events):
                    ev = self.events[i]
                    if pred(ev):
                        return ev
                    if ev.get("type") == "_eof":
                        return None
                    i += 1
                left = deadline - time.monotonic()
                if left <= 0:
                    return None
                self.cond.wait(left)

    def mark(self):
        with self.cond:
            return len(self.events)

    def since(self, start):
        with self.cond:
            return list(self.events[start:])

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
        return False

    def close(self, timeout=30):
        """Stop the session's own child and remove its dirs. Idempotent.

        Only `self.proc`, the child this Session started, is ever signalled.
        The dirs are removed in `finally`, whatever the waits do.
        """
        if self._closed:
            return self._returncode
        self._closed = True
        try:
            try:
                self.proc.stdin.close()
            except OSError:
                pass
            try:
                self.proc.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                self.proc.terminate()
                try:
                    self.proc.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    self.proc.kill()
                    try:
                        self.proc.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        pass
            self.wait(lambda e: e.get("type") == "_eof", timeout=5)
            self._returncode = self.proc.returncode
            self.log.write(json.dumps({"_exit": self._returncode, "_stderr_tail": "".join(self.stderr)[-2000:]}) + "\n")
        finally:
            try:
                self.log.close()
            finally:
                _rmdir(self.cwd)
                _rmdir(self.config_dir)
        return self._returncode


def is_type(t, subtype=None):
    return lambda e: e.get("type") == t and (subtype is None or e.get("subtype") == subtype)


def is_result(e):
    return e.get("type") == "result"


def is_can_use_tool(e):
    return e.get("type") == "control_request" and e.get("request", {}).get("subtype") == "can_use_tool"


def is_control_response(rid):
    return lambda e: e.get("type") == "control_response" and e.get("response", {}).get("request_id") == rid


def lifecycle(uid, state):
    return lambda e: e.get("type") == "command_lifecycle" and e.get("command_uuid") == uid and e.get("state") == state


def tool_uses(events):
    out = []
    for e in events:
        if e.get("type") == "assistant":
            for b in e.get("message", {}).get("content", []):
                if b.get("type") == "tool_use":
                    out.append({"t": e["_t"], "id": b["id"], "name": b["name"], "input": b.get("input"), "msg": e["message"].get("id")})
    return out


def tool_results(events):
    out = []
    for e in events:
        if e.get("type") == "user":
            c = e.get("message", {}).get("content")
            if isinstance(c, list):
                for b in c:
                    if isinstance(b, dict) and b.get("type") == "tool_result":
                        body = b.get("content")
                        if isinstance(body, list):
                            body = " ".join(x.get("text", "") for x in body if isinstance(x, dict))
                        out.append({"t": e["_t"], "tool_use_id": b["tool_use_id"], "is_error": b.get("is_error", False), "content": str(body)[:300]})
    return out


def usage(res):
    u = res.get("usage", {}) if res else {}
    return {k: u.get(k) for k in ("input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens", "output_tokens")}


def emit(summary):
    json.dump(summary, sys.stdout, indent=2, default=str)
    sys.stdout.write("\n")
