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
- **Persistent Coder sessions** backed by Zellij inside the workspace, so tabs,
  panes, jobs, and scrollback survive laptop and network disconnects.
- **Coder workspace creation** with optional dotfiles and reconnecting remote
  state snapshots in the local sidebar.

Everything else is still Zellij. The executable is named `flock`, and the
upstream [Zellij documentation](https://zellij.dev/documentation/) applies
unless a Flock feature says otherwise.

## Install

Download the archive, ZIP, or Windows installer for your platform from the
[latest Flock release](https://github.com/abeljim8am/flock/releases/latest).
Release `v26.0.0` is the first Flock release.

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

Agent hook integrations live in
[`default-plugins/flock-sidebar/assets`](default-plugins/flock-sidebar/assets).
They improve accuracy, but are optional because Flock can detect state from pane
content and foreground processes.

## Credits and license

Flock is built on Zellij and ports agent-state ideas and integrations from
herdr into the terminal multiplexer itself. It is distributed under the
[MIT license](LICENSE.md).
