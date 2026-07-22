//! Devcontainer support: the remote-agent binding + recognizer,
//! `devcontainer` argv builders, `.devcontainer` marker scanning, error
//! classification, and the prompt/starting/bootstrapping UI state.
//!
//! A devcontainer binds 1:1 to a zellij session on the remote-agent
//! architecture (see [`crate::coder`] and [`crate::ssh`]): the session is
//! created from a generated stringified layout whose embedded options set
//! `default_command` to the unified `remote-pty --provider devcontainer`
//! bridge and carry the typed `remote_backend`, with session serialization ON
//! — panes are persistent PTYs owned by the in-container daemon, so a bound
//! session resurrects and reattaches by stable UUID. The picker runs
//! `devcontainer up` ONCE before creating the session (serializing the only
//! racy step: container creation/build) and then bootstraps the flock binary
//! into the container; the bridge's reconnect loop re-runs `up` on failure,
//! reviving containers stopped behind the session's back.
//!
//! Everything here is pure (no host calls) so it unit-tests without a plugin
//! host; [`crate::State`] wires the argv builders into `run_command` and routes
//! the tagged `RunCommandResult`s back through the parsers.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::config::normalize;
use crate::remote_bootstrap;

/// Context key tagging a `devcontainer up` RunCommandResult; its value is the
/// (normalized) workspace folder the up was fired for.
pub const UP_CONTEXT_KEY: &str = "flock_devcontainer_up";
/// Context key tagging an in-container flock bootstrap RunCommandResult; its
/// value is the (normalized) workspace folder.
pub const BOOTSTRAP_CONTEXT_KEY: &str = "flock_devcontainer_bootstrap";
/// Context key tagging a `.devcontainer` marker scan; its value is the scan
/// scope ([`SCAN_SCOPE_ROOTS`] or [`SCAN_SCOPE_INDIVIDUAL`]).
pub const SCAN_CONTEXT_KEY: &str = "flock_devcontainer_scan";
/// Scan-scope value: markers one level under each project inside the root dirs.
pub const SCAN_SCOPE_ROOTS: &str = "roots";
/// Scan-scope value: markers directly inside each individual dir.
pub const SCAN_SCOPE_INDIVIDUAL: &str = "individual";

/// The binding argv: what every pane in a bound session runs. Single source of
/// truth for the shape [`parse_gateway`] recognizes. The workspace folder
/// travels as one argv element, so paths with spaces never get re-quoted.
pub fn remote_pty_argv(
    workspace_folder: &Path,
    pane_id: Option<&str>,
    executable: Option<&str>,
) -> Vec<String> {
    let mut argv = vec![
        executable.unwrap_or("flock").to_owned(),
        "remote-agent".to_owned(),
        "remote-pty".to_owned(),
        "--provider".to_owned(),
        "devcontainer".to_owned(),
        "--workspace-folder".to_owned(),
        workspace_folder.to_string_lossy().to_string(),
    ];
    if let Some(pane_id) = pane_id {
        argv.extend(["--pane-id".to_owned(), pane_id.to_owned()]);
    }
    argv
}

/// Inverse of [`remote_pty_argv`]: the bound workspace folder, or `None` for
/// anything else. Duplicated in `flock-sidebar/src/devcontainer.rs` (plugins
/// share no crate); the two must stay in sync.
#[cfg_attr(not(test), allow(dead_code))]
pub fn parse_gateway(argv: &[String]) -> Option<&str> {
    match argv {
        [flock, remote_agent, remote_pty, args @ ..]
            if is_flock_executable(flock)
                && remote_agent == "remote-agent"
                && remote_pty == "remote-pty" =>
        {
            let mut chunks = args.chunks_exact(2);
            let mut provider = None;
            let mut workspace_folder = None;
            for chunk in &mut chunks {
                match chunk {
                    [flag, value] if flag == "--provider" && provider.is_none() => {
                        provider = Some(value.as_str());
                    },
                    [flag, value] if flag == "--workspace-folder" && workspace_folder.is_none() => {
                        workspace_folder = Some(value.as_str());
                    },
                    [flag, _] if flag == "--pane-id" || flag == "--cwd" => {},
                    _ => return None,
                }
            }
            if !chunks.remainder().is_empty() || provider != Some("devcontainer") {
                return None;
            }
            workspace_folder.filter(|folder| !folder.is_empty())
        },
        _ => None,
    }
}

fn is_flock_executable(executable: &str) -> bool {
    executable == "flock" || (Path::new(executable).is_absolute() && !executable.is_empty())
}

/// Install the flock binary inside the container using the shared
/// arch-detecting script. `devcontainer exec` passes argv through verbatim
/// (docker-exec style), so the script rides raw — no quote wrapping. Fired on
/// EVERY open (create and switch): container rebuilds wipe the installed
/// binary, and the script's fast path makes the installed case a no-op, so
/// reopening from the picker doubles as the repair action.
pub fn bootstrap_argv(workspace_folder: &Path, debug_binary: Option<&str>) -> Vec<String> {
    if let Some(debug_binary) = debug_binary {
        return debug_bootstrap_argv(workspace_folder, debug_binary);
    }
    vec![
        "devcontainer".to_owned(),
        "exec".to_owned(),
        "--workspace-folder".to_owned(),
        workspace_folder.to_string_lossy().to_string(),
        "sh".to_owned(),
        "-c".to_owned(),
        remote_bootstrap::install_script(),
    ]
}

/// Stream an explicitly selected local binary into the container. The binary
/// path and workspace folder are positional shell arguments, never
/// interpolated into either script.
fn debug_bootstrap_argv(workspace_folder: &Path, debug_binary: &str) -> Vec<String> {
    let local_script = format!(
        r#"set -eu
binary="$1"
workspace_folder="$2"
[ -f "$binary" ] || {{ echo "flock: debug remote agent binary not found: $binary" >&2; exit 66; }}
remote={}
devcontainer exec --workspace-folder "$workspace_folder" sh -c "$remote" < "$binary""#,
        remote_bootstrap::quote_remote_script_arg(&remote_bootstrap::debug_install_script()),
    );
    vec![
        "sh".to_owned(),
        "-c".to_owned(),
        local_script,
        "flock-debug-bootstrap".to_owned(),
        debug_binary.to_owned(),
        workspace_folder.to_string_lossy().to_string(),
    ]
}

/// Context map for a bootstrap command, carrying the workspace folder.
pub fn bootstrap_context(workspace_folder: &Path) -> BTreeMap<String, String> {
    BTreeMap::from_iter([(
        BOOTSTRAP_CONTEXT_KEY.to_owned(),
        workspace_folder.to_string_lossy().to_string(),
    )])
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
/// chrome (user `remote_session_layout` base or the built-in flock mirror) as
/// a coder/ssh session, with the remote-pty binding and the typed
/// `remote_backend`. Serialization is left to the user's local config (server
/// default: on); when on, resurrected panes reattach to the in-container
/// daemon by stable UUID and the bridge revives a stopped container with
/// `devcontainer up` on its reconnect path.
pub fn layout_doc_for(
    workspace_folder: &Path,
    sidebar_args: &[(String, String)],
    base_layout: Option<&str>,
    executable: Option<&str>,
) -> String {
    let command = remote_pty_argv(workspace_folder, None, executable);
    let backend = serde_json::json!({
        "provider": "devcontainer",
        "workspace_folder": workspace_folder.to_string_lossy(),
        "local_session_id": "",
    })
    .to_string();
    let options = format!(
        "remote_backend {}\nshow_startup_tips false\nshow_release_notes false\n",
        kdl_quote(&backend)
    );
    crate::codespaces::layout_doc_with_options(&command, sidebar_args, base_layout, &options)
}

fn kdl_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
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
            "-mindepth",
            depth,
            "-maxdepth",
            depth,
            "(",
            "-name",
            ".devcontainer",
            "-o",
            "-name",
            ".devcontainer.json",
            ")",
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
    /// `devcontainer up` is in flight; the bootstrap follows on success.
    Starting,
    /// The in-container flock bootstrap is in flight; the session is created
    /// on success.
    Bootstrapping,
    /// The up/bootstrap failed; showing the classified error until a key
    /// dismisses it.
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
    fn gateway_binding_round_trips_including_spaced_paths() {
        let path = Path::new("/Users/me/my proj");
        let gateway = remote_pty_argv(path, None, None);
        assert_eq!(
            gateway,
            to_argv(&[
                "flock",
                "remote-agent",
                "remote-pty",
                "--provider",
                "devcontainer",
                "--workspace-folder",
                "/Users/me/my proj",
            ])
        );
        assert_eq!(parse_gateway(&gateway), Some("/Users/me/my proj"));

        let debug_gateway = remote_pty_argv(path, Some("uuid-1"), Some("/tmp/debug/flock"));
        assert_eq!(debug_gateway[0], "/tmp/debug/flock");
        assert_eq!(parse_gateway(&debug_gateway), Some("/Users/me/my proj"));
        let mut with_cwd = debug_gateway.clone();
        with_cwd.extend(["--cwd".to_owned(), "/workspaces/proj".to_owned()]);
        assert_eq!(parse_gateway(&with_cwd), Some("/Users/me/my proj"));
    }

    #[test]
    fn recognizer_rejects_other_commands() {
        assert_eq!(parse_gateway(&to_argv(&["sh", "-c", "echo hi"])), None);
        // Other providers on the unified subcommand are not this binding.
        assert_eq!(
            parse_gateway(&crate::coder::remote_pty_argv("alice/api", None, None)),
            None
        );
        assert_eq!(
            parse_gateway(&to_argv(&["gh", "codespace", "ssh", "-c", "x"])),
            None
        );
        let mut extra = remote_pty_argv(Path::new("/p"), None, None);
        extra.push("surplus".to_owned());
        assert_eq!(parse_gateway(&extra), None);
        assert_eq!(parse_gateway(&to_argv(&["fish"])), None);
    }

    #[test]
    fn bootstrap_execs_raw_arch_detecting_script() {
        let bootstrap = bootstrap_argv(Path::new("/work/my app"), None);
        assert_eq!(
            &bootstrap[..6],
            &[
                "devcontainer",
                "exec",
                "--workspace-folder",
                "/work/my app",
                "sh",
                "-c",
            ]
        );
        let script = bootstrap.last().unwrap();
        // Verbatim argv transport: the script is raw, not quote-wrapped.
        assert!(script.starts_with("set -eu"));
        assert!(script.contains("x86_64-unknown-linux-musl"));
        assert!(script.contains("aarch64-unknown-linux-musl"));
        assert!(!script.contains("/work/my app"));

        let debug = bootstrap_argv(Path::new("/work/app"), Some("/tmp/flock with spaces"));
        assert_eq!(&debug[..2], &["sh", "-c"]);
        assert_eq!(debug[3], "flock-debug-bootstrap");
        assert_eq!(debug[4], "/tmp/flock with spaces");
        assert_eq!(debug[5], "/work/app");
        assert!(debug[2].contains("devcontainer exec --workspace-folder \"$workspace_folder\""));
        assert!(debug[2].contains("< \"$binary\""));
        assert!(!debug[2].contains("/tmp/flock with spaces"));
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

    /// The generated doc must survive the exact parse the server runs on a
    /// stringified layout — including a workspace path with spaces — and must
    /// carry the typed backend without forcing serialization (the user's
    /// local config decides).
    #[test]
    fn layout_doc_parses_with_the_server_side_parser() {
        let path = Path::new("/Users/me/my proj");
        let args = vec![("individual_dirs".to_owned(), "/a;/b".to_owned())];
        let doc = layout_doc_for(path, &args, None, None);
        let (layout, config) = zellij_utils::input::layout::Layout::from_stringified_layout(
            &doc,
            zellij_utils::input::config::Config::default(),
        )
        .expect("generated layout doc must parse");
        assert!(
            layout.template.is_some(),
            "flock chrome should form the tab template"
        );
        assert_eq!(
            config.options.default_command.as_deref(),
            Some(remote_pty_argv(path, None, None).as_slice())
        );
        assert!(matches!(
            config.options.remote_backend,
            Some(zellij_tile::prelude::RemoteBackend::Devcontainer {
                ref workspace_folder,
                ..
            }) if workspace_folder == "/Users/me/my proj"
        ));
        assert_eq!(config.options.session_serialization, None);
    }

    #[test]
    fn layout_doc_uses_user_base_layout_when_provided() {
        let base = "layout {\n    pane borderless=true\n}";
        let doc = layout_doc_for(Path::new("/work/app"), &[], Some(base), None);
        assert!(doc.starts_with("layout {\n    pane borderless=true\n}\n"));
        assert!(doc.contains(r#""--workspace-folder" "/work/app""#));
        // The built-in mirror must not leak in alongside the user's base.
        assert!(!doc.contains("flock_ui"));
    }
}
