# Flock: good setup out of the box

A fresh `flock` install is byte-for-byte plain Zellij. Everything that makes it
Flock — the sidebar dock, the project selector, agent badges — is opt-in through
hand-authored KDL that the user has to know to write. This plan closes that gap.

Target: **a non-Nix user installs `flock`, runs it, and lands in the real Flock
experience.** Pointing it at project folders should take a few lines of
`config.kdl` at most, stated once.

## Locked decisions

| Decision | Choice |
| --- | --- |
| Bare `flock` | Always opens the project selector |
| Selector keybind | `Super s` |
| Unconfigured project list | Show nothing — no filesystem auto-discovery |
| Scope | This repo only |

`Super s` does not reach the TUI in a number of terminals (it is intercepted by
the OS or the terminal emulator). That is a documented caveat, not a blocker —
the docs need to say so and name the rebind.

Because the project list will not auto-discover, the *empty state* carries the
whole first-run experience. See Phase 3.

## Current gaps

1. **`default_layout` is unset.** `zellij-utils/assets/config/default.kdl:351`
   has it commented out, so bare `flock` loads the built-in `default` layout
   (tab-bar / pane / status-bar). The bundled `flock.kdl` layout — which does
   have the sidebar dock — is unreachable without the user setting
   `default_layout "flock"` themselves.

2. **No keybind opens the selector.** `Alt b` toggles the dock
   (`default.kdl:206`) but nothing launches the picker. Users write their own
   `LaunchOrFocusPlugin` bind and restate every plugin arg. When the arg set
   does not match the layout's, a *second* selector launches — the near-miss
   documented in `zellij-server/src/plugins/plugin_map.rs:563`.

3. **Selector config is stated three or more times:** the entry layout's plugin
   block, the keybind body, and `sidebar_args` re-emitted into every generated
   project/remote layout. There is no single "where my projects live."

4. **New project sessions get plain Zellij.** `DEFAULT_SESSION_LAYOUT = "default"`
   (`default-plugins/flock-selector/src/config.rs:22`), so a session the selector
   creates has no dock unless the user authors their own project layout and points
   `session_layout` at it. Remote sessions are already fine — `codespaces.rs:145`
   has a built-in dock template fallback.

5. **Zero-config selector is blank.** `root_dirs` / `individual_dirs` default to
   `Vec::new()` (`config.rs:79-80`). The UI does distinguish the case
   (`" no project folders configured"`, `ui.rs:291`) but the message is not
   actionable.

6. **The entry point is a shell alias over a hack layout.** The bundled
   `flock-selector.kdl` keeps its session alive with
   `pane command="tail" { args "-f" "/dev/null" }` and ships commented-out
   placeholder args. With bare `flock` becoming the selector, this stops being a
   side path and becomes *the* startup path — it has to be solid.

## Phase 0 — ship working defaults

Small, self-contained, no new config surface. Gets a fresh install from "plain
Zellij" to "sidebar + working selector keybind."

- `default.kdl`: set `default_layout "flock"`.
- `config.rs`: `DEFAULT_SESSION_LAYOUT` → `"flock"`, so selector-created
  sessions get the dock.
- `default.kdl`: register `flock-selector` and `flock-sidebar` in the
  `plugins {}` alias block (alias config already merges with call-site config
  winning — `zellij-utils/src/input/layout.rs:122-145`).
- `default.kdl`: bind `Super s` → `LaunchOrFocusPlugin "flock-selector"`
  (floating, move-to-focused-tab), alongside the existing `Alt b`.

## Phase 1 — a `flock { }` config block

The single source of truth. Parse a top-level `flock { }` node in
`Config::from_kdl` alongside `plugins` / `ui` / `web_client`
(`zellij-utils/src/kdl/mod.rs:4908-4938`) into a `FlockConfig` on `Config`:

```kdl
flock {
    root_dirs "~/src" "~/work"     // scanned one level deep
    individual_dirs "~/nixos"      // each is itself one project
    devcontainers true
    ssh true
}
```

Inject the resolved `FlockConfig` **underneath** the call-site configuration for
every `zellij:flock-selector` / `zellij:flock-sidebar` instance, at plugin-load
time in `zellij-server`. Doing it there (rather than only in alias resolution)
means it applies uniformly to a direct `zellij:flock-selector` in a layout, to
the alias, and to the keybind.

Layout args still override, so a project layout can opt out per-session.

Two payoffs:

- Zero args needed anywhere in the common case.
- **The duplicate-selector bug class disappears by construction.** All three call
  sites now derive identical configuration, so `running_plugin_satisfies_request`
  always matches instead of near-missing.

Also add a `flock { }` key to disable the selector-on-startup behavior, so Flock
stays usable as a drop-in Zellij replacement for anyone who wants a shell when
they type `flock`.

## Phase 2 — bare `flock` opens the selector

Bare `flock` attaches the fixed `flock-selector` session if it is live, else
creates it. Note `--session NAME` does **not** attach when the session is already
live (`AGENTS.md:26`), so this needs real attach-or-create logic rather than a
flag.

- Replace the `tail -f /dev/null` keepalive with something intentional now that
  this is the primary startup path.
- Keep an explicit `flock pick` subcommand as well: bare `flock` is now
  overloaded, and scripts and docs benefit from a name. Gets shell completion
  free via the existing `setup --generate-completion`.
- `attach_to_session` is effectively subsumed — the selector lists live sessions
  with agent badges, so picking one *is* attaching. Confirm the interaction
  rather than assuming it.
- Explicit forms (`flock attach`, `flock --session X`, `flock -l LAYOUT`) keep
  working unchanged.

## Phase 3 — make the empty state carry first-run

This is load-bearing: with no auto-discovery and the selector as the startup
screen, the unconfigured empty state is the first thing every new user sees.

- Replace `" no project folders configured"` (`ui.rs:291`) with actionable text
  that names the fix.
- Add an in-app "add a project folder" action that writes to the user's
  `flock { }` block. Precedent already exists in the SSH tab:
  `" no saved SSH hosts — Ctrl-o adds one"` (`ui.rs:333`).

Result: a fresh install shows a picker that explains itself and gets you
configured in a few keystrokes — without scanning the filesystem behind the
user's back.

## Phase 4 — docs and diagnostics

- Rewrite the README "Enable Flock" section. It currently teaches arg-pasting
  into layouts; it should read "works out of the box, here is how to point it at
  your projects," plus the `Super s` terminal caveat and how to rebind.
- Add the `flock { }` block, commented and documented, to `default.kdl` so
  `flock setup --dump-config` teaches it.
- Extend `flock setup --check` to report the resolved Flock config: project
  roots, which providers are enabled, which layout is default.

## Open risks

- **`Super s` reachability.** Needs testing across the terminals people actually
  use. If it is dead in most, the default is decorative and the docs have to lead
  with the rebind.
- **Selector-always is a visible divergence from Zellij.** The opt-out in Phase 1
  is the mitigation; it should be easy to find.
- **`default_layout "flock"` changes behavior for existing users** who currently
  get the plain default layout. Worth a CHANGELOG note.
