#!/usr/bin/env python3
"""Throwaway prototype of `flk context compile` (issue #1042, ADR-0032).

Compiles one context artifact in five layers, most stable first, from a
hand-rolled architect state directory (the flock fleet's `.fleet/`), the repo
checkout and, optionally, `gh`. Read-only on every input.

    python3 -I compile_context.py --role architect --fleet DIR --repo DIR \
        --skill SKILL.md --out OUTDIR [--offline]
    python3 -I compile_context.py --role builder --repo DIR --issue N \
        --brief BRIEF.md --out OUTDIR [--offline]

OUTDIR receives `layer1.md` .. `layer5.md`, `compiled.md` (all five with
breakpoint markers) and `manifest.json` (bytes, a token estimate and the
sha256 of each layer and of each cumulative prefix). Layers 1-3 carry no
timestamps, counters or volatile ids, so recompiling with unchanged inputs
reproduces them byte for byte. `--offline` skips every `gh` call.
"""

import argparse
import hashlib
import json
import os
import re
import subprocess

BREAK = "\n<!-- flk:cache-breakpoint layer={n} sha={sha} -->\n"

BUILDER_RULES = """# Builder rules (flock)

You are a builder: one issue, one worktree, one PR into `dev`.

- Read `AGENTS.md` first and follow it. The repo layer below is its current text.
- Commit messages are pre-approved by the architect: a lowercase conventional
  subject, then a `refs #<issue>` body line (the task layer names the issue).
- Commit with `nix develop --command git commit` (the hooks need the devShell, #642).
- Test with targeted `cargo nextest run` filters, never bare `cargo test`.
- No "placeholder" in comments, and no comment line ending in `;` (guardrail gates).
- Never stop, restart or hand off the running flk server on this machine. Probe
  against isolated instances only (tests/probes/README.md).
- Stay under the size budget stated in the task layer. If the change will not
  fit, stop and report `BLOCKED: split proposal: ...` instead of opening an
  oversized PR.
- Before pushing: `git fetch origin && git merge origin/dev`; keep both sides'
  intent on conflict.
- Push, then `gh pr create --base dev`. The body states root cause, change and
  test evidence.
- Report in at most 15 lines: PR URL, what changed, tests with real exit codes,
  what is left, tooling friction with exact errors. The very last line is
  exactly one of `DONE: <PR url>` or `BLOCKED: <reason>`.
"""


def sha(text):
    return hashlib.sha256(text.encode()).hexdigest()[:16]


def est_tokens(text):
    # Rough: Claude tokenizers average ~3.6 chars per token on English + code.
    return round(len(text) / 3.6)


def read(path):
    with open(path, encoding="utf-8") as fh:
        return fh.read()


def gh(args, offline):
    if offline:
        return None
    try:
        out = subprocess.run(["gh", *args], capture_output=True, text=True, timeout=60, check=True)
    except (OSError, subprocess.SubprocessError):
        return None
    return out.stdout


def git(repo, *args):
    out = subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True, check=False)
    return out.stdout.strip()


def adr_index(repo):
    """The ADR table rows from docs/adr/README.md, as on the checkout."""
    rows = [ln for ln in read(os.path.join(repo, "docs/adr/README.md")).splitlines() if ln.startswith("| [")]
    return "## ADR index\n\n| ADR | Decision | Status |\n| --- | --- | --- |\n" + "\n".join(rows) + "\n"


def repo_layer(repo):
    """Layer 2: changes when dev moves. Keyed by the blob ids of its inputs, never by time."""
    agents = read(os.path.join(repo, "AGENTS.md"))
    return "# Repo: conventions, ADR index\n\n" + agents.rstrip() + "\n\n" + adr_index(repo)


def architect_layers(args):
    fleet = args.fleet
    l1 = "# Role: architect\n\n" + re.sub(r"^---\n.*?\n---\n", "", read(args.skill), flags=re.S).strip() + "\n"
    l2 = repo_layer(args.repo)

    # Layer 3: frozen goal and owner decisions, plus the wave plan as a stable, sorted table.
    goal = read(os.path.join(fleet, "GOAL.md")).strip()
    tasks = json.load(open(os.path.join(fleet, "tasks.json"), encoding="utf-8"))
    tasks = tasks["tasks"] if isinstance(tasks, dict) else tasks
    state = json.load(open(os.path.join(fleet, ".tasks-state.json"), encoding="utf-8"))
    waves = {}
    for t in tasks:
        wave = t["id"].split("-", 1)[0]
        waves.setdefault(wave, []).append(t)
    plan = ["## Wave plan (open tasks only; done tasks are counted, not listed)", ""]
    for wave in sorted(waves):
        items = waves[wave]
        done = sum(1 for t in items if state.get(t["id"]) == "done")
        open_items = [t for t in items if state.get(t["id"]) != "done"]
        plan.append(f"- **{wave}**: {done}/{len(items)} done" + (":" if open_items else ""))
        for t in sorted(open_items, key=lambda t: t["id"]):
            after = f" (after {', '.join(t['after'])})" if t.get("after") else ""
            plan.append(f"  - {t['id']} {t['title']}{after}")
    l3 = "# Goal and decisions\n\n" + goal + "\n\n" + "\n".join(plan) + "\n"

    # Layer 4: the volatile snapshot. Everything here may change between two compiles.
    agents = json.load(open(os.path.join(fleet, "agents.json"), encoding="utf-8"))
    owners = {t.get("owner"): t["id"] for t in tasks if t.get("owner") and state.get(t["id"]) != "done"}
    inflight = [a for a in agents if a["name"] in owners]
    snap = ["# Snapshot", "", f"dev: {git(args.repo, 'rev-parse', '--short', 'origin/dev')}", "", "## In flight"]
    for a in sorted(inflight, key=lambda a: a["name"]):
        snap.append(f"- {a['name']} ({a.get('harness', '?')}) #{a.get('issue')} task {owners[a['name']]} branch {a.get('branch')}")
    prs = gh(["pr", "list", "-R", "gerchowl/flock", "--state", "open", "--json",
              "number,title,headRefName,reviewDecision,mergeStateStatus,isDraft"], args.offline)
    if prs:
        snap += ["", "## Open PRs"]
        for p in sorted(json.loads(prs), key=lambda p: p["number"]):
            snap.append(f"- #{p['number']} {p['headRefName']} merge={p['mergeStateStatus']} {p['title'][:90]}")
    plan_path = os.path.join(fleet, "batches", "plan")
    if os.path.exists(plan_path):
        queue = [ln for ln in read(plan_path).splitlines()
                 if ln.strip() and not os.path.exists(os.path.join(fleet, "batches", ln.split()[0] + ".closed"))]
        snap += ["", "## Landing queue", *([f"- {q}" for q in queue] or ["- (empty)"])]
    status = [ln for ln in read(os.path.join(fleet, "STATUS.md")).splitlines() if ln.strip()]
    snap += ["", f"## Last {args.events} events", *[f"- {ln[:400]}" for ln in status[-args.events:]]]
    l4 = "\n".join(snap) + "\n"

    nxt = next((ln for ln in reversed(status) if re.search(r"\b(Next|TODO)\b", ln)), None)
    l5 = "# Next action\n\n" + (nxt[:600] if nxt else "Read the snapshot and decide the next step.") + "\n"
    return [l1, l2, l3, l4, l5]


def builder_layers(args):
    l1 = BUILDER_RULES
    l2 = repo_layer(args.repo)
    brief = read(args.brief) if args.brief else ""
    # Drop the brief's own Rules section: layer 1 carries it once for every builder.
    brief = re.sub(r"\n## Rules\n.*", "\n", brief, flags=re.S).strip()
    l3 = f"# Task: issue #{args.issue}\n\n" + brief + "\n"
    # Everything the brief tells the builder to read is inlined, so the builder starts with it
    # instead of fetching it turn by turn: each issue it names (with comments) and each ADR path.
    issues = [args.issue] + [int(n) for n in re.findall(r"gh issue view (\d+)", brief) if int(n) != args.issue]
    for n in dict.fromkeys(issues):
        text = gh(["issue", "view", str(n), "-R", "gerchowl/flock", "--comments"], args.offline)
        l3 += f"\n## Issue #{n} (with comments)\n\n" + (text or "(offline)").strip() + "\n"
    for path in dict.fromkeys(re.findall(r"docs/adr/\d{4}-[a-z0-9-]+\.md", brief)):
        full = os.path.join(args.repo, path)
        if os.path.exists(full):
            l3 += f"\n## {path}\n\n" + read(full).strip() + "\n"
    l4 = "# Snapshot\n\n" + f"base: origin/dev at {git(args.repo, 'rev-parse', '--short', 'origin/dev')}\n"
    l5 = "# Next action\n\nExecute the task above under the builder rules. End with DONE: or BLOCKED:.\n"
    return [l1, l2, l3, l4, l5]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--role", choices=["architect", "builder"], required=True)
    ap.add_argument("--fleet")
    ap.add_argument("--repo", required=True)
    ap.add_argument("--skill")
    ap.add_argument("--issue", type=int)
    ap.add_argument("--brief")
    ap.add_argument("--events", type=int, default=20)
    ap.add_argument("--offline", action="store_true")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    layers = architect_layers(args) if args.role == "architect" else builder_layers(args)

    os.makedirs(args.out, exist_ok=True)
    manifest, compiled, prefix = {"role": args.role, "layers": []}, "", ""
    for n, text in enumerate(layers, 1):
        with open(os.path.join(args.out, f"layer{n}.md"), "w", encoding="utf-8") as fh:
            fh.write(text)
        prefix += text
        compiled += text + BREAK.format(n=n, sha=sha(text))
        manifest["layers"].append({"n": n, "bytes": len(text.encode()), "tokens_est": est_tokens(text),
                                   "sha": sha(text), "prefix_sha": sha(prefix)})
    with open(os.path.join(args.out, "compiled.md"), "w", encoding="utf-8") as fh:
        fh.write(compiled)
    with open(os.path.join(args.out, "manifest.json"), "w", encoding="utf-8") as fh:
        json.dump(manifest, fh, indent=1)
    print(json.dumps(manifest, indent=1))


if __name__ == "__main__":
    main()
