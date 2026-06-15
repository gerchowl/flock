---
number: 142
title: "Ship the status line + built-in config.toml defaults from herdr"
kind: issue
state: OPEN
author: gerchowl
labels: []
created: 2026-06-14T13:40:43Z
closed: 
url: https://github.com/gerchowl/herdr/issues/142
---

# Ship the status line + built-in config.toml defaults from herdr

Follow-up to #136 (P2). The presentation layer lives in the consumer's config repo today (status-line script with a hardcoded ~/dotfiles path; config.toml defaults re-derived per host). It should ship with herdr.

## Scope
- [ ] First-class `herdr statusline` subcommand/binary (reads the agent stdin JSON, prints model | cwd | branch | ...). Removes the consumer's hardcoded path.
- [ ] Built-in config.toml styling defaults the user's config overlays (not replaces).
- [ ] Content-hash drift detection: marker (vN) catches protocol changes; a content hash catches silent script-body changes within the same N.
- [ ] (stretch) optional session RAM/CPU in the statusline — cross-platform (macOS proc_pid_rusage vs Linux /proc).

Refs #136. Independent of #140.
