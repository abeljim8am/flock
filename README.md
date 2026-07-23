# Flock

**Zellij meets herdr, plus more.**

Flock is an agent-aware terminal workspace built on a fork of
[Zellij](https://github.com/zellij-org/zellij). It keeps Zellij's fast terminal
multiplexing, layouts, plugins, sessions, web client, and collaboration model,
then adds a control plane for coding agents and remote development environments.

## What Flock adds

- **Live agent status** for Codex, Claude Code, OpenCode, Gemini, Kimi, Hermes,
  and other terminal agents: working, waiting, blocked, done, or offline.
- **A unified sidebar** for sessions, projects, cloud workspaces, panes, and
  attention badges, with direct focus and switching.
- **A fast selector** with fuzzy search, frecency ranking, project discovery,
  and switch-or-create behavior.
- **Authoritative agent hooks** for Codex, Claude Code, and OpenCode, with
  screen detection as the zero-config fallback.
- **Remote providers** for GitHub Codespaces, devcontainers, and Coder.
- **Persistent Coder processes** backed by a small remote PTY daemon, while the
  Flock multiplexer, layouts, configuration, and plugins remain on the laptop.
  Remote shells, jobs, and bounded output replay survive laptop and network
  disconnects.
- **Coder workspace creation** with optional dotfiles and typed connection state
  in the local sidebar.

Everything else is still Zellij. The executable is named `flock`, and the
upstream [Zellij documentation](https://zellij.dev/documentation/) applies
unless a Flock feature says otherwise.

## Install

Download the archive, ZIP, or Windows installer for your platform from the
[latest Flock release](https://github.com/abeljim8am/flock/releases/latest).
The current release is `v26.6.1`; `v26.0.0` was the first Flock release.

To build from source:

```sh
git clone https://github.com/abeljim8am/flock.git
cd flock
cargo xtask build
```

For development builds and tests:

```sh
cargo xtask run
cargo xtask test
```

## Enable Flock

The bundled `flock-selector` and `flock-sidebar` layouts work out of the box.
Remote providers are opt-in:

```kdl
plugin location="zellij:flock-selector" {
    root_dirs "~/src"
    codespaces_enabled "true"
    devcontainers_enabled "true"
    coder_enabled "true"
}

plugin location="zellij:flock-sidebar" {
    root_dirs "~/src"
    codespaces_enabled "true"
    devcontainers_enabled "true"
    coder_enabled "true"
}
```

Provider requirements:

- Codespaces: authenticated `gh` CLI.
- Devcontainers: `devcontainer` CLI and Docker.
- Coder: authenticated `coder` CLI. Persistent sessions currently require a
  Linux x86_64 workspace with `tar`, `sha256sum`, and `curl`, `wget`, or Python 3.

Debug builds pass their exact `FLOCK_EXECUTABLE` path into generated local Coder
bridge panes, so `cargo run -- ...` tests the binary you just built instead of a
different `flock` on `PATH`. Set `FLOCK_EXECUTABLE=/absolute/path/to/flock` to
override it explicitly. Set `FLOCK_BUILD_REMOTE_AGENT=1` when running
`cargo xtask run` on a Linux x86_64 laptop to build and stream a static musl
binary to the workspace as the remote agent. This opt-in build requires Zig,
`musl-gcc`, or `x86_64-linux-musl-gcc`. On other platforms, point
`FLOCK_REMOTE_AGENT_BINARY` at a cross-compiled Linux x86_64 Flock binary.
Release builds continue installing the matching checksum-verified release.

### Coder session lifecycle

Coder shells are intentionally owned by the workspace's remote PTY daemon, so
the local host session can be suspended without terminating remote work:

- Detaching leaves the live host session and its remote shells running.
- Killing a session (including `kill-all-sessions`) synchronously saves its
  host layout, then disconnects locally. Reopening the same deterministic
  session name restores its tabs and reconnects to the same remote PTYs.
- Deleting a resumable session is final: Flock durably queues closure of every
  saved remote PTY before removing the host resurrection metadata. Interrupted
  closes are retried the next time Flock starts.
- Stopping or restarting the Coder workspace terminates the workspace daemon,
  so its remote shells are cleaned up immediately.

While a killed host session is intentionally suspended, its remote shells and
jobs remain alive and may continue producing bounded replay output until the
session is reopened, deleted, or the Coder workspace stops.

Agent hook integrations live in
[`default-plugins/flock-sidebar/assets`](default-plugins/flock-sidebar/assets).
They improve accuracy, but are optional because Flock can detect state from pane
content and foreground processes. Each integration is universal: the same
OpenCode plugin or Codex/Claude hook selects the local pipe, devcontainer file
bridge, or Coder remote-agent transport from `FLOCK_STATE_CHANNEL`. Install the
same integration versions in Coder workspaces through your dotfiles or agent
configuration; Flock does not rewrite those configurations automatically.

## Credits and license

Flock is built on Zellij and ports agent-state ideas and integrations from
herdr into the terminal multiplexer itself. It is distributed under the
[MIT license](LICENSE.md).
