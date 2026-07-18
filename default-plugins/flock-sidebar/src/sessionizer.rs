//! Sessionizer-style workspace filtering for the sidebar.
//!
//! `flock-selector` and zellij-sessionizer both treat `individual_dirs` as
//! exact projects and `root_dirs` as one-level-deep project collections. The
//! sidebar only needs to decide whether an already-live session belongs to that
//! list, so it can match `SessionInfo.workspace_root` directly instead of
//! running its own directory scan.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionizerConfig {
    individual_dirs: Vec<PathBuf>,
    root_dirs: Vec<PathBuf>,
    codespaces_enabled: bool,
    devcontainers_enabled: bool,
    coder_enabled: bool,
}

impl SessionizerConfig {
    pub fn from_args(config: &BTreeMap<String, String>) -> Self {
        let cwd = config
            .get("cwd")
            .map(|c| PathBuf::from(c.trim()))
            .filter(|c| !c.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from("/"));
        Self {
            individual_dirs: split_paths(config.get("individual_dirs"), &cwd),
            root_dirs: split_paths(config.get("root_dirs"), &cwd),
            codespaces_enabled: enabled(config.get("codespaces_enabled")),
            devcontainers_enabled: enabled(config.get("devcontainers_enabled")),
            coder_enabled: enabled(config.get("coder_enabled")),
        }
    }

    pub fn codespaces_enabled(&self) -> bool {
        self.codespaces_enabled
    }

    pub fn devcontainers_enabled(&self) -> bool {
        self.devcontainers_enabled
    }

    pub fn coder_enabled(&self) -> bool {
        self.coder_enabled
    }

    pub fn is_configured(&self) -> bool {
        !self.individual_dirs.is_empty() || !self.root_dirs.is_empty()
    }

    pub fn contains_workspace(&self, workspace_root: &Path) -> bool {
        let workspace_root = normalize(workspace_root);
        if workspace_root.as_os_str().is_empty() {
            return false;
        }

        self.individual_dirs
            .iter()
            .any(|dir| normalize(dir) == workspace_root)
            || self
                .root_dirs
                .iter()
                .any(|root| is_immediate_root_child(root, &workspace_root))
    }
}

fn enabled(value: Option<&String>) -> bool {
    value.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn split_paths(raw: Option<&String>, cwd: &Path) -> Vec<PathBuf> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    raw.split(';')
        .filter_map(|entry| resolve_path(entry, cwd))
        .collect()
}

fn resolve_path(raw: &str, cwd: &Path) -> Option<PathBuf> {
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

/// Must stay behaviorally identical to `normalize` in
/// `flock-selector/src/config.rs` — see the note there.
fn normalize(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.len() > 1 {
        let stripped = s.trim_end_matches('/');
        if !stripped.is_empty() {
            return PathBuf::from(stripped);
        }
    }
    path.to_path_buf()
}

fn is_immediate_root_child(root: &Path, path: &Path) -> bool {
    if is_hidden_entry(path) {
        return false;
    }
    let root = normalize(root);
    let path = normalize(path);
    path.parent().is_some_and(|parent| normalize(parent) == root)
}

fn is_hidden_entry(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
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
    fn unconfigured_filter_is_detectable() {
        let config = SessionizerConfig::from_args(&BTreeMap::new());
        assert!(!config.is_configured());
        assert!(!config.codespaces_enabled());
        assert!(!config.devcontainers_enabled());
        assert!(!config.coder_enabled());
    }

    #[test]
    fn provider_flags_are_opt_in_true_only() {
        let config = SessionizerConfig::from_args(&args(&[
            ("codespaces_enabled", "TRUE"),
            ("devcontainers_enabled", "yes"),
            ("coder_enabled", " true "),
        ]));
        assert!(config.codespaces_enabled());
        assert!(!config.devcontainers_enabled());
        assert!(config.coder_enabled());
    }

    #[test]
    fn individual_dirs_match_exact_workspace_roots() {
        let config = SessionizerConfig::from_args(&args(&[(
            "individual_dirs",
            "/work/app;/other/tool/",
        )]));
        assert!(config.is_configured());
        assert!(config.contains_workspace(Path::new("/work/app")));
        assert!(config.contains_workspace(Path::new("/other/tool")));
        // Repeated trailing slashes must collapse exactly like the selector's
        // config.rs normalize does, or sessions vanish from one side.
        assert!(config.contains_workspace(Path::new("/work/app//")));
        assert!(!config.contains_workspace(Path::new("/work/app/nested")));
    }

    #[test]
    fn root_dirs_match_only_immediate_visible_children() {
        let config = SessionizerConfig::from_args(&args(&[("root_dirs", "/work;/src/")]));
        assert!(config.contains_workspace(Path::new("/work/app")));
        assert!(config.contains_workspace(Path::new("/src/tool")));
        assert!(!config.contains_workspace(Path::new("/work/app/nested")));
        assert!(!config.contains_workspace(Path::new("/work/.hidden")));
        assert!(!config.contains_workspace(Path::new("/work")));
    }

    #[test]
    fn relative_paths_resolve_against_cwd() {
        let config = SessionizerConfig::from_args(&args(&[
            ("individual_dirs", "app"),
            ("root_dirs", "repos"),
            ("cwd", "/home/me"),
        ]));
        assert!(config.contains_workspace(Path::new("/home/me/app")));
        assert!(config.contains_workspace(Path::new("/home/me/repos/tool")));
        assert!(!config.contains_workspace(Path::new("/app")));
    }

    #[test]
    fn expands_home_paths() {
        std::env::set_var("HOME", "/home/me");
        let config = SessionizerConfig::from_args(&args(&[("individual_dirs", "~/app")]));
        assert!(config.contains_workspace(Path::new("/home/me/app")));
    }
}
