#!/bin/sh
# S0 probe 6 (ADR-0036 owner decision 1, §1): does `claude auth login` work,
# and where does its token land, for a macOS user with no GUI login session?
#
# BLOCKED on the operator: it needs a second macOS user, which an agent may
# not create (no sudo). Run it AS that user, never as the operator:
#
#   1. Create the user once (O19a #1208 does this for flk-agent; for a
#      throwaway check any non-admin user works), and do NOT log it in at
#      the login window, so it has no GUI session and no unlocked login
#      keychain.
#   2. Start a shell as it without a GUI login, e.g.
#        ssh flk-agent@localhost        (Remote Login on), or
#        sudo -iu flk-agent             (run by the operator)
#   3. sh /path/to/flock/docs/adr/0036-probes/s0_login_flk_agent.sh
#      and complete the browser step it prints on the operator's browser.
#   4. Paste the output into #1307. Afterwards: `claude auth logout` with the
#      same CLAUDE_CONFIG_DIR, and `rm -rf "$HOME/.flk/claude/s0-login"`.
#
# What the binary (2.1.296) does, read from its code: the OAuth blob is
# written with `security add-generic-password -U -a "$USER" -s
# "Claude Code-credentials-<first 8 hex of sha256(CLAUDE_CONFIG_DIR)>"` into
# the user's default keychain. If that write fails, and the failure is not
# "keychain locked" (security exit 36) for an item the keychain already
# holds, it falls back to a plaintext `.credentials.json` in the config dir.
# This script records which of the two happened and whether a later,
# separate non-GUI session can read the token back.
#
# It prints no secret: `security find-generic-password` is called without
# `-w`, and the credentials file is only listed, never read.

set -u
export CLAUDE_CONFIG_DIR="$HOME/.flk/claude/s0-login"
mkdir -p "$CLAUDE_CONFIG_DIR"
chmod 700 "$HOME/.flk" "$HOME/.flk/claude" "$CLAUDE_CONFIG_DIR" 2>/dev/null
H=$(printf %s "$CLAUDE_CONFIG_DIR" | shasum -a 256 | cut -c1-8)
SVC="Claude Code-credentials-$H"

echo "== who"; id; sw_vers -productVersion
echo "== GUI session for this uid?"
launchctl print "gui/$(id -u)" >/dev/null 2>&1 && echo "gui domain: present" || echo "gui domain: absent"
echo "== keychains"
security default-keychain -d user 2>&1
security list-keychains -d user 2>&1
security show-keychain-info 2>&1
echo "== claude"; claude --version

echo "== login (complete the browser step; paste the code back if asked)"
claude auth login
echo "login exit=$?"

echo "== where did the token land?"
security find-generic-password -a "$USER" -s "$SVC" >/dev/null 2>&1
echo "keychain item '$SVC': exit=$? (0 found, 44 not found, 36 locked)"
ls -l "$CLAUDE_CONFIG_DIR/.credentials.json" 2>&1

echo "== can this session use it?"
claude auth status 2>&1 | sed -n 1,5p
claude -p --model haiku --setting-sources local --strict-mcp-config --no-session-persistence "Reply with exactly: ok"
echo "turn exit=$?"

echo "== now open a NEW non-GUI shell as this user and run only:"
echo "   CLAUDE_CONFIG_DIR=$CLAUDE_CONFIG_DIR claude -p --model haiku --no-session-persistence 'Reply with exactly: ok'"
echo "   (and once more after a reboot, before any login: a locked keychain is exit 36)"
