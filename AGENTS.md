# Local development

## Interactive testing

- From the repository root, prefer optimized bundled UI plugins:

  ```sh
  cargo xtask run --quick-run -- --session flock-dev
  ```

- Do not use plain `cargo run` for interactive UI testing. It embeds the
  unoptimized `target/wasm32-wasip1/debug` plugins (roughly 80–90 MB each),
  which can make the selector take about 10 seconds to load or process input.
- The first `--quick-run` invocation may spend a minute or two compiling the
  native dev binary with optimized dependencies. Later launches reuse it.
- `--quick-run` uses `zellij-utils/assets/plugins`, so it does not include
  unbuilt plugin source changes. When changing a plugin, build the optimized
  plugin assets first with `cargo xtask build --release --plugins-only`; avoid
  committing regenerated `.wasm` assets unless they are intentionally part of
  the change.

## Session and Coder reconnect testing

- Use a fresh session name when isolating a test from old resurrection
  snapshots.
- `--session NAME` creates a session; it does not attach when that session is
  already live. Reattach to a live session with:

  ```sh
  cargo xtask run --quick-run -- attach NAME
  ```

- To test reconnect, leave the remote application running, detach with
  `Ctrl-o` then `d`, and use the `attach` command above.
- Attaching to a live session keeps its existing server and bridge processes,
  so it does not load newly compiled native changes. To restart that session
  from its saved layout using the new binary, run:

  ```sh
  cargo xtask run --quick-run -- kill-session NAME
  cargo xtask run --quick-run -- attach --force-run-commands NAME
  ```

- Depending on session options, closing the final client may leave the session
  running in the background. Check whether it is live before using
  `--session NAME` again.
- Do not use exiting OpenCode as the reconnect check. The user's default layout
  intentionally launches OpenCode in three tabs and relaunches it after exit.
- Debug native builds automatically use the binary under test for local Coder
  bridge panes through `FLOCK_EXECUTABLE`.
- Set `FLOCK_BUILD_REMOTE_AGENT=1` before `cargo xtask run` only when the remote
  agent itself must be rebuilt and streamed; this requires a supported static
  Linux toolchain.

## Focused verification

- Run `cargo test remote_agent` for remote transport and daemon behavior.
- Run focused `zellij-server` tests for PTY or pane lifecycle changes.
- Unix socket tests can fail with `Operation not permitted` in a restricted
  sandbox; rerun the exact test with the required host permission before
  treating it as a product failure.

## Disk hygiene

`target/` in this repo has reached 300 GB. It grows that way because the build
has several independent axes — profiles (`dev`, `release`), target triples
(host, `wasm32-wasip1`, `x86_64-unknown-linux-musl`), and rustc identity — and
each combination keeps a complete copy of the dependency graph. On top of that,
`zellij-utils` embeds the plugin `.wasm` blobs via `include_dir`, so every one
of the ~90 test binaries carries all of them. Keep the number of axes down.

- **Always build inside the devenv shell.** `devenv hook <shell>` auto-activates
  it on directory change, so a plain `cd` into the repo is enough. Outside it,
  `cargo` resolves to the rustup shim in `~/.nix-profile/bin`, which uses
  `~/.rustup`'s rustc rather than nix's. Both are 1.92.0, but they are different
  binaries with different sysroots, so cargo fingerprints them separately and
  builds a second full tree. This applies to editors too — launch them from an
  activated shell so `rust-analyzer` shares one toolchain.
- **Do not add profiles.** A named profile gets its own directory under
  `target/` and shares nothing with `dev`, even where the settings are
  identical. Dependencies are optimized in `dev` itself
  (`[profile.dev.package."*"] opt-level = 3`); this replaced a separate
  `dev-opt` profile that doubled the tree. If a build needs different codegen
  settings, override in place with `cargo --config` rather than branching to a
  new profile.
- **Keep dev debug info slim.** `[profile.dev] debug = "line-tables-only"` is
  deliberate — full DWARF is the majority of a debug tree here. It still gives
  readable `anyhow` and `insta` backtraces. Raise it locally via
  `cargo --config 'profile.dev.debug=2'` when you actually need lldb, and do not
  commit that.
- **Prefer targeted cleans over `cargo clean`.** A full clean throws away the
  host tree you use every day along with the ones you do not:

  ```sh
  cargo clean --target wasm32-wasip1
  cargo clean --target x86_64-unknown-linux-musl
  cargo clean --release
  cargo clean -p zellij-server   # single crate
  ```

- **Sweep stale artifacts instead of letting them accumulate.** Most of the
  growth is orphaned output from old toolchains and abandoned branches:

  ```sh
  cargo install cargo-sweep
  cargo sweep --installed   # drop artifacts from toolchains no longer present
  cargo sweep -r -t 15 .    # drop anything untouched for 15 days
  ```

- **Do not set `FLOCK_BUILD_REMOTE_AGENT=1` habitually.** It adds the
  `x86_64-unknown-linux-musl` tree, and with `vendored_curl` that means building
  OpenSSL, curl, and libssh2 from source into yet another target directory. Set
  it only when the remote agent itself changed.
- **Agents working in git worktrees:** point `CARGO_TARGET_DIR` at one shared
  path outside the worktree. Otherwise each worktree starts a full tree from
  scratch, which is the fastest way back to 300 GB.
