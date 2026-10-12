#!/usr/bin/env bash
# replay.sh <tag> <rounds>: the ADR-0032 "today vs compiled" replays (#1042).
#
# Inputs, all prepared beforehand and passed by environment:
#   WORK        scratch dir for prompts, fresh run dirs and results
#   WT_A, WT_B  two sandbox builder dirs (AGENTS.md, docs/adr, BRIEF.md for #1021 / #1018)
#   FLEET_COPY  a read-only copy of the architect's .fleet/ state
#   B_A, B_B    compile_context.py --role builder output dirs for the two briefs
#   ARCH        compile_context.py --role architect output dir
#   SUMMARY     a real compaction summary (text), made unique per run here
#   PROBE_MODEL model for probe_claude.sh (default claude-haiku-5-5)
# Every run gets a fresh copy of its dir, so no run inherits another's per-session
# cache. Warm the architect layer 1 once before (one probe call with
# --append-system-prompt-file "$ARCH/layer1.md" and the same tools).
set -u
here=$(cd "$(dirname "$0")" && pwd); R=$here/probe_claude.sh; tag=$1; rounds=$2
fresh() { local d=$WORK/dirs/$tag-$1-$RANDOM$RANDOM; mkdir -p "$WORK/dirs"; cp -R "$2" "$d"; echo "$d"; }
G='MEASUREMENT RUN: do not edit files, build, run tests, commit or push.'
TB=(--max-turns 15 --allowedTools "Read" "Bash(gh issue view:*)" "Bash(cat:*)" "Bash(head:*)" "Bash(sed:*)" "Bash(grep:*)" "Bash(ls:*)")
TA=(--max-turns 15 --allowedTools "Read" "Bash(cat:*)" "Bash(tail:*)" "Bash(head:*)" "Bash(ls:*)" "Bash(grep:*)" "Bash(python3:*)")
X=(--exclude-dynamic-system-prompt-sections --append-system-prompt-file)
printf '%s\n' "Read BRIEF.md and execute it exactly. $G Do only its reading steps, then reply with a 5-line implementation plan and stop." > "$WORK/bt.txt"
for s in A B; do d=B_$s; { cat "${!d}/layer3.md" "${!d}/layer4.md"; echo; echo "# Next action"; echo; echo "$G Everything the task needs to be read is above; reply with a 5-line implementation plan and stop."; } > "$WORK/bc-$s.txt"; done
CONT='You are the architect for flock (goal-s1 armed). Context was compacted. 1. Read GOAL.md (frozen) and the tail (~30 lines) of STATUS.md (latest SNAPSHOT is authoritative), then `tail -5 supervisor.log`. 2. Check landing: `cat batches/plan`, `ls batches` (.closed = merged, .deferred = dropped from that batch). 3. Reply with: the current subgoal, what is in flight, and the single next action. MEASUREMENT RUN: do not run flk or gh, do not edit any file.'
CC='You are the architect for flock (goal-s1 armed). Context was compacted. The compiled context below (repo, goal and decisions, snapshot, next action) is authoritative. Reply with: the current subgoal, what is in flight, and the single next action. MEASUREMENT RUN: do not run flk or gh, do not edit any file.'
for r in $(seq 1 "$rounds"); do
  nonce="round-$tag-$r-$RANDOM$RANDOM"
  # new layer 1-2 bytes per round: the first builder of a round is cold, the second warm
  { echo "<!-- $nonce -->"; cat "$B_A/layer1.md" "$B_A/layer2.md"; } > "$WORK/bshared-$r.md"
  $R "$(fresh btA "$WT_A")" "$WORK/$tag-bt-A-$r.json" "$WORK/bt.txt" "${TB[@]}"
  $R "$(fresh btB "$WT_B")" "$WORK/$tag-bt-B-$r.json" "$WORK/bt.txt" "${TB[@]}"
  $R "$(fresh bcA "$WT_A")" "$WORK/$tag-bc-A-$r.json" "$WORK/bc-A.txt" "${X[@]}" "$WORK/bshared-$r.md" "${TB[@]}"
  $R "$(fresh bcB "$WT_B")" "$WORK/$tag-bc-B-$r.json" "$WORK/bc-B.txt" "${X[@]}" "$WORK/bshared-$r.md" "${TB[@]}"
  { echo "summary-$nonce"; cat "$SUMMARY"; echo; echo "$CONT"; } > "$WORK/at-$r.txt"
  { echo "summary-$nonce"; cat "$SUMMARY"; echo; echo "$CC"; echo; cat "$ARCH/layer2.md" "$ARCH/layer3.md" "$ARCH/layer4.md" "$ARCH/layer5.md"; } > "$WORK/ac-$r.txt"
  { echo "<!-- arch-$nonce -->"; cat "$ARCH/layer1.md"; } > "$WORK/acold-$r.md"
  $R "$(fresh at "$FLEET_COPY")" "$WORK/$tag-at-$r.json" "$WORK/at-$r.txt" "${TA[@]}"
  $R "$(fresh acc "$FLEET_COPY")" "$WORK/$tag-acc-$r.json" "$WORK/ac-$r.txt" "${X[@]}" "$WORK/acold-$r.md" "${TA[@]}"
  $R "$(fresh acw "$FLEET_COPY")" "$WORK/$tag-acw-$r.json" "$WORK/ac-$r.txt" "${X[@]}" "$ARCH/layer1.md" "${TA[@]}"
done
