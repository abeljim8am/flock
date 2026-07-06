//! Recognizing codespace-bound sessions and panes.
//!
//! flock-selector creates a session bound to a GitHub codespace by injecting a
//! per-session `default_command` of `gh codespace ssh -c <name>` — every new
//! pane/tab in that session runs the SSH transport instead of a local shell.
//! The sidebar recognizes that binding in two places:
//!
//! - `SessionInfo.default_command` marks a whole session as codespace-bound
//!   (listed in the workspace section even when its `workspace_root` isn't a
//!   configured project folder, and badged);
//! - a pane's argv matching the same shape marks the *pane* as remote: its
//!   local command is just the transport, so agent identity must come from the
//!   rendered screen instead (see `detect::identify_agent_from_screen`).
//!
//! Must stay behaviorally identical to `parse_codespace_ssh` in
//! `flock-selector/src/codespaces.rs` — the selector writes the binding this
//! recognizer reads (plugins share no crate; see the `normalize` precedent in
//! `sessionizer.rs`).

/// Recognize the codespace binding argv (`gh codespace ssh -c <name>`),
/// returning the codespace name.
pub fn parse_codespace_ssh(argv: &[String]) -> Option<&str> {
    match argv {
        [gh, codespace, ssh, dash_c, name]
            if gh == "gh" && codespace == "codespace" && ssh == "ssh" && dash_c == "-c" =>
        {
            Some(name)
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn recognizes_the_binding_shape() {
        assert_eq!(
            parse_codespace_ssh(&to_argv(&["gh", "codespace", "ssh", "-c", "my-cs"])),
            Some("my-cs")
        );
    }

    #[test]
    fn rejects_everything_else() {
        assert_eq!(parse_codespace_ssh(&to_argv(&["gh", "codespace", "list"])), None);
        assert_eq!(parse_codespace_ssh(&to_argv(&["fish"])), None);
        assert_eq!(parse_codespace_ssh(&to_argv(&[])), None);
        assert_eq!(
            parse_codespace_ssh(&to_argv(&["gh", "codespace", "ssh", "-c", "x", "--", "ls"])),
            None
        );
    }
}
