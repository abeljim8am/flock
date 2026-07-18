<h1 align="center">
  <br>
  <img src="https://raw.githubusercontent.com/zellij-org/zellij/main/assets/logo.png" alt="logo" width="200">
  <br>
  Zellij
  <br>
  <br>
</h1>

<p align="center">
  <img src="https://raw.githubusercontent.com/zellij-org/zellij/main/assets/demo.gif" alt="demo">
</p>
<h4 align="center">
  [<a href="https://zellij.dev/documentation/installation">Installation</a>]
  [<a href="https://zellij.dev/screencasts/">Screencasts & Tutorials</a>]
  [<a href="https://zellij.dev/documentation/configuration">Configuration</a>]
  [<a href="https://zellij.dev/documentation/layouts">Layouts</a>]
  [<a href="https://zellij.dev/documentation/faq">FAQ</a>]
</h4>
<p align="center">
  <a href="https://discord.gg/CrUAFH3"><img alt="Discord Chat" src="https://img.shields.io/discord/771367133715628073?color=5865F2&label=discord&style=flat-square"></a>
  <a href="https://matrix.to/#/#zellij_general:matrix.org"><img alt="Matrix Chat" src="https://img.shields.io/matrix/zellij_general:matrix.org?color=1d7e64&label=matrix%20chat&style=flat-square&logo=matrix"></a>
  <a href="https://zellij.dev/documentation/"><img alt="Zellij documentation" src="https://img.shields.io/badge/zellij-documentation-fc0060?style=flat-square"></a>
</p>

<br>
    <p align="center">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://github.com/user-attachments/assets/bc5daac4-140a-4b83-8729-71c944ee1100">
      <img src="https://github.com/user-attachments/assets/55156624-a71a-46b5-939e-f562e3b2dd7f" alt="Sponsored by ">
    </picture>
    &nbsp;
    &nbsp;
    <a href="https://www.gresearch.com/">
        <picture>
          <source media="(prefers-color-scheme: dark)" srcset="https://github.com/user-attachments/assets/d609936a-abf8-4406-8cfc-889f76a09d74">
          <img src="https://github.com/user-attachments/assets/742ae902-fe9d-41c6-baf2-4bc143061da3" alt="gresearch logo">
        </picture>
    </a>
</p>

# What is this?

[Zellij](#origin-of-the-name) is a workspace aimed at developers, ops-oriented people and anyone who loves the terminal. Similar programs are sometimes called "Terminal Multiplexers".

Zellij is designed around the philosophy that one must not sacrifice simplicity for power, taking pride in its great experience out of the box as well as the advanced features it places at its users' fingertips.

Zellij is geared toward beginner and power users alike - allowing deep customizability, personal automation through [layouts](https://zellij.dev/documentation/layouts.html), true multiplayer collaboration, unique UX features such as floating and stacked panes, and a [plugin system](https://zellij.dev/documentation/plugins.html) allowing one to create plugins in any language that compiles to WebAssembly.

Zellij includes a built-in [web-client](https://zellij.dev/tutorials/web-client/), making a terminal optional.

You can get started by [installing](https://zellij.dev/documentation/installation.html) Zellij and checking out the [Screencasts & Tutorials](https://zellij.dev/screencasts/).

For more details about our future plans, read about upcoming features in our [roadmap](#roadmap).

## Flock: AI-agent status integrations (this fork)

This fork ships the `flock-sidebar` / `flock-selector` plugins, which track AI
coding agents running in your panes and show whether each one is working,
blocked on you, or done. Detection works out of the box from screen content;
for authoritative state, agents can self-report through the bundled hook
integrations under `default-plugins/flock-sidebar/assets/`.

To install the opencode integration:

```sh
cp default-plugins/flock-sidebar/assets/opencode/flock-agent-state.js ~/.config/opencode/plugins/
```

The Claude Code and Codex hook scripts live beside it (`assets/claude/`,
`assets/codex/`) and are wired into each agent's own hook configuration.

### Flock remote development providers

GitHub Codespaces, devcontainers, and Coder are opt-in. This is a breaking
default change: all three integrations are disabled unless their argument is
set to `true` (case-insensitive). When disabled, a provider is not shown or
queried and its remote session bindings are treated as ordinary commands.

Pass the same flags to `flock-selector` and `flock-sidebar` in custom layouts:

```kdl
plugin location="zellij:flock-selector" {
    root_dirs "~/src"
    codespaces_enabled "true"
    devcontainers_enabled "true"
    coder_enabled "true"
    coder_dotfiles_uri "https://github.com/example/dotfiles.git"
    coder_dotfiles_branch "main"
    // coder_dotfiles_parameter "dotfiles_uri"
    // coder_dotfiles_branch_parameter "dotfiles_branch"
    remote_session_layout "~/.config/zellij/layouts/flock-remote.kdl"
}

plugin location="zellij:flock-sidebar" {
    root_dirs "~/src"
    codespaces_enabled "true"
    devcontainers_enabled "true"
    coder_enabled "true"
}
```

`remote_session_layout` supplies the shared base layout for Codespaces,
devcontainer, and Coder sessions. The old `codespace_session_layout` key is
still accepted as a deprecated fallback when `remote_session_layout` is not
present. The selector's built-in generated remote layout forwards all three
enable flags to its sidebar automatically; a custom remote layout must include
the matching sidebar arguments as shown above.

Coder uses the deployment currently authenticated by the Coder CLI. Its tab
lists `coder list --output json`; opening a workspace creates a session whose
default command is `coder ssh owner/name`, and Ctrl-x stops it with
`coder stop -y owner/name`. Run `coder login <url>` before enabling the
integration. In the Coder tab, Ctrl-o opens workspace creation: choose a Coder
template, enter a name, and press Enter to start provisioning in the background.
The selector returns to the workspace list instead of opening the new workspace.

`coder_dotfiles_uri` is an optional, selector-only Git repository URL supplied
at create time through Coder's conventional `dotfiles_uri` template parameter.
Set `coder_dotfiles_branch` to supply the conventional `dotfiles_branch`
parameter too. Override either parameter name with `coder_dotfiles_parameter`
or `coder_dotfiles_branch_parameter`. Dotfiles are only applied when the
selected template exposes these parameters; Flock does not install them after
creation. See Coder's [create parameter defaults](https://coder.com/docs/reference/cli/create)
and [workspace dotfiles guide](https://coder.com/docs/user-guides/workspace-dotfiles).

GitHub Codespaces similarly requires an authenticated `gh` CLI,
and devcontainers require the `devcontainer` CLI and Docker.

## How do I install it?

The easiest way to install Zellij is through a [package for your OS](./docs/THIRD_PARTY_INSTALL.md).

If one is not available for your OS, you could download a prebuilt binary from the [latest release](https://github.com/zellij-org/zellij/releases/latest) and place it in your `$PATH`. If you'd like, we could [automatically choose one for you](#try-zellij-without-installing).

You can also install (compile) with `cargo`:

```
cargo install --locked zellij
```

#### Try Zellij without installing

bash/zsh:
```bash
bash <(curl -L https://zellij.dev/launch)
```
fish/xonsh:
```bash
bash -c 'bash <(curl -L https://zellij.dev/launch)'
```

#### Installing from `main`
Installing Zellij from the `main` branch is not recommended. This branch represents pre-release code, is constantly being worked on and may contain broken or unusable features. In addition, using it may corrupt the cache for future versions, forcing users to clear it before they can use the officially released version.

That being said - no-one will stop you from using it (and bug reports involving new features are greatly appreciated), but please consider using the latest release instead as detailed at the top of this section.

## How do I start a development environment?

* Clone the project
* In the project folder, for debug builds run: `cargo xtask run`
* To run all tests: `cargo xtask test`

For more build commands, see [CONTRIBUTING.md](CONTRIBUTING.md).

## Configuration
For configuring Zellij, please see the [Configuration Documentation](https://zellij.dev/documentation/configuration.html).

## About issues in this repository
Issues in this repository, whether open or closed, do not necessarily indicate a problem or a bug in the software. They only indicate that the reporter wanted to communicate their experiences or thoughts to the maintainers. The Zellij maintainers do their best to go over and reply to all issue reports, but unfortunately cannot promise these will always be dealt with or even read. Your understanding is appreciated.

## Roadmap
Presented here is the project roadmap, divided into three main sections.

These are issues that are either being actively worked on or are planned for the near future.

***If you'll click on the image, you'll be led to an SVG version of it on the website where you can directly click on every issue***

[![roadmap](https://github.com/user-attachments/assets/bb55d213-4a68-4c84-ae72-7db5c9bf94fb)](https://zellij.dev/roadmap)

## Origin of the Name
[From Wikipedia, the free encyclopedia](https://en.wikipedia.org/wiki/Zellij)

Zellij (Arabic: الزليج, romanized: zillīj; also spelled zillij or zellige) is a style of mosaic tilework made from individually hand-chiseled tile pieces. The pieces were typically of different colours and fitted together to form various patterns on the basis of tessellations, most notably elaborate Islamic geometric motifs such as radiating star patterns composed of various polygons. This form of Islamic art is one of the main characteristics of architecture in the western Islamic world. It is found in the architecture of Morocco, the architecture of Algeria, early Islamic sites in Tunisia, and in the historic monuments of al-Andalus (in the Iberian Peninsula).

## License

MIT

## Sponsored by
<a href="https://terminaltrove.com/"><img src="https://avatars.githubusercontent.com/u/121595180?s=200&v=4" width="80px"></a>
