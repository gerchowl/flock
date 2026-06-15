#!/usr/bin/env python3
"""Archive all gerchowl/herdr issues + PRs into docs/issues and docs/PRs as markdown.

Run before breaking the fork so the issue/PR history is preserved in-repo.
"""
import json
import re
import subprocess
import sys
from pathlib import Path

REPO = "gerchowl/herdr"
ROOT = Path(__file__).resolve().parent.parent
ISSUES_DIR = ROOT / "docs" / "issues"
PRS_DIR = ROOT / "docs" / "PRs"


def gh(args):
    out = subprocess.run(
        ["gh", *args], capture_output=True, text=True, check=True
    )
    return out.stdout


def slug(title, n=60):
    s = re.sub(r"[^a-z0-9]+", "-", (title or "").lower()).strip("-")
    return (s[:n].strip("-")) or "untitled"


def fmt_comments(comments):
    if not comments:
        return ""
    parts = ["\n\n---\n\n## Comments\n"]
    for c in comments:
        author = (c.get("author") or {}).get("login", "ghost")
        when = c.get("createdAt", "")
        body = (c.get("body") or "").rstrip()
        parts.append(f"\n### {author} — {when}\n\n{body}\n")
    return "".join(parts)


def write_item(kind, dir_, item):
    num = item["number"]
    title = item.get("title", "")
    state = item.get("state", "")
    author = (item.get("author") or {}).get("login", "ghost")
    labels = [l["name"] for l in item.get("labels", [])]
    created = item.get("createdAt", "")
    closed = item.get("closedAt", "") or ""
    url = item.get("url", "")
    body = (item.get("body") or "").rstrip()
    extra = ""
    if kind == "pr":
        extra = (
            f"merged: {item.get('mergedAt') or ''}\n"
            f"base: {item.get('baseRefName','')}\n"
            f"head: {item.get('headRefName','')}\n"
        )
    fm = (
        "---\n"
        f"number: {num}\n"
        f"title: {json.dumps(title, ensure_ascii=False)}\n"
        f"kind: {kind}\n"
        f"state: {state}\n"
        f"author: {author}\n"
        f"labels: {json.dumps(labels)}\n"
        f"created: {created}\n"
        f"closed: {closed}\n"
        f"{extra}"
        f"url: {url}\n"
        "---\n\n"
    )
    md = fm + f"# {title}\n\n" + (body or "_(no description)_") + fmt_comments(
        item.get("comments")
    )
    path = dir_ / f"{num:04d}-{slug(title)}.md"
    path.write_text(md + "\n", encoding="utf-8")
    return path.name


def export(kind):
    sub = "issue" if kind == "issue" else "pr"
    dir_ = ISSUES_DIR if kind == "issue" else PRS_DIR
    dir_.mkdir(parents=True, exist_ok=True)
    nums = json.loads(
        gh([sub, "list", "-R", REPO, "--state", "all", "--limit", "1000", "--json", "number"])
    )
    fields = "number,title,state,author,labels,body,createdAt,closedAt,url,comments"
    if kind == "pr":
        fields += ",mergedAt,baseRefName,headRefName"
    index = []
    for i, row in enumerate(sorted(nums, key=lambda r: r["number"]), 1):
        n = row["number"]
        item = json.loads(gh([sub, "view", str(n), "-R", REPO, "--json", fields]))
        name = write_item(kind, dir_, item)
        index.append((item["number"], item.get("state", ""), item.get("title", ""), name))
        print(f"  [{kind}] {i}/{len(nums)} #{n} -> {name}", flush=True)
    # index file
    lines = [f"# {kind} archive ({REPO}) — {len(index)} items\n"]
    for num, state, title, name in index:
        lines.append(f"- [#{num}]({name}) `{state}` — {title}")
    (dir_ / "README.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
    return len(index)


if __name__ == "__main__":
    which = sys.argv[1] if len(sys.argv) > 1 else "both"
    total = 0
    if which in ("both", "issue"):
        total += export("issue")
    if which in ("both", "pr"):
        total += export("pr")
    print(f"done: {total} items archived")
