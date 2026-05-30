//! Git branch / ahead-behind probing for workspace headers (Phase 6).
//!
//! Each session now carries a `workspace_root` (the folder it was started in,
//! supplied by the forked server). We group the sidebar by that folder and show
//! the folder's git branch + ahead/behind beside its header. The probe is a
//! single `git status -sb` run via the plugin host's `run_command` against the
//! workspace path; its `## ` header line carries everything we render.

use std::collections::BTreeMap;

/// Context key tagging a `RunCommandResult` as one of our git probes; its value
/// is the workspace path the result belongs to.
pub const GIT_CONTEXT_KEY: &str = "flock_git_path";

/// The argv of the one-shot probe. `git status -sb` prints a `## branch...`
/// header as its first line whether or not there are working-tree changes, and
/// `--porcelain=v1` keeps that header format stable across git versions/locales.
pub const GIT_STATUS_ARGS: &[&str] = &["git", "status", "-sb", "--porcelain=v1"];

/// A workspace's resolved git position, rendered beside its header.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitInfo {
    /// The current branch (or `HEAD` when detached). Empty if unknown.
    pub branch: String,
    /// Commits the branch is ahead of its upstream.
    pub ahead: usize,
    /// Commits the branch is behind its upstream.
    pub behind: usize,
    /// Whether the path is a git repository at all. When false we render
    /// nothing — a folder without a repo just shows its name.
    pub is_repo: bool,
}

/// Parse the first (`## `) line of `git status -sb` output into a [`GitInfo`].
///
/// Handles the common header shapes:
/// - `## main...origin/main` → branch `main`, no ahead/behind
/// - `## main...origin/main [ahead 1, behind 2]` → ahead 1, behind 2
/// - `## main` (no upstream) → branch `main`
/// - `## HEAD (no branch)` (detached) → branch `HEAD`
/// - `## No commits yet on main` (unborn branch) → branch `main`
///
/// Returns `None` if the output has no recognizable header (treated by callers
/// as "not a repo").
pub fn parse_status_branch(stdout: &str) -> Option<GitInfo> {
    let first = stdout.lines().next()?;
    let rest = first.strip_prefix("## ")?;

    let ahead = extract_count(rest, "ahead ");
    let behind = extract_count(rest, "behind ");

    let branch = if let Some(after) = rest.strip_prefix("No commits yet on ") {
        // Unborn branch: the name is everything up to an optional " [".
        branch_head(after)
    } else if rest.starts_with("HEAD (no branch)") {
        "HEAD".to_string()
    } else {
        branch_head(rest)
    };

    Some(GitInfo {
        branch,
        ahead,
        behind,
        is_repo: true,
    })
}

/// The local branch name from a header body: everything before the `...`
/// upstream separator, or before the ` [ahead/behind]` block, or the whole
/// body.
fn branch_head(body: &str) -> String {
    let end = body
        .find("...")
        .or_else(|| body.find(" ["))
        .unwrap_or(body.len());
    body[..end].trim().to_string()
}

/// Pull the integer following `needle` (e.g. `"ahead "`) out of the header,
/// defaulting to 0 when absent.
fn extract_count(haystack: &str, needle: &str) -> usize {
    haystack
        .find(needle)
        .map(|idx| &haystack[idx + needle.len()..])
        .and_then(|tail| {
            let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse().ok()
        })
        .unwrap_or(0)
}

/// Build the `run_command` context map routing a probe's result back to its
/// workspace path.
pub fn git_context(path: &str) -> BTreeMap<String, String> {
    let mut ctx = BTreeMap::new();
    ctx.insert(GIT_CONTEXT_KEY.to_string(), path.to_string());
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clean_upstream_header() {
        let info = parse_status_branch("## main...origin/main\n").unwrap();
        assert_eq!(info.branch, "main");
        assert_eq!((info.ahead, info.behind), (0, 0));
        assert!(info.is_repo);
    }

    #[test]
    fn parses_ahead_and_behind() {
        let info = parse_status_branch("## feat...origin/feat [ahead 3, behind 12]\n M x").unwrap();
        assert_eq!(info.branch, "feat");
        assert_eq!((info.ahead, info.behind), (3, 12));
    }

    #[test]
    fn parses_ahead_only() {
        let info = parse_status_branch("## main...origin/main [ahead 1]").unwrap();
        assert_eq!((info.ahead, info.behind), (1, 0));
    }

    #[test]
    fn parses_branch_without_upstream() {
        let info = parse_status_branch("## local-only").unwrap();
        assert_eq!(info.branch, "local-only");
    }

    #[test]
    fn parses_detached_head() {
        let info = parse_status_branch("## HEAD (no branch)").unwrap();
        assert_eq!(info.branch, "HEAD");
    }

    #[test]
    fn parses_unborn_branch() {
        let info = parse_status_branch("## No commits yet on main").unwrap();
        assert_eq!(info.branch, "main");
    }

    #[test]
    fn rejects_non_header() {
        assert!(parse_status_branch("").is_none());
        assert!(parse_status_branch("fatal: not a git repository").is_none());
    }
}
