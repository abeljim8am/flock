//! Devcontainer support: the binding wrapper + recognizer, `devcontainer` argv
//! builders, `.devcontainer` marker scanning, error classification, and the
//! prompt/starting UI state.
//!
//! A devcontainer binds 1:1 to a zellij session the same way a codespace does
//! (see [`crate::codespaces`]): the session is created from a generated
//! stringified layout whose embedded options set `default_command` to a
//! wrapper that starts (idempotently) and execs into the folder's container,
//! plus `session_serialization false` so a dead bound session is never
//! resurrected. The picker runs `devcontainer up` ONCE before creating the
//! session — that serializes the only racy step (container creation/build);
//! every pane's wrapper `up` after that is a cheap start/attach that also
//! revives a container stopped behind the session's back.
//!
//! The binding argv is `["sh", "-c", WRAPPER_SCRIPT, WRAPPER_ARG0, <path>]`:
//! the script text is a constant and the workspace folder travels as the
//! positional `$1`, so no quoting of the path ever happens and the recognizer
//! ([`parse_devcontainer_command`]) is an exact match, like
//! [`crate::codespaces::parse_codespace_ssh`].
//!
//! Everything here is pure (no host calls) so it unit-tests without a plugin
//! host; [`crate::State`] wires the argv builders into `run_command` and routes
//! the tagged `RunCommandResult`s back through the parsers.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::codespaces::layout_doc_with_binding;
use crate::config::normalize;

/// Context key tagging a `devcontainer up` RunCommandResult; its value is the
/// (normalized) workspace folder the up was fired for.
pub const UP_CONTEXT_KEY: &str = "flock_devcontainer_up";
/// Context key tagging a `.devcontainer` marker scan; its value is the scan
/// scope ([`SCAN_SCOPE_ROOTS`] or [`SCAN_SCOPE_INDIVIDUAL`]).
pub const SCAN_CONTEXT_KEY: &str = "flock_devcontainer_scan";
/// Scan-scope value: markers one level under each project inside the root dirs.
pub const SCAN_SCOPE_ROOTS: &str = "roots";
/// Scan-scope value: markers directly inside each individual dir.
pub const SCAN_SCOPE_INDIVIDUAL: &str = "individual";

/// `$0` of the wrapper — a marker naming the binding so the recognizer can't
/// mistake an arbitrary user `sh -c` for ours.
///
/// The wrapper shape is duplicated in `flock-sidebar/src/devcontainer.rs`
/// (plugins share no crate — see the `normalize` precedent in `config.rs`);
/// the two must stay in sync.
pub const WRAPPER_ARG0: &str = "flock-devcontainer";

/// The constant wrapper script every pane in a bound session runs. `$1` is the
/// workspace folder (see [`wrapper_argv`]) — passed positionally so the script
/// text never embeds the path. `up` is idempotent: with the container already
/// created (the picker guarantees that before the session exists) it is a fast
/// start/attach, and it revives a container stopped behind the session's back.
/// Its stdout is silenced (a JSON success line per pane is noise); stderr is
/// kept so a real failure prints before the pane closes. Before entering the
/// container it copies the host's managed OpenCode state plugin into the
/// remote user's config when that plugin exists. The final exec forwards the
/// pane id and explicitly selects Flock's file bridge; the inner single-quoted
/// `sh -c` runs in the container, where `${SHELL}` (set by the devcontainer
/// CLI's userEnvProbe when available) picks the login shell.
///
/// Duplicated in `flock-sidebar/src/devcontainer.rs`; keep in sync.
pub const WRAPPER_SCRIPT: &str = r#"devcontainer up --workspace-folder "$1" >/dev/null || exit $?; hook="${XDG_CONFIG_HOME:-$HOME/.config}/opencode/plugins/flock-agent-state.js"; if [ -r "$hook" ]; then devcontainer exec --workspace-folder "$1" sh -c 'dir="${XDG_CONFIG_HOME:-$HOME/.config}/opencode/plugins"; mkdir -p "$dir" || exit 1; tmp="$dir/.flock-agent-state.js.tmp.$$"; cat >"$tmp" || exit 1; if cmp -s "$tmp" "$dir/flock-agent-state.js"; then rm -f "$tmp"; else mv -f "$tmp" "$dir/flock-agent-state.js"; fi' <"$hook" || printf '%s\n' 'flock: warning: could not install the OpenCode state plugin in the devcontainer' >&2; fi; exec devcontainer exec --workspace-folder "$1" --remote-env ZELLIJ_PANE_ID="$ZELLIJ_PANE_ID" --remote-env FLOCK_STATE_CHANNEL=file sh -c 'exec "${SHELL:-sh}" -l'"#;

/// The binding argv: what every pane in a bound session runs. Single source of
/// truth for the shape [`parse_devcontainer_command`] recognizes.
pub fn wrapper_argv(workspace_folder: &Path) -> Vec<String> {
    vec![
        "sh".to_owned(),
        "-c".to_owned(),
        WRAPPER_SCRIPT.to_owned(),
        WRAPPER_ARG0.to_owned(),
        workspace_folder.to_string_lossy().to_string(),
    ]
}

/// Recognize a devcontainer binding in a session's `default_command`,
/// returning the workspace folder. Must stay the exact inverse of
/// [`wrapper_argv`]. Duplicated in `flock-sidebar/src/devcontainer.rs`
/// (plugins share no crate); the two must stay in sync.
///
/// The selector itself has no runtime caller: it matches bound sessions by
/// `workspace_root` (a devcontainer session is created with the project as
/// its cwd), so the recognizer lives here as the tested inverse of the shape
/// the sidebar reads.
#[cfg_attr(not(test), allow(dead_code))]
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

/// Argv for the picker's one-time `devcontainer up` (create/build/start —
/// whatever the folder's container needs).
pub fn up_argv(workspace_folder: &Path) -> Vec<String> {
    vec![
        "devcontainer".to_owned(),
        "up".to_owned(),
        "--workspace-folder".to_owned(),
        workspace_folder.to_string_lossy().to_string(),
    ]
}

/// Context map for an up command, carrying the workspace folder.
pub fn up_context(workspace_folder: &Path) -> BTreeMap<String, String> {
    BTreeMap::from_iter([(
        UP_CONTEXT_KEY.to_owned(),
        workspace_folder.to_string_lossy().to_string(),
    )])
}

/// The generated stringified layout a bound session is created from — the same
/// chrome (user `remote_session_layout` base or the built-in flock mirror)
/// as a codespace session, with this binding's wrapper argv appended. See
/// [`crate::codespaces::layout_doc_with_binding`] for the shared mechanics.
pub fn layout_doc_for(
    workspace_folder: &Path,
    sidebar_args: &[(String, String)],
    base_layout: Option<&str>,
) -> String {
    layout_doc_with_binding(&wrapper_argv(workspace_folder), sidebar_args, base_layout)
}

// ---------------------------------------------------------------------------
// .devcontainer marker scanning
// ---------------------------------------------------------------------------

/// Argv scanning every root dir for `.devcontainer` markers one level under
/// each project (`<root>/<project>/.devcontainer{,.json}`). `find` takes all
/// roots in one invocation; the parens are literal argv elements (no shell).
pub fn scan_roots_argv(roots: &[PathBuf]) -> Vec<String> {
    scan_argv_at_depth(roots, "2")
}

/// Argv scanning every individual project dir for markers directly inside it.
pub fn scan_individual_argv(dirs: &[PathBuf]) -> Vec<String> {
    scan_argv_at_depth(dirs, "1")
}

fn scan_argv_at_depth(starts: &[PathBuf], depth: &str) -> Vec<String> {
    let mut argv = vec!["find".to_owned(), "-L".to_owned()];
    argv.extend(starts.iter().map(|p| p.to_string_lossy().to_string()));
    argv.extend(
        [
            "-mindepth", depth, "-maxdepth", depth, "(", "-name", ".devcontainer", "-o", "-name",
            ".devcontainer.json", ")",
        ]
        .into_iter()
        .map(str::to_owned),
    );
    argv
}

/// Context map for a marker scan, carrying its scope.
pub fn scan_context(scope: &str) -> BTreeMap<String, String> {
    BTreeMap::from_iter([(SCAN_CONTEXT_KEY.to_owned(), scope.to_owned())])
}

/// Parse a marker scan's stdout into the set of project folders that contain a
/// `.devcontainer` (each hit's parent, normalized). Callers should parse
/// regardless of the exit code: `find` over several start paths exits nonzero
/// when any one is missing but still prints valid hits for the rest.
pub fn parse_scan_output(stdout: &str) -> HashSet<PathBuf> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| Path::new(l).parent().map(normalize))
        .collect()
}

// ---------------------------------------------------------------------------
// Error classification
// ---------------------------------------------------------------------------

/// Why a `devcontainer up` failed, reduced to the states the UI renders
/// distinctly (each with an actionable hint).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevcontainerError {
    /// The `devcontainer` CLI isn't installed / not on the host PATH.
    CliMissing,
    /// The CLI ran but couldn't reach the docker daemon.
    DockerDown,
    /// Anything else — the last stderr line (builds put the message at the
    /// tail), for display.
    Other(String),
}

/// Classify a failed `devcontainer` run from its exit code and stderr.
pub fn classify_error(exit_code: Option<i32>, stderr: &str) -> DevcontainerError {
    let stderr_lower = stderr.to_lowercase();
    // A missing binary surfaces either as a shell 127 or as the host's direct
    // spawn failure ("No such file or directory (os error 2)", exit 2).
    if exit_code == Some(127)
        || stderr_lower.contains("command not found")
        || stderr_lower.contains("no such file or directory")
    {
        return DevcontainerError::CliMissing;
    }
    if stderr_lower.contains("docker daemon") || stderr_lower.contains("docker.sock") {
        return DevcontainerError::DockerDown;
    }
    let last_line = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    DevcontainerError::Other(last_line.trim().to_owned())
}

// ---------------------------------------------------------------------------
// Prompt / starting UI state
// ---------------------------------------------------------------------------

/// Where the devcontainer flow is, while it owns the picker's keyboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevcontainerPhase {
    /// Asking "start in devcontainer? y/n".
    Prompt,
    /// `devcontainer up` is in flight; the session is created on success.
    Starting,
    /// The up failed; showing the classified error until a key dismisses it.
    Failed(DevcontainerError),
}

/// The project a devcontainer prompt/up is pending for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDevcontainer {
    /// The project folder (normalized) — the wrapper's `$1` and the created
    /// session's cwd.
    pub path: PathBuf,
    /// The folder basename, for the prompt text.
    pub display_name: String,
    pub phase: DevcontainerPhase,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn wrapper_argv_round_trips_through_recognizer() {
        let argv = wrapper_argv(Path::new("/Users/me/my proj"));
        assert_eq!(parse_devcontainer_command(&argv), Some("/Users/me/my proj"));
        assert!(argv[2].contains("opencode/plugins/flock-agent-state.js"));
        assert!(argv[2].contains("--remote-env ZELLIJ_PANE_ID=\"$ZELLIJ_PANE_ID\""));
        assert!(argv[2].contains("--remote-env FLOCK_STATE_CHANNEL=file"));
    }

    #[test]
    fn recognizer_rejects_other_commands() {
        // A user's own sh -c is not the binding.
        assert_eq!(
            parse_devcontainer_command(&to_argv(&["sh", "-c", "echo hi"])),
            None
        );
        // Same shape but a modified script or marker is not the binding.
        assert_eq!(
            parse_devcontainer_command(&to_argv(&[
                "sh",
                "-c",
                "devcontainer up --workspace-folder \"$1\"",
                WRAPPER_ARG0,
                "/p"
            ])),
            None
        );
        assert_eq!(
            parse_devcontainer_command(&to_argv(&[
                "sh",
                "-c",
                WRAPPER_SCRIPT,
                "not-the-marker",
                "/p"
            ])),
            None
        );
        // Extra args and the codespace binding are rejected.
        let mut extra = wrapper_argv(Path::new("/p"));
        extra.push("surplus".to_owned());
        assert_eq!(parse_devcontainer_command(&extra), None);
        assert_eq!(
            parse_devcontainer_command(&to_argv(&["gh", "codespace", "ssh", "-c", "x"])),
            None
        );
        assert_eq!(parse_devcontainer_command(&to_argv(&["fish"])), None);
    }

    #[test]
    fn up_argv_shape() {
        assert_eq!(
            up_argv(Path::new("/work/app")),
            to_argv(&["devcontainer", "up", "--workspace-folder", "/work/app"])
        );
    }

    #[test]
    fn scan_argvs_probe_marker_depths() {
        assert_eq!(
            scan_roots_argv(&[PathBuf::from("/work"), PathBuf::from("/oss")]),
            to_argv(&[
                "find",
                "-L",
                "/work",
                "/oss",
                "-mindepth",
                "2",
                "-maxdepth",
                "2",
                "(",
                "-name",
                ".devcontainer",
                "-o",
                "-name",
                ".devcontainer.json",
                ")",
            ])
        );
        assert_eq!(
            scan_individual_argv(&[PathBuf::from("/solo/proj")]),
            to_argv(&[
                "find",
                "-L",
                "/solo/proj",
                "-mindepth",
                "1",
                "-maxdepth",
                "1",
                "(",
                "-name",
                ".devcontainer",
                "-o",
                "-name",
                ".devcontainer.json",
                ")",
            ])
        );
    }

    #[test]
    fn parse_scan_output_maps_markers_to_projects() {
        let out = "/work/app/.devcontainer\n\n  /work/lib/.devcontainer.json  \n/work/app/.devcontainer\n";
        let parsed = parse_scan_output(out);
        assert_eq!(
            parsed,
            HashSet::from_iter([PathBuf::from("/work/app"), PathBuf::from("/work/lib")])
        );
    }

    #[test]
    fn classifies_cli_missing() {
        // Direct spawn failure of a missing binary (host run-command path).
        assert_eq!(
            classify_error(Some(2), "No such file or directory (os error 2)"),
            DevcontainerError::CliMissing
        );
        assert_eq!(
            classify_error(Some(127), "sh: devcontainer: command not found"),
            DevcontainerError::CliMissing
        );
    }

    #[test]
    fn classifies_docker_down() {
        assert_eq!(
            classify_error(
                Some(1),
                "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?"
            ),
            DevcontainerError::DockerDown
        );
    }

    #[test]
    fn classifies_other_with_last_stderr_line() {
        let stderr = "Step 3/7 : RUN apt-get install nope\nE: Unable to locate package nope\nERROR: build failed\n\n";
        assert_eq!(
            classify_error(Some(1), stderr),
            DevcontainerError::Other("ERROR: build failed".to_owned())
        );
    }

    #[test]
    fn layout_doc_carries_wrapper_binding_and_disables_serialization() {
        let doc = layout_doc_for(Path::new("/work/app"), &[], None);
        assert!(doc.contains("session_serialization false"));
        assert!(doc.starts_with("layout {"));
        assert!(doc.contains(r#""flock-devcontainer" "/work/app""#));
    }

    /// The generated doc must survive the exact parse the server runs on a
    /// stringified layout — including the wrapper script's embedded quotes and
    /// a workspace path with spaces.
    #[test]
    fn layout_doc_parses_with_the_server_side_parser() {
        let path = Path::new("/Users/me/my proj");
        let args = vec![("individual_dirs".to_owned(), "/a;/b".to_owned())];
        let doc = layout_doc_for(path, &args, None);
        let (layout, config) = zellij_utils::input::layout::Layout::from_stringified_layout(
            &doc,
            zellij_utils::input::config::Config::default(),
        )
        .expect("generated layout doc must parse");
        assert!(layout.template.is_some(), "flock chrome should form the tab template");
        assert_eq!(
            config.options.default_command.as_deref(),
            Some(wrapper_argv(path).as_slice())
        );
        assert_eq!(config.options.session_serialization, Some(false));
    }

    #[test]
    fn layout_doc_uses_user_base_layout_when_provided() {
        let base = "layout {\n    pane borderless=true\n}";
        let doc = layout_doc_for(Path::new("/work/app"), &[], Some(base));
        assert!(doc.starts_with("layout {\n    pane borderless=true\n}\n"));
        assert!(doc.contains(r#""flock-devcontainer" "/work/app""#));
        // The built-in mirror must not leak in alongside the user's base.
        assert!(!doc.contains("flock_ui"));
    }
}
