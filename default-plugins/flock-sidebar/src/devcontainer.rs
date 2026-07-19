//! Recognizing devcontainer-bound sessions and panes.
//!
//! flock-selector creates a session bound to a folder's devcontainer by
//! injecting a per-session `default_command` of
//! `["sh", "-c", WRAPPER_SCRIPT, WRAPPER_ARG0, <workspace folder>]` — a
//! constant wrapper script that (idempotently) starts the folder's container
//! and execs into it, with the folder travelling as the positional `$1`. Like
//! the codespace binding (see `codespace.rs`), the sidebar recognizes it in
//! two places: `SessionInfo.default_command` marks the whole session bound,
//! and a pane argv matching the shape marks the pane remote (agent identity
//! must come from the rendered screen — the container's processes live in
//! another PID namespace, invisible to the host's process walk).
//!
//! Must stay behaviorally identical to `parse_devcontainer_command` in
//! `flock-selector/src/devcontainers.rs` — the selector writes the binding
//! this recognizer reads (plugins share no crate; see the `normalize`
//! precedent in `sessionizer.rs`).

/// `$0` of the wrapper — a marker naming the binding so the recognizer can't
/// mistake an arbitrary user `sh -c` for ours. Keep in sync with
/// `flock-selector/src/devcontainers.rs`.
pub const WRAPPER_ARG0: &str = "flock-devcontainer";

/// The constant wrapper script every pane in a bound session runs. It copies
/// the host's managed OpenCode state plugin into the remote user's config when
/// present, then forwards the pane id and selects the file bridge explicitly.
/// Keep in sync with `flock-selector/src/devcontainers.rs`.
pub const WRAPPER_SCRIPT: &str = r#"devcontainer up --workspace-folder "$1" >/dev/null || exit $?; hook="${XDG_CONFIG_HOME:-$HOME/.config}/opencode/plugins/flock-agent-state.js"; if [ -r "$hook" ]; then devcontainer exec --workspace-folder "$1" sh -c 'dir="${XDG_CONFIG_HOME:-$HOME/.config}/opencode/plugins"; mkdir -p "$dir" || exit 1; tmp="$dir/.flock-agent-state.js.tmp.$$"; cat >"$tmp" || exit 1; if cmp -s "$tmp" "$dir/flock-agent-state.js"; then rm -f "$tmp"; else mv -f "$tmp" "$dir/flock-agent-state.js"; fi' <"$hook" || printf '%s\n' 'flock: warning: could not install the OpenCode state plugin in the devcontainer' >&2; fi; exec devcontainer exec --workspace-folder "$1" --remote-env ZELLIJ_PANE_ID="$ZELLIJ_PANE_ID" --remote-env FLOCK_STATE_CHANNEL=file sh -c 'exec "${SHELL:-sh}" -l'"#;

/// Recognize the devcontainer binding argv, returning the workspace folder.
pub fn parse_devcontainer_command(argv: &[String]) -> Option<&str> {
    match argv {
        [sh, dash_c, script, arg0, path]
            if sh == "sh" && dash_c == "-c" && script == WRAPPER_SCRIPT && arg0 == WRAPPER_ARG0 =>
        {
            Some(path)
        },
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// In-container hook bridge (the opencode integration's file channel)
// ---------------------------------------------------------------------------
//
// Hooks are the only detection path for opencode (screen identification knows
// only Claude/Codex chrome), and inside a container `zellij pipe` can't work:
// no zellij binary, no server socket. So the opencode integration
// (`assets/opencode/flock-agent-state.js`) falls back to writing its reports
// to `/tmp/flock-state/pane-<id>` *inside* the container — one line in the
// same `key=value` format as the pipe args, plus `ts=<epoch secs>` — and the
// sidebar polls those files from the host: resolve the container by its
// devcontainer label, `docker exec … cat` the files, and feed each fresh line
// through the ordinary hook parser (`hook::parse_hook_report` ignores the
// extra `ts`/`source` keys).

/// Context key tagging the container-id lookup (`docker ps`) result; its value
/// is the workspace folder it was resolved for.
pub const PS_CONTEXT_KEY: &str = "flock_devcontainer_ps";
/// Context key tagging the hook-file read (`docker exec … cat`) result.
pub const HOOKS_CONTEXT_KEY: &str = "flock_devcontainer_hooks";

/// A non-`idle` report older than this is ignored: a hard-killed agent can't
/// retract its last "working"/"blocked" file, so stale urgency must expire.
/// (`idle` is a safe resting state and ages without harm.)
pub const HOOK_STALE_SECS: u64 = 600;

/// Argv resolving the workspace folder's container id. Both the devcontainer
/// CLI and VS Code label containers with the host-side workspace path.
pub fn ps_argv(workspace_folder: &str) -> Vec<String> {
    vec![
        "docker".to_owned(),
        "ps".to_owned(),
        "-q".to_owned(),
        "--filter".to_owned(),
        format!("label=devcontainer.local_folder={}", workspace_folder),
    ]
}

/// Argv dumping every pane state file inside the container. The glob expands
/// in the *container's* shell; a missing dir/no files is silenced (exit 1 with
/// empty stderr — distinct from docker's own container-gone errors). Reads the
/// files the opencode integration writes; keep the path in sync with
/// `STATE_DIR` in `assets/opencode/flock-agent-state.js`.
pub fn hooks_cat_argv(container_id: &str) -> Vec<String> {
    vec![
        "docker".to_owned(),
        "exec".to_owned(),
        container_id.to_owned(),
        "sh".to_owned(),
        "-c".to_owned(),
        "cat /tmp/flock-state/pane-* 2>/dev/null".to_owned(),
    ]
}

/// Split hook-file contents (one report line per file, concatenated by `cat`)
/// into per-report `key=value` maps, ready for `hook::parse_hook_report`.
/// Malformed pairs within a line are skipped rather than sinking the line.
pub fn parse_state_lines(stdout: &str) -> Vec<std::collections::BTreeMap<String, String>> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.split(',')
                .filter_map(|pair| {
                    let (key, value) = pair.split_once('=')?;
                    let key = key.trim();
                    if key.is_empty() {
                        return None;
                    }
                    Some((key.to_owned(), value.trim().to_owned()))
                })
                .collect()
        })
        .collect()
}

/// Whether a parsed report is fresh enough to apply. Reports without a
/// parseable `ts` are accepted (the pipe channel has no timestamp either);
/// `idle` never expires; anything else must be younger than
/// [`HOOK_STALE_SECS`].
pub fn report_is_fresh(
    args: &std::collections::BTreeMap<String, String>,
    now_epoch_secs: u64,
) -> bool {
    let Some(ts) = args
        .get("ts")
        .and_then(|raw| raw.trim().parse::<u64>().ok())
    else {
        return true;
    };
    if args
        .get("state")
        .is_some_and(|state| state.eq_ignore_ascii_case("idle"))
    {
        return true;
    }
    now_epoch_secs.saturating_sub(ts) <= HOOK_STALE_SECS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn binding_argv(path: &str) -> Vec<String> {
        to_argv(&["sh", "-c", WRAPPER_SCRIPT, WRAPPER_ARG0, path])
    }

    #[test]
    fn recognizes_the_binding_shape() {
        let argv = binding_argv("/Users/me/my proj");
        assert_eq!(parse_devcontainer_command(&argv), Some("/Users/me/my proj"));
        assert!(argv[2].contains("opencode/plugins/flock-agent-state.js"));
        assert!(argv[2].contains("--remote-env ZELLIJ_PANE_ID=\"$ZELLIJ_PANE_ID\""));
        assert!(argv[2].contains("--remote-env FLOCK_STATE_CHANNEL=file"));
    }

    #[test]
    fn rejects_everything_else() {
        assert_eq!(
            parse_devcontainer_command(&to_argv(&["sh", "-c", "echo hi"])),
            None
        );
        assert_eq!(
            parse_devcontainer_command(&to_argv(&[
                "sh",
                "-c",
                "devcontainer exec --workspace-folder \"$1\"",
                WRAPPER_ARG0,
                "/p"
            ])),
            None
        );
        assert_eq!(
            parse_devcontainer_command(&to_argv(&["sh", "-c", WRAPPER_SCRIPT, "other", "/p"])),
            None
        );
        let mut extra = binding_argv("/p");
        extra.push("surplus".to_owned());
        assert_eq!(parse_devcontainer_command(&extra), None);
        assert_eq!(
            parse_devcontainer_command(&to_argv(&["gh", "codespace", "ssh", "-c", "x"])),
            None
        );
        assert_eq!(parse_devcontainer_command(&to_argv(&[])), None);
    }

    #[test]
    fn poll_argvs_target_the_labelled_container() {
        assert_eq!(
            ps_argv("/work/my proj"),
            to_argv(&[
                "docker",
                "ps",
                "-q",
                "--filter",
                "label=devcontainer.local_folder=/work/my proj",
            ])
        );
        assert_eq!(
            hooks_cat_argv("abc123"),
            to_argv(&[
                "docker",
                "exec",
                "abc123",
                "sh",
                "-c",
                "cat /tmp/flock-state/pane-* 2>/dev/null",
            ])
        );
    }

    #[test]
    fn parses_state_lines_into_report_maps() {
        let stdout = "pane_id=3,state=working,agent=opencode,source=flock:opencode,ts=100\n\
                      \n\
                      pane_id=5,state=idle,agent=opencode,malformed,ts=90\n";
        let maps = parse_state_lines(stdout);
        assert_eq!(maps.len(), 2);
        assert_eq!(maps[0].get("pane_id").map(String::as_str), Some("3"));
        assert_eq!(maps[0].get("state").map(String::as_str), Some("working"));
        assert_eq!(maps[0].get("ts").map(String::as_str), Some("100"));
        // The malformed pair is dropped; the rest of the line survives.
        assert_eq!(maps[1].get("pane_id").map(String::as_str), Some("5"));
        assert_eq!(maps[1].len(), 4);
    }

    #[test]
    fn freshness_expires_stale_urgency_but_not_idle() {
        let line = |state: &str, ts: u64| {
            parse_state_lines(&format!("pane_id=1,state={},ts={}", state, ts))
                .pop()
                .unwrap()
        };
        let now = 10_000;
        assert!(report_is_fresh(&line("working", now - 5), now));
        assert!(!report_is_fresh(
            &line("working", now - HOOK_STALE_SECS - 1),
            now
        ));
        assert!(!report_is_fresh(
            &line("blocked", now - HOOK_STALE_SECS - 1),
            now
        ));
        // Idle is a resting state — it never expires.
        assert!(report_is_fresh(
            &line("idle", now - HOOK_STALE_SECS - 1),
            now
        ));
        // No parseable ts → accepted, like the pipe channel.
        let no_ts = parse_state_lines("pane_id=1,state=working").pop().unwrap();
        assert!(report_is_fresh(&no_ts, now));
    }
}
