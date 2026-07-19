#!/bin/sh
# installed by flock-sidebar (flock)
# managed by flock-sidebar; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# FLOCK_INTEGRATION_ID=codex
# FLOCK_INTEGRATION_VERSION=1
#
# Ported from herdr's codex integration hook. Instead of writing herdr's unix
# socket, it reports the agent's state to the flock-sidebar plugin over a Zellij
# CLI pipe. Codex carries no subagent frames, so no stdin parsing is needed and
# this stays pure shell.

set -eu

action="${1:-}"
# Drain stdin so codex's hook payload doesn't block on the pipe; we don't use it.
cat >/dev/null 2>&1 || true

case "$action" in
  working|idle|blocked|release) ;;
  *) exit 0 ;;
esac

# Only report from inside a Zellij pane, and only if the CLI is available.
[ -n "${ZELLIJ_PANE_ID:-}" ] || exit 0
command -v flock >/dev/null 2>&1 || exit 0

flock pipe --name flock-state \
  --args "pane_id=${ZELLIJ_PANE_ID},state=${action},agent=codex,source=flock:codex" \
  </dev/null >/dev/null 2>&1 || true
