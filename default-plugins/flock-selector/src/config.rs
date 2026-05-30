//! Parsing the plugin's KDL args into a [`SelectorConfig`].
//!
//! The arg shape matches `laperlej/zellij-sessionizer` verbatim so this plugin
//! slots into the user's existing nix config (the `sessionizerRootDirs` /
//! `sessionizerIndividualDirs` options feed it unchanged):
//!
//! - `individual_dirs` — `;`-separated paths; each entry is itself **one**
//!   selectable project.
//! - `root_dirs` — `;`-separated paths; each is **scanned one level deep**, and
//!   every immediate subdirectory becomes a selectable project.
//! - `session_layout` — the layout new project sessions open with (used in
//!   Phase 9; stored now).
//! - `cwd` — base for resolving relative paths (sessionizer passes `cwd = "/"`).
//!
//! Values arrive as the `configuration: BTreeMap<String, String>` zellij hands
//! to `load()` from the layout/keybind `plugin { ... }` body.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Layout new sessions open with when none is configured.
pub const DEFAULT_SESSION_LAYOUT: &str = "default";

/// The selector's resolved configuration, derived from the plugin's KDL args.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorConfig {
    /// Paths that are each themselves one selectable project.
    pub individual_dirs: Vec<PathBuf>,
    /// Paths scanned one level deep — each immediate subdirectory is a project.
    pub root_dirs: Vec<PathBuf>,
    /// Layout new sessions open with (consumed in Phase 9; stored here now so a
    /// reconfigure refresh keeps it current).
    pub session_layout: String,
    /// Base for resolving relative `individual_dirs` / `root_dirs` entries.
    pub cwd: PathBuf,
}

impl Default for SelectorConfig {
    fn default() -> Self {
        Self {
            individual_dirs: Vec::new(),
            root_dirs: Vec::new(),
            session_layout: DEFAULT_SESSION_LAYOUT.to_string(),
            cwd: PathBuf::from("/"),
        }
    }
}

impl SelectorConfig {
    /// Build the config from the plugin's KDL args.
    pub fn from_args(config: &BTreeMap<String, String>) -> Self {
        let cwd = config
            .get("cwd")
            .map(|c| PathBuf::from(c.trim()))
            .filter(|c| !c.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from("/"));
        let session_layout = config
            .get("session_layout")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_SESSION_LAYOUT.to_string());
        SelectorConfig {
            individual_dirs: split_paths(config.get("individual_dirs"), &cwd),
            root_dirs: split_paths(config.get("root_dirs"), &cwd),
            session_layout,
            cwd,
        }
    }
}

/// Split a `;`-separated path list into resolved absolute [`PathBuf`]s, dropping
/// empty entries. Each entry is `~`-expanded and resolved relative to `cwd`.
fn split_paths(raw: Option<&String>, cwd: &Path) -> Vec<PathBuf> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    raw.split(';')
        .filter_map(|entry| resolve_path(entry, cwd))
        .collect()
}

/// Resolve one raw path entry: trim it, expand a leading `~`/`~/`, and join it
/// onto `cwd` if it is still relative. Returns `None` for an empty entry.
pub fn resolve_path(raw: &str, cwd: &Path) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let expanded = expand_home(trimmed);
    if expanded.is_absolute() {
        Some(normalize(&expanded))
    } else {
        Some(normalize(&cwd.join(expanded)))
    }
}

/// Expand a leading `~` (alone or `~/...`) using `$HOME`. Leaves the path
/// untouched when `$HOME` is unset or the path doesn't start with `~`.
fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

/// Drop a trailing slash so paths compare/dedupe consistently (e.g. a scanned
/// subdir and a `workspace_root` both land on the same key). The root `/` is
/// left as-is.
pub fn normalize(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.len() > 1 {
        if let Some(stripped) = s.strip_suffix('/') {
            return PathBuf::from(stripped);
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn defaults_when_empty() {
        let c = SelectorConfig::from_args(&BTreeMap::new());
        assert!(c.individual_dirs.is_empty());
        assert!(c.root_dirs.is_empty());
        assert_eq!(c.session_layout, DEFAULT_SESSION_LAYOUT);
        assert_eq!(c.cwd, PathBuf::from("/"));
    }

    #[test]
    fn splits_and_resolves_absolute_paths() {
        let c = SelectorConfig::from_args(&args(&[
            ("individual_dirs", "/a/project ; /b/other"),
            ("root_dirs", "/work"),
            ("session_layout", "ai"),
        ]));
        assert_eq!(
            c.individual_dirs,
            vec![PathBuf::from("/a/project"), PathBuf::from("/b/other")]
        );
        assert_eq!(c.root_dirs, vec![PathBuf::from("/work")]);
        assert_eq!(c.session_layout, "ai");
    }

    #[test]
    fn drops_empty_entries() {
        let c = SelectorConfig::from_args(&args(&[("root_dirs", "/a;;  ;/b")]));
        assert_eq!(c.root_dirs, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    #[test]
    fn relative_paths_resolve_against_cwd() {
        let c = SelectorConfig::from_args(&args(&[("individual_dirs", "rel/proj"), ("cwd", "/base")]));
        assert_eq!(c.individual_dirs, vec![PathBuf::from("/base/rel/proj")]);
    }

    #[test]
    fn trailing_slash_normalized() {
        assert_eq!(
            resolve_path("/a/b/", &PathBuf::from("/")),
            Some(PathBuf::from("/a/b"))
        );
        assert_eq!(resolve_path("  ", &PathBuf::from("/")), None);
    }

    #[test]
    fn expands_home_with_env() {
        std::env::set_var("HOME", "/home/me");
        assert_eq!(
            resolve_path("~/proj", &PathBuf::from("/")),
            Some(PathBuf::from("/home/me/proj"))
        );
        assert_eq!(
            resolve_path("~", &PathBuf::from("/")),
            Some(PathBuf::from("/home/me"))
        );
    }
}
