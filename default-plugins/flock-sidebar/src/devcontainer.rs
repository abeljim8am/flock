//! Recognizing devcontainer-bound sessions and panes.
//!
//! flock-selector creates a session bound to a folder's devcontainer by
//! injecting a per-session `default_command` of the unified remote-agent
//! bridge: `flock remote-agent remote-pty --provider devcontainer
//! --workspace-folder <path>`. Like the other bindings, the sidebar
//! recognizes it in two places: the typed `SessionInfo.remote_backend` (or
//! this argv fallback) marks the whole session bound, and a pane argv
//! matching the shape marks the pane remote. Agent state arrives over the
//! in-container daemon's report-state channel, exactly like coder/ssh — the
//! old docker-exec hook polling is gone.
//!
//! Must stay behaviorally identical to `parse_gateway` in
//! `flock-selector/src/devcontainers.rs` — the selector writes the binding
//! this recognizer reads (plugins share no crate; see the `normalize`
//! precedent in `sessionizer.rs`).

use std::path::Path;

/// Recognize the devcontainer binding argv, returning the workspace folder.
pub fn parse_devcontainer_command(argv: &[String]) -> Option<&str> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn to_argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn binding_argv(path: &str) -> Vec<String> {
        to_argv(&[
            "flock",
            "remote-agent",
            "remote-pty",
            "--provider",
            "devcontainer",
            "--workspace-folder",
            path,
        ])
    }

    #[test]
    fn recognizes_the_binding_shape() {
        let argv = binding_argv("/Users/me/my proj");
        assert_eq!(parse_devcontainer_command(&argv), Some("/Users/me/my proj"));
        let mut with_pane = binding_argv("/p");
        with_pane.extend(to_argv(&["--pane-id", "uuid-1"]));
        assert_eq!(parse_devcontainer_command(&with_pane), Some("/p"));
    }

    #[test]
    fn rejects_everything_else() {
        assert_eq!(
            parse_devcontainer_command(&to_argv(&["sh", "-c", "echo hi"])),
            None
        );
        // Other providers on the unified subcommand are not this binding.
        assert_eq!(
            parse_devcontainer_command(&to_argv(&[
                "flock",
                "remote-agent",
                "remote-pty",
                "--provider",
                "coder",
                "--workspace",
                "alice/api",
            ])),
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
}
