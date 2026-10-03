#!/usr/bin/env python3
"""Archive this repository's issues + PRs into docs/issues and docs/PRs as markdown.

Run before breaking the fork so the issue/PR history is preserved in-repo.

The repository is resolved from the checkout's own `origin` remote rather than
hardcoded, so a fork can never inherit an upstream pointer and archive the wrong
tracker (#512). Every generated filename is derived from the issue title, so a
title that names a host becomes a filename: the hermetic gate (#510) covers the
committed tree, which is what catches it.
"""
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ISSUES_DIR = ROOT / "docs" / "issues"
PRS_DIR = ROOT / "docs" / "PRs"

_OWNER_REPO = re.compile(r"^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$")
# https://host/owner/repo[.git] | ssh://git@host/owner/repo[.git]
# git://host/owner/repo[.git]     | git@host:owner/repo[.git]
_REMOTE = re.compile(
    r"^(?:https?|ssh|git)://[^/]+/(?P<p>[A-Za-z0-9._-]+/[A-Za-z0-9._-]+?)(?:\.git)?/?$"
    r"|^[^/\s]+@[^/\s:]+:(?P<s>[A-Za-z0-9._-]+/[A-Za-z0-9._-]+?)(?:\.git)?$"
)


def resolve_repo(root=None):
    """Return `owner/repo` for the checkout this script lives in.

    Raises if the remote is missing or unparseable: archiving the wrong
    repository silently is worse than refusing to run.
    """
    root = Path(root or ROOT)
    try:
        url = subprocess.run(
            ["git", "-C", str(root), "remote", "get-url", "origin"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
    except (subprocess.CalledProcessError, FileNotFoundError) as exc:
        raise SystemExit(
            f"error: cannot read the origin remote of {root}: {exc}"
        ) from exc
    m = _REMOTE.match(url)
    repo = (m.group("p") or m.group("s")) if m else None
    if not repo or not _OWNER_REPO.match(repo):
        # A local-path or otherwise unrecognised remote is refused rather than
        # guessed at: a wrong owner/repo silently archives another tracker.
        raise SystemExit(f"error: cannot parse owner/repo from origin remote {url!r}")
    return repo


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


def export(kind, repo):
    sub = "issue" if kind == "issue" else "pr"
    dir_ = ISSUES_DIR if kind == "issue" else PRS_DIR
    dir_.mkdir(parents=True, exist_ok=True)
    nums = json.loads(
        gh([sub, "list", "-R", repo, "--state", "all", "--limit", "1000", "--json", "number"])
    )
    fields = "number,title,state,author,labels,body,createdAt,closedAt,url,comments"
    if kind == "pr":
        fields += ",mergedAt,baseRefName,headRefName"
    index = []
    for i, row in enumerate(sorted(nums, key=lambda r: r["number"]), 1):
        n = row["number"]
        item = json.loads(gh([sub, "view", str(n), "-R", repo, "--json", fields]))
        name = write_item(kind, dir_, item)
        index.append((item["number"], item.get("state", ""), item.get("title", ""), name))
        print(f"  [{kind}] {i}/{len(nums)} #{n} -> {name}", flush=True)
    # index file
    lines = [f"# {kind} archive ({repo}) — {len(index)} items\n"]
    for num, state, title, name in index:
        lines.append(f"- [#{num}]({name}) `{state}` — {title}")
    (dir_ / "README.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
    return len(index)


def main(argv):
    which = argv[0] if argv else "both"
    if which not in ("both", "issue", "pr"):
        raise SystemExit(f"error: unknown target {which!r}; use issue, pr or both")
    repo = resolve_repo()
    total = 0
    if which in ("both", "issue"):
        total += export("issue", repo)
    if which in ("both", "pr"):
        total += export("pr", repo)
    print(f"done: {total} items archived from {repo}")


if __name__ == "__main__":
    main(sys.argv[1:])
