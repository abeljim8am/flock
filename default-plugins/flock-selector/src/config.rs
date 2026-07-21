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
pub const DEFAULT_CODER_DOTFILES_PARAMETER: &str = "dotfiles_uri";
pub const DEFAULT_CODER_DOTFILES_BRANCH_PARAMETER: &str = "dotfiles_branch";

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
    /// Fixed name to rename our own session to on load. Set by the cold-shell
    /// `flock-selector` layout so the picker's throwaway entry session always
    /// has the same stable name (which the sidebar hides) instead of a random
    /// one. Left `None` for a keybind launch, where renaming the user's working
    /// session would be wrong.
    pub session_name: Option<String>,
    /// The raw arg pairs the flock-sidebar shares with this plugin, exactly as
    /// received. Generated remote-session layouts embed them into sidebar plugin
    /// blocks so the spawned sidebar filters workspaces the same way the
    /// user's regular flock sessions do.
    pub sidebar_args: Vec<(String, String)>,
    /// Opt-in provider flags. Only a case-insensitive `true` enables one.
    pub codespaces_enabled: bool,
    pub devcontainers_enabled: bool,
    pub coder_enabled: bool,
    pub ssh_enabled: bool,
    /// Optional repository injected as a Coder template parameter default when
    /// creating workspaces. This is selector-only: the sidebar never creates
    /// workspaces.
    pub coder_dotfiles_uri: Option<String>,
    /// Template parameter receiving [`Self::coder_dotfiles_uri`].
    pub coder_dotfiles_parameter: String,
    /// Optional branch injected alongside the dotfiles repository.
    pub coder_dotfiles_branch: Option<String>,
    /// Template parameter receiving [`Self::coder_dotfiles_branch`].
    pub coder_dotfiles_branch_parameter: String,
    /// Path to a layout file used as the base for all remote-bound sessions
    /// (`remote_session_layout` arg; `~`-expanded). The deprecated
    /// `codespace_session_layout` is used only when the new key is absent.
    /// When set and readable,
    /// bound sessions get this layout's chrome with the SSH binding options
    /// appended, instead of the built-in flock mirror. The file's content
    /// panes must NOT carry explicit `command`s — an explicit command
    /// overrides the session's `default_command`, so such a pane would open
    /// locally instead of SSHing into the codespace.
    pub remote_session_layout: Option<PathBuf>,
}

impl Default for SelectorConfig {
    fn default() -> Self {
        Self {
            individual_dirs: Vec::new(),
            root_dirs: Vec::new(),
            session_layout: DEFAULT_SESSION_LAYOUT.to_string(),
            cwd: PathBuf::from("/"),
            session_name: None,
            sidebar_args: Vec::new(),
            codespaces_enabled: false,
            devcontainers_enabled: false,
            coder_enabled: false,
            ssh_enabled: false,
            coder_dotfiles_uri: None,
            coder_dotfiles_parameter: DEFAULT_CODER_DOTFILES_PARAMETER.to_owned(),
            coder_dotfiles_branch: None,
            coder_dotfiles_branch_parameter: DEFAULT_CODER_DOTFILES_BRANCH_PARAMETER.to_owned(),
            remote_session_layout: None,
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
        let session_name = config
            .get("session_name")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let sidebar_args = [
            "individual_dirs",
            "root_dirs",
            "cwd",
            "codespaces_enabled",
            "devcontainers_enabled",
            "coder_enabled",
            "ssh_enabled",
        ]
        .iter()
        .filter_map(|key| {
            config
                .get(*key)
                .filter(|value| !value.trim().is_empty())
                .map(|value| (key.to_string(), value.clone()))
        })
        .collect();
        let remote_session_layout = config
            .get("remote_session_layout")
            .or_else(|| config.get("codespace_session_layout"))
            .and_then(|raw| resolve_path(raw, &cwd));
        SelectorConfig {
            individual_dirs: split_paths(config.get("individual_dirs"), &cwd),
            root_dirs: split_paths(config.get("root_dirs"), &cwd),
            session_layout,
            cwd,
            session_name,
            sidebar_args,
            codespaces_enabled: enabled(config.get("codespaces_enabled")),
            devcontainers_enabled: enabled(config.get("devcontainers_enabled")),
            coder_enabled: enabled(config.get("coder_enabled")),
            ssh_enabled: enabled(config.get("ssh_enabled")),
            coder_dotfiles_uri: config
                .get("coder_dotfiles_uri")
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty()),
            coder_dotfiles_parameter: config
                .get("coder_dotfiles_parameter")
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| DEFAULT_CODER_DOTFILES_PARAMETER.to_owned()),
            coder_dotfiles_branch: config
                .get("coder_dotfiles_branch")
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty()),
            coder_dotfiles_branch_parameter: config
                .get("coder_dotfiles_branch_parameter")
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| DEFAULT_CODER_DOTFILES_BRANCH_PARAMETER.to_owned()),
            remote_session_layout,
        }
    }
}

fn enabled(value: Option<&String>) -> bool {
    value.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
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

/// Drop trailing slashes so paths compare/dedupe consistently (e.g. a scanned
/// subdir and a `workspace_root` both land on the same key). The root `/` is
/// left as-is.
///
/// Must stay behaviorally identical to `normalize` in
/// `flock-sidebar/src/sessionizer.rs`: the sidebar filters sessions the
/// selector creates by comparing the same configured paths, so any
/// divergence makes sessions silently disappear from one side.
pub fn normalize(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.len() > 1 {
        let stripped = s.trim_end_matches('/');
        if !stripped.is_empty() {
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
        assert_eq!(c.session_name, None);
        assert!(!c.codespaces_enabled);
        assert!(!c.devcontainers_enabled);
        assert!(!c.coder_enabled);
        assert_eq!(c.coder_dotfiles_uri, None);
        assert_eq!(c.coder_dotfiles_parameter, DEFAULT_CODER_DOTFILES_PARAMETER);
        assert_eq!(c.coder_dotfiles_branch, None);
        assert_eq!(
            c.coder_dotfiles_branch_parameter,
            DEFAULT_CODER_DOTFILES_BRANCH_PARAMETER
        );
        assert_eq!(c.remote_session_layout, None);
    }

    #[test]
    fn provider_flags_only_accept_true_case_insensitively() {
        let c = SelectorConfig::from_args(&args(&[
            ("codespaces_enabled", " TRUE "),
            ("devcontainers_enabled", "1"),
            ("coder_enabled", "TrUe"),
        ]));
        assert!(c.codespaces_enabled);
        assert!(!c.devcontainers_enabled);
        assert!(c.coder_enabled);
    }

    #[test]
    fn coder_dotfiles_settings_trim_and_default_parameter() {
        let configured = SelectorConfig::from_args(&args(&[(
            "coder_dotfiles_uri",
            "  https://example.test/my dotfiles.git  ",
        )]));
        assert_eq!(
            configured.coder_dotfiles_uri.as_deref(),
            Some("https://example.test/my dotfiles.git")
        );
        assert_eq!(configured.coder_dotfiles_parameter, "dotfiles_uri");

        let custom = SelectorConfig::from_args(&args(&[
            ("coder_dotfiles_uri", "https://example.test/dotfiles"),
            ("coder_dotfiles_parameter", "  personal_dotfiles  "),
            ("coder_dotfiles_branch", "  main  "),
            ("coder_dotfiles_branch_parameter", "  personal_branch  "),
        ]));
        assert_eq!(custom.coder_dotfiles_parameter, "personal_dotfiles");
        assert_eq!(custom.coder_dotfiles_branch.as_deref(), Some("main"));
        assert_eq!(custom.coder_dotfiles_branch_parameter, "personal_branch");
    }

    #[test]
    fn remote_layout_precedes_deprecated_codespace_layout() {
        let c = SelectorConfig::from_args(&args(&[
            ("remote_session_layout", "remote.kdl"),
            ("codespace_session_layout", "legacy.kdl"),
            ("cwd", "/config"),
        ]));
        assert_eq!(
            c.remote_session_layout,
            Some(PathBuf::from("/config/remote.kdl"))
        );

        let legacy = SelectorConfig::from_args(&args(&[
            ("codespace_session_layout", "legacy.kdl"),
            ("cwd", "/config"),
        ]));
        assert_eq!(
            legacy.remote_session_layout,
            Some(PathBuf::from("/config/legacy.kdl"))
        );
    }

    #[test]
    fn forwards_provider_flags_to_generated_sidebars() {
        let c = SelectorConfig::from_args(&args(&[
            ("codespaces_enabled", "true"),
            ("devcontainers_enabled", "false"),
            ("coder_enabled", "TRUE"),
        ]));
        assert!(c
            .sidebar_args
            .contains(&("codespaces_enabled".into(), "true".into())));
        assert!(c
            .sidebar_args
            .contains(&("devcontainers_enabled".into(), "false".into())));
        assert!(c
            .sidebar_args
            .contains(&("coder_enabled".into(), "TRUE".into())));
    }

    #[test]
    fn session_name_parsed_when_set_and_none_when_blank() {
        let c = SelectorConfig::from_args(&args(&[("session_name", "  flock-selector ")]));
        assert_eq!(c.session_name.as_deref(), Some("flock-selector"));
        let blank = SelectorConfig::from_args(&args(&[("session_name", "   ")]));
        assert_eq!(blank.session_name, None);
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
        let c =
            SelectorConfig::from_args(&args(&[("individual_dirs", "rel/proj"), ("cwd", "/base")]));
        assert_eq!(c.individual_dirs, vec![PathBuf::from("/base/rel/proj")]);
    }

    #[test]
    fn trailing_slash_normalized() {
        assert_eq!(
            resolve_path("/a/b/", &PathBuf::from("/")),
            Some(PathBuf::from("/a/b"))
        );
        // Repeated trailing slashes must collapse too — the sidebar's
        // sessionizer normalizes the same way, and the two must agree.
        assert_eq!(
            resolve_path("/a/b//", &PathBuf::from("/")),
            Some(PathBuf::from("/a/b"))
        );
        assert_eq!(normalize(Path::new("/")), PathBuf::from("/"));
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
