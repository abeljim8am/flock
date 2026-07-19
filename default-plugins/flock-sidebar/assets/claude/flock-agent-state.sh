#!/bin/sh
# installed by flock-sidebar (flock)
# managed by flock-sidebar; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# FLOCK_INTEGRATION_ID=claude
# FLOCK_INTEGRATION_VERSION=1
#
# Ported from herdr's claude integration hook. Instead of writing herdr's unix
# socket, it reports the agent's state to the flock-sidebar plugin over a Zellij
# CLI pipe:
#
#   flock pipe --name flock-state --args 'pane_id=<id>,state=<state>,agent=claude,...'
#
# Zellij exports the running pane's id as $ZELLIJ_PANE_ID, which the plugin maps
# back to the pane it tracks. The subagent-suppression logic is herdr's, kept
# verbatim so recap/away-summary frames can't revive an idle pane.

set -eu

action="${1:-}"
hook_input_file="$(mktemp "${TMPDIR:-/tmp}/flock-claude-hook.XXXXXX")" || exit 0
trap 'rm -f "$hook_input_file"' EXIT HUP INT TERM
cat >"$hook_input_file" 2>/dev/null || true

case "$action" in
  working|idle|blocked|release) ;;
  *) exit 0 ;;
esac

# Only report from inside a Zellij pane, and only if the CLI is available.
[ -n "${ZELLIJ_PANE_ID:-}" ] || exit 0
command -v flock >/dev/null 2>&1 || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

FLOCK_ACTION="$action" FLOCK_HOOK_INPUT_FILE="$hook_input_file" python3 - <<'PY'
import json
import os
import subprocess

source = "flock:claude"
action = os.environ.get("FLOCK_ACTION", "")
pane_id = os.environ.get("ZELLIJ_PANE_ID")
hook_input_file = os.environ.get("FLOCK_HOOK_INPUT_FILE")

if not pane_id:
    raise SystemExit(0)

hook_input = {}
if hook_input_file:
    try:
        with open(hook_input_file, encoding="utf-8") as handle:
            content = handle.read()
        if content.strip():
            hook_input = json.loads(content)
    except Exception:
        hook_input = {}

hook_event_name = str(hook_input.get("hook_event_name") or "")
is_subagent = bool(hook_input.get("agent_id"))
if hook_event_name == "SubagentStop":
    # SubagentStop is a completion event. Claude recap/away-summary can emit it
    # after the main turn has already stopped. Never let it revive an idle pane.
    raise SystemExit(0)
if is_subagent and action in ("idle", "release"):
    # Subagent completion must not make the parent pane look done early.
    raise SystemExit(0)

args = f"pane_id={pane_id},state={action},agent=claude,source={source}"

try:
    subprocess.run(
        ["flock", "pipe", "--name", "flock-state", "--args", args],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=2,
        check=False,
    )
except Exception:
    pass
PY
