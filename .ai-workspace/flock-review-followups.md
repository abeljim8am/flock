# Flock review — remaining follow-ups

Source: code review of the `flock` bookmark (2026-06-10). The 12 confirmed
findings were fixed in the commit range `6b6277ea..8535ed4f` (termwiz parked-Esc
flush → flock-sidebar dead-code removal). The items below were flagged in the
same review but deliberately left out of that pass — they are larger refactors
or judgment calls, not bugs. Ordered roughly by value.

## 1. Shared crate for the duplicated flock plugin code

`flock-sidebar` and `flock-selector` carry near-verbatim copies of:

- `palette.rs` — the whole module (fg/bg/goto SGR helpers, `RESET`/`BOLD`/`DIM`/
  `NORMAL_INTENSITY`, `Theme` + `Theme::from_style`). The selector copy already
  needs `#[allow(dead_code)]` on ~6 fields. The carefully-documented "muted must
  not be `named.gray`" decision is encoded twice.
- Path config helpers — `split_paths` / `resolve_path` / `expand_home` /
  `normalize` exist in `flock-selector/src/config.rs` and
  `flock-sidebar/src/sessionizer.rs`. The `normalize` divergence was aligned and
  cross-referenced in commit `e852a5e4`, but the comment-discipline only holds
  until someone edits one side. The two plugins are contractually required to
  parse the identical `individual_dirs` / `root_dirs` args the same way (the
  sidebar filters sessions the selector creates).
- Row-rendering toolkit — `truncate_text`, the `Span` struct with
  `new`/`bold`/`dim` builders, and the `render_row` span emitter are duplicated
  between the two `ui.rs` files.

**Suggested shape:** a small `flock-common` lib crate under `default-plugins/`
(both plugins already depend on `zellij-tile`, so a path dependency is
routine; it must compile to `wasm32-wasip1`, which pure code does). Move
`Theme`+SGR helpers, the path-config module, and the `Span`/`truncate_text`
row toolkit. Check `xtask` builds plugins per-crate and doesn't need the new
member listed anywhere.

## 2. flock-selector: replace the hand-rolled fuzzy matcher

`flock-selector/src/fuzzy.rs` is a ~130-line bespoke subsequence matcher with
scores and highlight ranges. The workspace already ships `fuzzy-matcher`
(SkimMatcherV2) via session-manager — `fuzzy_indices` returns score + matched
indices (see `default-plugins/session-manager/src/single_screen.rs:219`).

Two divergent ranking algorithms now ship in one product: the same query can
rank differently in flock-selector vs session-manager, and gap penalties /
word-boundary / camelCase / unicode handling all have to be re-debugged in the
bespoke copy. Note: the char-boundary panic fixed in `bbe45714` originated in
this hand-rolled matcher's byte ranges; switching to the crate removes the
class of bug, not just the instance.

## 3. Sidebar session polling is O(S²) across sessions

Every session's sidebar calls `get_session_list()` once per second
(`SESSION_REFRESH_SECS = 1.0` in `flock-sidebar/src/main.rs`). Each host call
re-scans the sock dir, reads + KDL-parses every session's
`session-metadata.kdl` (which now carries full pane manifests and agent maps),
does a resurrection-cache readdir/stat pass, then broadcasts a full
`SessionUpdate` to **all** plugins with no changed-diff
(`zellij-server/src/screen.rs`, `update_session_infos`). With S sessions
each polling, that's O(S²) file reads/parses per second plus S full-state
plugin broadcasts per second even when nothing changed.

**Options (cheapest first):**
- Diff in `update_session_infos` before broadcasting `Event::SessionUpdate`
  (the sidebar already diffs on its side; the server doesn't).
- Lengthen the poll, or poll only while the sidebar is visible.
- Longer term: have the metadata writer notify on change instead of every
  session polling every other session.

## 4. flock-selector re-runs `find` every 10s forever

`Event::Timer` re-arms `fire_scans()` unconditionally (`REFRESH_SECS = 10.0`,
`flock-selector/src/main.rs:~140`). A picker pane left open forks one
`find -L <root> -maxdepth 1` per configured root every 10 seconds
indefinitely, plus parse + `merge_candidates` re-sorting, even when idle or
hidden. Scan on load and on focus/visibility regain instead, or skip rescans
while the pane is hidden/unfocused.

## 5. Claude hook script spawns python3 per hook event

`default-plugins/flock-sidebar/assets/claude/flock-agent-state.sh` creates a
temp file (`mktemp` + `cat`), then boots a full python3 interpreter just to
read `hook_event_name` and `agent_id` from stdin JSON, then execs `zellij pipe`
via `subprocess`. Claude Code fires hooks on every state transition, so that's
~30–50ms of interpreter startup plus two extra process spawns per event. The
sibling codex script (`assets/codex/flock-agent-state.sh`) proves the
pure-shell path works — parse the two fields with `case`/parameter expansion
(or only invoke python when genuinely needed) and exec `zellij` directly.

## 6. Hook pipe protocol is hardcoded in three places

The `flock-state` pipe name and the `pane_id=,state=,agent=,source=` arg format
are spelled out independently in the claude script, the codex script, and
`hook.rs::parse_hook_report`. Both scripts also duplicate the same shell
prologue (managed-file header, `FLOCK_INTEGRATION_VERSION`, `set -eu`, the
action whitelist, the `ZELLIJ_PANE_ID`/zellij-CLI guards). Adding a state or
renaming an arg takes three synchronized edits, and both scripts swallow all
errors by design — a missed edit silently stops sidebar updates for one agent.
Consider one templated script (or a sourced common snippet) with only the
claude-specific subagent suppression differing. Note: commit `8535ed4f`
dropped `source`/`message` from the *parsed* report (the parser tolerates and
ignores them) — if the scripts are ever regenerated, those args can be dropped
from the emit line too.

## 7. Altitude: plugin-specific state baked into core types

Core `SessionInfo` (`zellij-utils/src/data.rs`) gained `flock_sidebar_state`
and `agent_states`, plumbed through Screen state, the protobuf `Event`
conversion, and the KDL session-metadata serializer. `PublishFlockSidebarState`
is a plugin-named command in the public plugin API (own `ScreenInstruction`,
`ScreenContext`, proto `CommandName`, shim), parallel to `PublishAgentState`.

Consequences: every future plugin wanting cross-session state must edit five
core layers, and the wire/persistence formats permanently encode one bundled
plugin's name and presentation enum (renaming/removing the sidebar becomes a
breaking change to on-disk session metadata).

**Suggested shape:** a generic "publish session state under a plugin-scoped
key" command (plugin-keyed key/value blob riding the existing cross-session
metadata transport), with `flock_sidebar_state`/`agent_states` as the first
two users. Worth doing before the wire format calcifies further.

## 8. `HIDDEN_SESSION_NAME = "flock-selector"` magic-string coupling

The sidebar hides the selector's cold-shell host session by hardcoding its
session name (`flock-sidebar/src/main.rs`), which must match the
`session_name` arg inside the bundled `flock-selector.kdl` layout. A user who
names a real session `flock-selector` has it silently vanish from the sidebar;
editing the bundled layout's name makes the launcher session reappear as a
ghost workspace everywhere. A real "hidden/utility session" attribute on
`SessionInfo` (or server-enforced naming convention) is the right altitude —
pairs naturally with item 7.

## 9. Render-path clones in the sidebar

`visible_sessions()` (`flock-sidebar/src/main.rs`) deep-clones every
`SessionInfo` (pane manifests, tabs, agent maps) on every render, and
`select_next`/`select_prev` → `targets()` rebuilds the same cloned Vec per
scroll notch — up to ~8×/sec while anything is working and visible. Filter by
reference (`Vec<&SessionInfo>`) or cache the filtered list and invalidate on
`SessionUpdate`. (Lower priority now that backgrounded sessions stay on the
slow tick — commit `980caaa7`.)

## 10. Smaller notes

- **`ResizePaneIdToFixedWidth` is a one-way door.** Commit `06202409` validates
  and rolls back failures, but a successfully-applied `Fixed` cols constraint
  still has no API to convert back to a flexible/percent dimension — ordinary
  resizes of that pane then fail with `CantResizeFixedPanes`. Also, a future
  fixed-*height* rail (the obvious sibling feature) would require duplicating
  the whole six-layer chain; consider generalizing `ResizeStrategy` with an
  exact-size variant instead.
- **Builtin tag list has two sources of truth.** The `zellij:` tag whitelist in
  `zellij-utils/src/input/plugins.rs` and the `add_plugin!` asset list in
  `consts.rs` must be kept in sync by hand (pre-existing pattern, now two
  entries longer). A macro or build-time check linking them would prevent the
  next builtin from shipping half-wired. (The is-builtin permission auto-grant
  itself was verified to match zellij's pre-existing trust model — builtins
  already bypassed `check_command_permission` on main.)
- **Denied permission writes no reply for `*AndReply` shims.** The new
  synchronous shims (`kill_sessions`, `delete_dead_session`,
  `delete_all_dead_sessions`) read a host reply that the `Denied` arm of
  `host_run_plugin_command` never writes; they degrade to a misleading
  `Err("EOF while parsing")`. This follows a pre-existing pattern (dozens of
  older reply-reading shims have the same gap, some of which `unwrap` and would
  panic), so the right fix is in the `Denied` arm — write an error response
  for reply-bearing commands — as its own cleanup across all of them.
- **`delete_all_dead_sessions` timeout doesn't cancel the work.** The 500ms
  `tokio::time::timeout` around the `spawn_blocking` fs task reports
  "Timed out deleting dead sessions" while the deletion may still complete
  (or fail) in the background, and `resurrectable_sessions.rs` clears its list
  optimistically before the result is known. The plugin currently ignores the
  reply, so this is cosmetic — until something starts trusting it.
- **flock-sidebar unit tests can't run in this environment.** The crate's
  `.cargo/config.toml` targets `wasm32-wasip1`, so `cargo test` from the
  plugin dir builds a wasm test binary that needs a wasi runner (none
  installed; `cargo test -p flock-sidebar` from the workspace root fails to
  link the host-fn imports instead). Either install/configure a runner
  (`CARGO_TARGET_WASM32_WASIP1_RUNNER=wasmtime`) in CI/dev setup, or give
  `zellij-tile` host-target stubs for the new shim fns so host `cargo test`
  links. Until then, `cargo check --tests` is the only local verification.
