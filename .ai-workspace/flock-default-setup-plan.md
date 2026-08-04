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

- Make the built-in `flock` layout the startup **fallback**, not a shipped
  `default_layout` value. `default_layout` stays unset, so a user's own
  `layouts/default.kdl` is still what loads on startup; only when they have not
  written one does Flock use its own layout. Setting the option instead would
  skip that lookup and silently ignore a file the user expects to be their
  startup layout — a breaking change for no benefit. The rule lives in two
  places that must agree (`LayoutInfo::from_config` and the loader in
  `layout.rs`), keyed off `FALLBACK_BUILTIN_LAYOUT`.
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

Project the resolved `FlockConfig` **underneath** the `flock-selector` /
`flock-sidebar` plugin aliases, computed on demand via
`Config::plugin_aliases_with_flock_defaults()` and applied where the server
consumes the aliases. Call-site args still override, so a layout can opt out.

Two payoffs:

- Zero args needed anywhere in the common case.
- **The duplicate-selector bug class disappears by construction.** Every call
  site derives identical configuration, so `running_plugin_satisfies_request`
  always matches instead of near-missing.

The plan originally called for injecting at plugin-load time in `zellij-server`,
to also cover a layout naming `zellij:flock-selector` directly. Projecting onto
the aliases instead accepts that gap — documented, and narrow now that the
bundled layouts and the README all use the alias form — in exchange for one seam
in `zellij-utils` that is fully unit-testable.

What it must **not** do is fold the projection into `Config.plugins`. The stored
aliases are what gets serialized back to disk (the configuration plugin writes
the whole config, and the first-run wizard triggers it), so baking the values in
would be a one-way door: the next write copies them into each alias body, where
they outrank `flock { }` itself, and editing the block silently stops having any
effect. `FlockConfig` therefore needs `to_kdl` as well, or the section is dropped
on that same write.

Deferred to Phase 2: the key that disables selector-on-startup. That behavior
does not exist yet, and shipping a config key that does nothing is worse than
adding it when it works.

## Phase 2 — bare `flock` opens the selector

Bare `flock` attaches the fixed `flock-selector` session if it is live, else
creates it. Note `--session NAME` does **not** attach when the session is already
live (`AGENTS.md:26`), so this needs real attach-or-create logic rather than a
flag.

Implemented by rewriting the request into the `attach --create` form and letting
that branch run, rather than adding a parallel create path. That branch already
resolves live / dead-but-resurrectable / absent correctly — and a hand-rolled
create would call `assert_session_ne`, which **exits the process** when the name
exists as a dead session, so bare `flock` would have started failing outright
once a stale picker snapshot existed.

- The `tail -f /dev/null` keepalive is gone: the picker is the session's only
  pane. A plugin pane cannot exit, so the shell-exits-during-startup race it
  guarded against cannot happen. This also matches the tiled (not floating)
  selector pane the author's own daily layout already used.
- `flock pick` (alias `p`) added. Falls through to `start_client` with no
  `main.rs` change, and gets shell completion free via
  `setup --generate-completion`.
- `attach_to_session` needed no handling: it is only consulted inside the
  `session_name` branch, which selector mode already declines to touch. Confirmed
  by reading the branch, not assumed.
- Explicit forms (`flock attach`, `flock --session X`, `flock -l LAYOUT`, other
  subcommands, reconnects) keep working unchanged.
- The first-run setup wizard is deliberately **not** special-cased. On a brand-new
  install it floats over the picker, which reads as a reasonable funnel once
  Phase 3 makes the empty project list actionable: configure UI in the wizard,
  dismiss, then be told how to add a project folder.

## Phase 3 — make the empty state carry first-run

This is load-bearing: with no auto-discovery and the selector as the startup
screen, the unconfigured empty state is the first thing every new user sees.

- Replaced `" no project folders configured"` with a multi-row block that names
  the file, shows the `flock { root_dirs ... }` snippet, and says to reopen the
  selector. Degrades to a single line that still names the fix when the pane is
  too short, rather than truncating mid-snippet.
- Also rewrote the *configured-but-empty* state, which was `" no projects found"`.
  It now names the likely cause: `root_dirs` is scanned one level deep, so
  pointing it at a project rather than its parent finds nothing, and
  `individual_dirs` is the option for a folder that is itself a project.

**The in-app "add a project folder" action was dropped, deliberately.** The only
way a plugin can persist to `config.kdl` is `reconfigure`, which rewrites the
whole file from the serialized config — it backs the old one up and prepends a
pointer to the backup, but comments and formatting do not survive in the live
file. It also needs a new `Reconfigure` permission (a one-time dialog for every
existing user), and for a declaratively-managed config it would replace the
symlink and break the manager. Paying all that to save one paste, on a file the
user owns and we document with comments, is the wrong trade. Revisit only if the
`flock { }` block can be patched surgically with comments preserved.

Result: a fresh install shows a picker that explains itself — without scanning the
filesystem behind the user's back, and without rewriting a file the user owns.

## Phase 4 — docs and diagnostics

- ~~Rewrite the README "Enable Flock" section.~~ Done incrementally across
  phases 0–3; it now reads "works out of the box, here is how to point it at your
  projects", with the `Super s` caveat and the rebind.
- ~~Add the `flock { }` block, commented and documented, to `default.kdl`.~~ Done
  in Phase 1, so `flock setup --dump-config` teaches it.
- `flock setup --check` reports the resolved Flock config. It already listed the
  directories Flock *searches*; it now also reports what it *resolved* —
  `[STARTUP LAYOUT]` (built-in vs. a user file, by path), `[SELECTOR ON STARTUP]`,
  the project folders, the enabled providers, and the `flock` plugin aliases.

  Deliberately loud rather than terse in the cases where configuration silently
  does nothing: no project folders, selector opted out, or a missing `flock`
  plugin alias (which makes the whole `flock { }` section inert for that plugin).
  That last case is unreachable by editing config.kdl — alias merging can override
  an entry but never remove one — but it is reachable through the public
  `plugin_aliases_with_flock_defaults`, and is unit-tested rather than left as
  dead code.

  This is the diagnostic whose absence made verifying Phase 0 and Phase 2 by hand
  so awkward: both needed a live session and a `dump-layout` to answer "which
  layout won" and "did the config reach the plugin".

## Open risks

- **`Super s` reachability.** Needs testing across the terminals people actually
  use. If it is dead in most, the default is decorative and the docs have to lead
  with the rebind.
- **Selector-always is a visible divergence from Zellij.** The opt-out in Phase 1
  is the mitigation; it should be easy to find.
- **The flock fallback changes the startup chrome for existing users** who have
  no `layouts/default.kdl` and were getting the plain default layout. They keep
  every pane they had plus a sidebar dock, and `default_layout "default"` opts
  out. Worth a CHANGELOG note, but not breaking: anyone with their own
  `default.kdl` is unaffected by construction.
- **Verifying startup layout resolution by hand is booby-trapped.** On a config
  dir with no `config.kdl`, the first-run setup wizard both overrides the layout
  and *writes a `config.kdl` into that directory* — which silently pollutes a
  test fixture if you point the binary at one. Pre-seed a `config.kdl` in a temp
  config dir when checking this by hand.
