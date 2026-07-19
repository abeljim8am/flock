//! Building the candidate project set from the configured folder sources.
//!
//! Two sources feed the candidate list (see [`crate::config`]):
//! - each `individual_dirs` entry is itself one project, and
//! - each `root_dirs` entry is scanned one level deep, every immediate
//!   subdirectory becoming a project.
//!
//! Scanning a root happens off the plugin host's `run_command` (a `find` one
//! level deep) since the roots are arbitrary host paths, not the plugin's `/host`
//! mount. Each scan's `RunCommandResult` is routed back to its root via a context
//! tag, parsed here into subdirectory paths, and merged + de-duplicated with the
//! individual dirs into the final ordered [`Project`] list.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::normalize;

/// Context key tagging a `RunCommandResult` as one of our root scans; its value
/// is the root path the result belongs to.
pub const SCAN_CONTEXT_KEY: &str = "flock_scan_root";

/// A single selectable project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// The project's absolute path (normalized: no trailing slash). The frecency
    /// db key, the session-badge match key against `SessionInfo.workspace_root`,
    /// and the cwd a launched session is rooted in (Phase 9).
    pub path: PathBuf,
    /// The folder basename — the primary label and fuzzy-match target.
    pub name: String,
    /// A home-shortened path (`~/...`) shown dimmed beside the name and used as
    /// the secondary fuzzy-match target.
    pub display_path: String,
}

impl Project {
    /// Build a project from its absolute path, computing the basename label and
    /// the home-shortened display path.
    pub fn from_path(path: &Path) -> Self {
        let path = normalize(path);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        Project {
            display_path: shorten_home(&path),
            name,
            path,
        }
    }
}

/// The argv for scanning one root one level deep. `-mindepth 1 -maxdepth 1
/// -type d` lists immediate subdirectories only; `-L` follows symlinked dirs the
/// way a user expects a project root to behave. Hidden dot-directories are not
/// surfaced as projects when scanning a root.
pub fn scan_argv(root: &str) -> Vec<String> {
    vec![
        "find".to_string(),
        "-L".to_string(),
        root.to_string(),
        "-mindepth".to_string(),
        "1".to_string(),
        "-maxdepth".to_string(),
        "1".to_string(),
        "-type".to_string(),
        "d".to_string(),
        "!".to_string(),
        "-name".to_string(),
        ".*".to_string(),
    ]
}

/// Build the `run_command` context map routing a scan's result back to its root.
pub fn scan_context(root: &str) -> BTreeMap<String, String> {
    let mut ctx = BTreeMap::new();
    ctx.insert(SCAN_CONTEXT_KEY.to_string(), root.to_string());
    ctx
}

/// Parse a `find` scan's stdout into subdirectory paths (one per non-empty line).
pub fn parse_scan_output(stdout: &str) -> Vec<PathBuf> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| normalize(Path::new(l)))
        .filter(|p| !is_hidden_entry(p))
        .collect()
}

/// Merge the individual dirs with every root's scanned subdirs into one ordered,
/// de-duplicated project list. Individual dirs come first (in config order),
/// then scanned subdirs sorted by name for a stable display; duplicates (same
/// normalized path) are dropped, keeping the first occurrence.
pub fn merge_candidates(
    individual_dirs: &[PathBuf],
    scanned: &BTreeMap<String, Vec<PathBuf>>,
) -> Vec<Project> {
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut projects = Vec::new();

    for dir in individual_dirs {
        let p = normalize(dir);
        if seen.insert(p.clone()) {
            projects.push(Project::from_path(&p));
        }
    }

    // Scanned subdirs: flatten across roots, then sort by basename so the list is
    // stable regardless of filesystem iteration order.
    let mut subdirs: Vec<PathBuf> = scanned
        .values()
        .flatten()
        .map(|p| normalize(p))
        .filter(|p| !is_hidden_entry(p))
        .collect();
    subdirs.sort_by(|a, b| {
        let an = a.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        let bn = b.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        an.cmp(&bn).then_with(|| a.cmp(b))
    });
    for dir in subdirs {
        if seen.insert(dir.clone()) {
            projects.push(Project::from_path(&dir));
        }
    }

    projects
}

/// Render `path` with a leading `$HOME` replaced by `~`, for compact display.
pub fn shorten_home(path: &Path) -> String {
    let full = path.to_string_lossy().to_string();
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            if full == home {
                return "~".to_string();
            }
            let with_slash = format!("{}/", home);
            if let Some(rest) = full.strip_prefix(&with_slash) {
                return format!("~/{}", rest);
            }
        }
    }
    full
}

fn is_hidden_entry(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.starts_with('.'))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_basename_and_shortened_path() {
        std::env::set_var("HOME", "/home/me");
        let p = Project::from_path(Path::new("/home/me/code/zellij/"));
        assert_eq!(p.name, "zellij");
        assert_eq!(p.display_path, "~/code/zellij");
        assert_eq!(p.path, PathBuf::from("/home/me/code/zellij"));
    }

    #[test]
    fn parses_find_lines() {
        let out = "/work/a\n/work/b\n\n  /work/c  \n";
        assert_eq!(
            parse_scan_output(out),
            vec![
                PathBuf::from("/work/a"),
                PathBuf::from("/work/b"),
                PathBuf::from("/work/c"),
            ]
        );
    }

    #[test]
    fn parse_scan_output_skips_hidden_dot_directories() {
        let out = "/work/app\n/work/.git\n/work/.config\n/work/lib\n";
        assert_eq!(
            parse_scan_output(out),
            vec![PathBuf::from("/work/app"), PathBuf::from("/work/lib")]
        );
    }

    #[test]
    fn merge_dedupes_and_orders() {
        let individual = vec![PathBuf::from("/solo/proj")];
        let mut scanned = BTreeMap::new();
        scanned.insert(
            "/work".to_string(),
            vec![
                PathBuf::from("/work/zeta"),
                PathBuf::from("/work/.hidden"),
                PathBuf::from("/work/alpha"),
            ],
        );
        // A duplicate of the individual dir surfaced by a scan is dropped.
        scanned.insert("/solo".to_string(), vec![PathBuf::from("/solo/proj")]);

        let merged = merge_candidates(&individual, &scanned);
        let names: Vec<&str> = merged.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["proj", "alpha", "zeta"]);
    }

    #[test]
    fn scan_argv_lists_one_level_deep() {
        assert_eq!(
            scan_argv("/work"),
            vec![
                "find",
                "-L",
                "/work",
                "-mindepth",
                "1",
                "-maxdepth",
                "1",
                "-type",
                "d",
                "!",
                "-name",
                ".*",
            ]
        );
    }
}
