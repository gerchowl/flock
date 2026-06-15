---
number: 145
title: "Keys and clipboard not reaching inner pane when Claude Code runs inside a herdr pane"
kind: issue
state: OPEN
author: gerchowl
labels: []
created: 2026-06-14T14:17:40Z
closed: 
url: https://github.com/gerchowl/herdr/issues/145
---

# Keys and clipboard not reaching inner pane when Claude Code runs inside a herdr pane

## Current behavior
Running Claude Code inside a herdr pane, two interactions don't reach Claude's TUI:

1. **Paste into Claude's prompt** — pasting from the system clipboard (cmd+v in Alacritty) does not land in Claude's input. Nothing appears, or the paste is consumed by herdr.
2. **Arrow-key navigation of Claude's option picker** — Claude renders a question with multiple options navigated by ↑/↓ + Enter. Inside a herdr pane, only Enter reaches Claude; ↑/↓ do not, so the highlight is stuck on the first option.

Mouse-clicking an option line **does** select it, so mouse events pass through. Specific key events (arrows, bracketed-paste sequences) appear to be consumed by herdr instead of forwarded.

## Expected behavior
A pane running Claude Code should behave the same as a bare Alacritty tab: clipboard paste arrives at Claude's input, and ↑/↓ moves the highlight in Claude's option picker.

## Shortest reproduction
1. `herdr` (fresh server)
2. In the root pane: `claude`
3. Ask Claude something that triggers an AskUserQuestion with ≥2 options
4. Press ↑/↓ — highlight does not move
5. Copy a string to the system clipboard, focus the Claude pane, cmd+v — text does not appear in Claude's input

## Impact
Makes Claude Code awkward to use inside herdr for any flow that needs pasting (URLs, error output, long prompts) or selecting from a question prompt. Mouse-click is the only workaround for the picker; there's no workaround for paste.

## Versions / environment
- herdr `0.6.8-fork.27beb41`
- macOS `26.4`, Apple Silicon (mba22)
- Terminal: Alacritty
- `TERM=xterm-256color`
- Shell: zsh
- Claude Code CLI (latest)
