---
number: 106
title: "branch_session injects a 'you have been branched, pivot' prompt into the forked agent"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-13T09:00:49Z
closed: 2026-06-13T09:42:53Z
url: https://github.com/gerchowl/herdr/issues/106
---

# branch_session injects a 'you have been branched, pivot' prompt into the forked agent

## Feature (user): branch_session injects a pivot prompt into the forked agent

When ctrl+shift+b (branch_session) forks the focused agent into a new worktree, the FORKED agent currently resumes the same session cold -- it doesn't know it was branched, so it tends to duplicate the parent's direction instead of diverging. Inject a short context message into the fork's chat on branch, e.g.:

> "You have been branched from a parent session into a new worktree (<branch>). The parent continues the original direction; you should pivot -- pick up a different thread, an alternative approach, or the next sub-task. Re-orient before continuing."

So the human's intent in branching (explore a fork in parallel) is communicated to the agent.

## Design questions
- **Injection mechanism**: how does herdr inject into a resumed Claude session? branch_session resumes the agent in the new worktree -- the inject would be a synthetic user message at resume. Does herdr drive the agent CLI (claude --resume) such that it can prepend a prompt? Likely: write the message as the first input to the forked pane's PTY, OR via a claude-code resume hook. Investigate the resume path.
- **Configurable text**: a [keys] or [branch] config for the injected template (with a <branch> placeholder), default as above. Empty = no injection (opt-out).
- **One-shot**: injected once at fork, never on subsequent resumes of that workspace.

## Pitfalls
- Don't inject into a non-Claude pane (a plain shell branch) -- only when the branched session is an agent.
- Timing: the agent must be ready to receive input (resume completed) before the inject, else it's lost (the #1 stale-alias / readiness lesson).
- Idempotency: a re-branch or re-resume must not re-inject.

## References
branch_session (fork worktree + resume agent), the attention/agent-session plumbing, claude-code resume/hooks.

---

## Comments

### gerchowl — 2026-06-13T09:26:43Z

## Spike consolidated (2 design reviews: herdr-internals + claude-code capabilities)

Two viable mechanisms surfaced; they DISAGREE on one empirical point.

### Option A -- positional prompt on the fork launch (herdr-internals review)
`branch_plan()` (src/agent_resume.rs:150) is the SINGLE construction site of the fork command, today `["claude", "--resume", "<id>", "--fork-session"]`. Append the pivot message as one more argv element -> Claude takes it as the first user turn at process start. **~20-line patch**, no PTY-write timing, no readiness signal, no hook plumbing.
- **Idempotency is automatic**: the argv is built once at branch time, consumed once (worktrees.rs:761), and NOT persisted -- later resumes reconstruct the plan via agent_resume::plan() which appends neither --fork-session nor the message. So later resumes naturally re-inject nothing.
- **Readiness is a non-issue**: the kernel guarantees argv is present before claude's first instruction. (Critically, the reviewer grepped and found NO input-ready signal anywhere in herdr -- which KILLS the PTY-write alternative the issue listed.)
- **Non-claude agents** (codex/copilot) don't take a positional initial prompt -> claude-only for v1 (branch_plan already returns None for non-official sources, so the guard exists).
- **Semantic fit**: the nudge BECOMES the first user turn -- the human effectively saying "you're branched, pivot." For a directive nudge that is arguably the RIGHT altitude (the agent acts on it immediately), not a con.

### Option B -- SessionStart hook + additionalContext (claude-code review)
A `SessionStart` hook (matcher `startup`) emits `hookSpecificOutput.additionalContext`, gated by `HERDR_BRANCHED_FROM` env + a one-shot `.herdr/branched.pending` sentinel the hook deletes. System-reminder altitude (transient, not persistent like --append-system-prompt), official mechanism.
- More moving parts: env signal + sentinel file + a shipped hook in the worktree's .claude/settings.json.
- Softer altitude: ambient guidance vs a directive turn.

### The disagreement (empirically decidable)
B claims positional-prompt-WITH-`--resume` is "brittle / some versions ignore the positional when resuming interactively." A claims claude is interactive-by-default and the positional seeds the first turn. **This is a 1-minute test**: does `claude --resume <id> --fork-session "msg"` actually start interactive with the message as the first turn? If YES -> Option A wins decisively (simplest, self-idempotent, right altitude). If the positional is ignored/run-and-exits under --resume -> fall back to Option B.

### Recommendation
**Option A, pending the empirical check.** It is dramatically simpler, structurally idempotent, needs no hook/sentinel machinery, and the directive altitude fits a "pivot now" nudge. Config: `[branch] pivot_message = "..."` (empty = opt-out, <branch> placeholder). claude-only v1; codex/copilot fork cold (separate spike if wanted). If the check fails, Option B is the clean fallback with the same config knob.

### gerchowl — 2026-06-13T09:42:52Z

Shipped in PR #107 (spike -> Option A, positional prompt). [worktrees] branch_pivot_message, claude-only, idempotent by construction. Live-verify the interactive seeding by branching once; SessionStart-hook fallback (Option B) documented in the spike if needed.

