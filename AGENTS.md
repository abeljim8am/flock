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
  native `dev-opt` binary. Later launches reuse it.
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
