#!/bin/sh
# installed by flock-sidebar (flock)
# managed by flock-sidebar; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# FLOCK_INTEGRATION_ID=codex
# FLOCK_INTEGRATION_VERSION=3
#
# Ported from herdr's codex integration hook. The same hook reports through a
# local Flock pipe or the Coder remote-agent daemon according to
# FLOCK_STATE_CHANNEL. Codex carries no subagent frames, so no stdin parsing is
# needed and this stays pure shell.

set -eu

action="${1:-}"
# Drain stdin so codex's hook payload doesn't block on the pipe; we don't use it.
cat >/dev/null 2>&1 || true

case "$action" in
  working|idle|blocked|release) ;;
  *) exit 0 ;;
esac

# Only report from inside a Flock pane. FLOCK_EXECUTABLE is exported by Flock
# so this cannot accidentally resolve to the unrelated util-linux `flock`.
[ -n "${FLOCK_PANE_ID:-}" ] || exit 0
flock_executable="${FLOCK_EXECUTABLE:-}"
[ -n "$flock_executable" ] && [ -x "$flock_executable" ] || exit 0

if [ "${FLOCK_STATE_CHANNEL:-}" = "remote-agent" ]; then
  set -- "$flock_executable" remote-agent report-state \
    --pane-id "$FLOCK_PANE_ID" --state "$action" --agent codex
else
  set -- "$flock_executable" pipe --name flock-state \
    --args "pane_id=${FLOCK_PANE_ID},state=${action},agent=codex,source=flock:codex"
fi

"$@" </dev/null >/dev/null 2>&1 &
pipe_pid=$!
(sleep 2; kill "$pipe_pid" 2>/dev/null || true) &
watchdog_pid=$!
wait "$pipe_pid" 2>/dev/null || true
kill "$watchdog_pid" 2>/dev/null || true
wait "$watchdog_pid" 2>/dev/null || true
