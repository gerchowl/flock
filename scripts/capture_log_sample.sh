#!/usr/bin/env bash
# Capture a REPRESENTATIVE session log for the guardrails `log-budget` gate.
#
# The gate compares an observed event distribution against log-budgets.toml.
# It only COMPARES — producing the sample is this repo's job, and it has to be
# MEASURED rather than authored, or the whole point (catching the level-vs-
# frequency defects a source scan cannot see, #318) is lost.
#
# What it does: runs a real headless `flk server` under a throwaway
# XDG_CONFIG_HOME for a fixed wall-clock window, drives a little ordinary
# traffic over the socket API, then copies the server's JSONL log out. The
# window matters more than the traffic: the defects this gate exists to catch
# are TIMER-driven, so the sample has to span enough sampler ticks for a
# per-iteration event to dominate if one still does.
#
# Usage: scripts/capture_log_sample.sh [seconds]   (default 60)
set -euo pipefail

seconds="${1:-60}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out_dir="$root/logs"
out="$out_dir/flock-session.jsonl"

echo "building flk (debug)..."
cargo build --locked --bin flk >/dev/null

bin="$root/target/debug/flk"
# Debug builds write under `flock-dev`, release under `flock` (app_dir_name).
app_dir="flock-dev"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
config_home="$work/config"
mkdir -p "$config_home/$app_dir"
printf 'onboarding = false\n' > "$config_home/$app_dir/config.toml"
socket="$work/flock.sock"

echo "running a server for ${seconds}s..."
XDG_CONFIG_HOME="$config_home" \
  XDG_RUNTIME_DIR="$work/run" \
  FLOCK_SOCKET_PATH="$socket" \
  SHELL=/bin/sh \
  "$bin" server >/dev/null 2>&1 &
server_pid=$!
# The server is killed rather than asked to stop: a `server.stop` would be one
# more lifecycle event in a sample whose job is to characterise the STEADY
# state. Trap covers the case where the wait below is interrupted.
trap 'kill "$server_pid" 2>/dev/null || true; rm -rf "$work"' EXIT

for _ in $(seq 1 50); do
  [ -S "$socket" ] && break
  sleep 0.1
done
if [ ! -S "$socket" ]; then
  echo "server never bound $socket" >&2
  exit 1
fi

# Ordinary user-shaped traffic, spread across the window so it interleaves with
# the samplers rather than landing in one burst at the start.
#
# Workspace create/focus is deliberately the workload: those are LIFECYCLE
# events, so they are what a healthy log is legitimately made of, and they give
# the percentage direction a real denominator. Without them the sample is a
# dozen startup lines where every event is trivially >5% and the gate reports
# noise on a log that is in fact perfect.
api() {
  printf '%s\n' "$1" | nc -U "$socket" >/dev/null 2>&1 || true
}

ticks=$((seconds / 2))
[ "$ticks" -lt 1 ] && ticks=1
for i in $(seq 1 "$ticks"); do
  api "{\"id\":\"cap_new_$i\",\"method\":\"workspace.create\",\"params\":{\"focus\":true,\"label\":\"cap-$i\"}}"
  api "{\"id\":\"cap_list_$i\",\"method\":\"workspace.list\",\"params\":{}}"
  api "{\"id\":\"cap_ping_$i\",\"method\":\"ping\",\"params\":{}}"
  sleep 2
done

kill "$server_pid" 2>/dev/null || true
wait "$server_pid" 2>/dev/null || true

mkdir -p "$out_dir"
cat "$config_home/$app_dir/flock-server.log" > "$out"
echo "wrote $out ($(wc -l < "$out" | tr -d ' ') records)"
echo
echo "gate it with:  guardrails-log-budget"
