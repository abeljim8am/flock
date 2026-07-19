//! GitHub Codespaces support: the data model, `gh` argv builders, list-output
//! parsing, error classification, ranking, and the switch-or-create resolution
//! for codespace-bound sessions.
//!
//! A codespace binds 1:1 to a zellij session through the session's
//! `default_command` (the fork's per-session option): a bound session is
//! created from a generated stringified layout whose embedded options set
//! `default_command "gh" "codespace" "ssh" "-c" "<name>"` — so every new pane
//! and tab SSHes into the codespace — and `session_serialization false` — so a
//! dead bound session can never be resurrected (reconnecting always goes back
//! through the picker, which re-establishes the binding). The binding is
//! recognized back out of `SessionInfo.default_command` by
//! [`parse_codespace_ssh`]; it does not depend on the session's name.
//!
//! Everything here is pure (no host calls) so it unit-tests without a plugin
//! host; [`crate::State`] wires the argv builders into `run_command` and routes
//! the tagged `RunCommandResult`s back through the parsers.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use crate::fuzzy::fuzzy_match;

/// Context key tagging the `gh codespace list` RunCommandResult.
pub const LIST_CONTEXT_KEY: &str = "flock_codespace_list";
/// Context key tagging a `gh codespace stop` RunCommandResult; its value is the
/// codespace name being stopped.
pub const STOP_CONTEXT_KEY: &str = "flock_codespace_stop";
/// Context key tagging the `cat <remote_session_layout>` RunCommandResult that
/// loads the user's shared remote layout base.
pub const LAYOUT_CONTEXT_KEY: &str = "flock_codespace_layout";

/// Where the last successful list is cached inside the plugin's `/data` mount,
/// so the section renders instantly on open while a live refresh runs.
const CACHE_PATH: &str = "/data/codespaces.json";

/// Fallback session name when a codespace yields no usable characters.
const FALLBACK_NAME: &str = "codespace";

/// One codespace, reduced to the fields the picker needs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Codespace {
    /// The globally-unique GitHub codespace name (the `-c` argument).
    pub name: String,
    /// The user-facing display name; falls back to `name` when unset.
    pub display_name: String,
    /// The repository the codespace belongs to (`owner/repo`).
    pub repository: String,
    /// The raw state string as reported by `gh` (e.g. "Available", "Shutdown").
    pub state: String,
    /// RFC3339 last-used timestamp; used as a recency tiebreak (RFC3339 in a
    /// single timezone orders correctly as a string).
    pub last_used_at: String,
}

/// A coarse view of the state string, for badge rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateKind {
    /// Ready to connect ("Available").
    Running,
    /// Stopped; `gh codespace ssh` will boot it ("Shutdown" and friends).
    Stopped,
    /// In flux ("Starting", "Provisioning", "ShuttingDown", ...).
    Busy,
    Unknown,
}

impl Codespace {
    pub fn state_kind(&self) -> StateKind {
        match self.state.as_str() {
            "Available" => StateKind::Running,
            "Shutdown" | "Created" | "Archived" => StateKind::Stopped,
            "Starting" | "Provisioning" | "Queued" | "Awaiting" | "Rebuilding" | "ShuttingDown"
            | "Exporting" | "Updating" => StateKind::Busy,
            _ => StateKind::Unknown,
        }
    }
}

// ---------------------------------------------------------------------------
// gh argv builders + the binding recognizer
// ---------------------------------------------------------------------------

/// Argv for listing codespaces as JSON.
pub fn list_argv() -> Vec<String> {
    vec![
        "gh".to_owned(),
        "codespace".to_owned(),
        "list".to_owned(),
        "--json".to_owned(),
        "name,displayName,repository,state,lastUsedAt".to_owned(),
    ]
}

/// Argv for stopping a codespace.
pub fn stop_argv(codespace_name: &str) -> Vec<String> {
    vec![
        "gh".to_owned(),
        "codespace".to_owned(),
        "stop".to_owned(),
        "-c".to_owned(),
        codespace_name.to_owned(),
    ]
}

/// The binding argv: what every pane in a bound session runs. Single source of
/// truth for the shape [`parse_codespace_ssh`] recognizes.
pub fn ssh_argv(codespace_name: &str) -> Vec<String> {
    vec![
        "gh".to_owned(),
        "codespace".to_owned(),
        "ssh".to_owned(),
        "-c".to_owned(),
        codespace_name.to_owned(),
    ]
}

/// Recognize a codespace binding in a session's `default_command`, returning
/// the codespace name. Must stay the exact inverse of [`ssh_argv`]. The
/// flock-sidebar duplicates this recognizer (plugins share no crate — see the
/// `normalize` precedent in `config.rs`); the two must stay in sync.
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

/// The flock session chrome, mirrored from the bundled flock layout so a
/// codespace-bound session looks and behaves exactly like a regular flock
/// session: tab-bar, docked sidebar, status-bar, and the flock_ui swap
/// layouts (which give new tabs and multi-pane arrangements the same shape).
/// `__SIDEBAR_25__` / `__SIDEBAR_40__` are replaced with sidebar plugin
/// blocks carrying this plugin's own dir args, so the spawned sidebar filters
/// workspaces like the user's other sessions do.
///
/// Must stay structurally in sync with
/// `zellij-utils/assets/layouts/flock.kdl` + `flock.swap.kdl` (the swap
/// file's contents are inlined here because a stringified layout has no
/// sibling `.swap.kdl` to pick up).
const FLOCK_LAYOUT_TEMPLATE: &str = r#"layout {
    pane size=1 borderless=true {
        plugin location="tab-bar"
    }
    pane split_direction="Vertical" {
        pane size="25%" borderless=true {
            __SIDEBAR_25__
        }
        pane
    }
    pane size=2 borderless=true {
        plugin location="status-bar"
    }
    tab_template name="flock_ui" {
        pane size=1 borderless=true {
            plugin location="tab-bar"
        }
        pane split_direction="vertical" {
            pane size=40 borderless=true {
                __SIDEBAR_40__
            }
            children
        }
        pane size=2 borderless=true {
            plugin location="status-bar"
        }
    }
    swap_tiled_layout name="vertical" {
        flock_ui max_panes=6 {
            pane split_direction="vertical" {
                pane
                pane { children; }
            }
        }
        flock_ui max_panes=9 {
            pane split_direction="vertical" {
                pane { children; }
                pane { pane; pane; pane; pane; }
            }
        }
        flock_ui max_panes=13 {
            pane split_direction="vertical" {
                pane { children; }
                pane { pane; pane; pane; pane; }
                pane { pane; pane; pane; pane; }
            }
        }
    }
    swap_tiled_layout name="horizontal" {
        flock_ui max_panes=5 {
            pane
            pane
        }
        flock_ui max_panes=9 {
            pane {
                pane split_direction="vertical" { children; }
                pane split_direction="vertical" { pane; pane; pane; pane; }
            }
        }
        flock_ui max_panes=13 {
            pane {
                pane split_direction="vertical" { children; }
                pane split_direction="vertical" { pane; pane; pane; pane; }
                pane split_direction="vertical" { pane; pane; pane; pane; }
            }
        }
    }
    swap_tiled_layout name="stacked" {
        flock_ui min_panes=6 {
            pane split_direction="vertical" {
                pane
                pane stacked=true { children; }
            }
        }
    }
    swap_floating_layout name="staggered" {
        floating_panes
    }
    swap_floating_layout name="enlarged" {
        floating_panes max_panes=10 {
            pane { x "5%"; y 1; width "90%"; height "90%"; }
            pane { x "5%"; y 2; width "90%"; height "90%"; }
            pane { x "5%"; y 3; width "90%"; height "90%"; }
            pane { x "5%"; y 4; width "90%"; height "90%"; }
            pane { x "5%"; y 5; width "90%"; height "90%"; }
            pane { x "5%"; y 6; width "90%"; height "90%"; }
            pane { x "5%"; y 7; width "90%"; height "90%"; }
            pane { x "5%"; y 8; width "90%"; height "90%"; }
            pane { x "5%"; y 9; width "90%"; height "90%"; }
            pane { x 10; y 10; width "90%"; height "90%"; }
        }
    }
    swap_floating_layout name="spread" {
        floating_panes max_panes=1 {
            pane {y "50%"; x "50%"; }
        }
        floating_panes max_panes=2 {
            pane { x "1%"; y "25%"; width "45%"; }
            pane { x "50%"; y "25%"; width "45%"; }
        }
        floating_panes max_panes=3 {
            pane { y "55%"; width "45%"; height "45%"; }
            pane { x "1%"; y "1%"; width "45%"; }
            pane { x "50%"; y "1%"; width "45%"; }
        }
        floating_panes max_panes=4 {
            pane { x "1%"; y "55%"; width "45%"; height "45%"; }
            pane { x "50%"; y "55%"; width "45%"; height "45%"; }
            pane { x "1%"; y "1%"; width "45%"; height "45%"; }
            pane { x "50%"; y "1%"; width "45%"; height "45%"; }
        }
    }
}
"#;

/// The generated stringified layout a bound session is created from: the
/// user's `remote_session_layout` file (or deprecated legacy fallback) when read
/// (`base_layout`), else the built-in flock chrome mirror (see
/// [`FLOCK_LAYOUT_TEMPLATE`]) — in both cases with the binding +
/// no-resurrection options appended (layout-doc options merge over the base
/// config server-side). Every content pane — initial, split, or new tab —
/// falls back to the session's `default_command` and SSHes into the
/// codespace; plugin panes (sidebar, status bars) are unaffected. A pane
/// whose SSH ends (remote `exit`, dropped connection) closes like a normal
/// shell exit. Base layouts should open a single initial tab: several tabs
/// racing `gh codespace ssh` against a stopped codespace all but guarantees
/// the losing connections' tabs close on arrival.
pub fn layout_doc_for(
    codespace_name: &str,
    sidebar_args: &[(String, String)],
    base_layout: Option<&str>,
) -> String {
    layout_doc_with_binding(&ssh_argv(codespace_name), sidebar_args, base_layout)
}

/// Build a bound session's layout doc for an arbitrary binding argv — the
/// shared mechanics behind [`layout_doc_for`] and the devcontainer variant
/// ([`crate::devcontainers::layout_doc_for`]): the user's base layout (or the
/// built-in flock chrome mirror with the sidebar args injected), with the
/// binding + no-resurrection options appended.
pub fn layout_doc_with_binding(
    binding_argv: &[String],
    sidebar_args: &[(String, String)],
    base_layout: Option<&str>,
) -> String {
    let quoted: Vec<String> = binding_argv.iter().map(|arg| kdl_quote(arg)).collect();
    let layout = match base_layout {
        Some(base) => format!("{}\n", base.trim_end()),
        None => FLOCK_LAYOUT_TEMPLATE
            .replace("__SIDEBAR_25__", &sidebar_plugin_block(sidebar_args, 12))
            .replace("__SIDEBAR_40__", &sidebar_plugin_block(sidebar_args, 16)),
    };
    format!(
        "{}default_command {}\nsession_serialization false\n",
        layout,
        quoted.join(" ")
    )
}

/// A `plugin location="zellij:flock-sidebar"` block with the shared dir args
/// as its children, indented to sit at `indent` spaces (the marker's depth).
fn sidebar_plugin_block(sidebar_args: &[(String, String)], indent: usize) -> String {
    if sidebar_args.is_empty() {
        return r#"plugin location="zellij:flock-sidebar""#.to_owned();
    }
    let pad = " ".repeat(indent);
    let mut block = String::from("plugin location=\"zellij:flock-sidebar\" {\n");
    for (key, value) in sidebar_args {
        block.push_str(&format!("{}    {} {}\n", pad, key, kdl_quote(value)));
    }
    block.push_str(&format!("{}}}", pad));
    block
}

/// Quote a string as a KDL string literal.
fn kdl_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Context map for the list command.
pub fn list_context() -> BTreeMap<String, String> {
    BTreeMap::from_iter([(LIST_CONTEXT_KEY.to_owned(), String::new())])
}

/// Context map for a stop command, carrying the codespace name.
pub fn stop_context(codespace_name: &str) -> BTreeMap<String, String> {
    BTreeMap::from_iter([(STOP_CONTEXT_KEY.to_owned(), codespace_name.to_owned())])
}

/// Argv for reading the user's shared remote layout base off the host.
pub fn layout_read_argv(path: &Path) -> Vec<String> {
    vec!["cat".to_owned(), path.to_string_lossy().to_string()]
}

/// Context map for the layout read.
pub fn layout_read_context() -> BTreeMap<String, String> {
    BTreeMap::from_iter([(LAYOUT_CONTEXT_KEY.to_owned(), String::new())])
}

// ---------------------------------------------------------------------------
// gh output parsing + error classification
// ---------------------------------------------------------------------------

/// Why a `gh codespace` invocation failed, reduced to the states the UI
/// renders distinctly (each with an actionable hint).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GhError {
    /// `gh` isn't installed / not on the host PATH.
    GhMissing,
    /// `gh` is installed but not logged in.
    NotAuthed,
    /// Logged in, but the token lacks the `codespace` scope.
    MissingScope,
    /// Anything else — the first stderr line, for display.
    Other(String),
}

/// Classify a failed `gh` run from its exit code and stderr.
pub fn classify_error(exit_code: Option<i32>, stderr: &str) -> GhError {
    let stderr_lower = stderr.to_lowercase();
    if stderr_lower.contains("\"codespace\" scope")
        || stderr_lower.contains("gh auth refresh -h github.com -s codespace")
    {
        return GhError::MissingScope;
    }
    if stderr_lower.contains("gh auth login") || stderr_lower.contains("not logged in") {
        return GhError::NotAuthed;
    }
    if exit_code == Some(127)
        || stderr_lower.contains("command not found")
        || stderr_lower.contains("no such file or directory")
    {
        return GhError::GhMissing;
    }
    let first_line = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    GhError::Other(first_line.trim().to_owned())
}

/// Parse `gh codespace list --json name,displayName,repository,state,lastUsedAt`
/// output. Field extraction is defensive (`repository` is read as either a
/// plain string or an object) so a `gh` output-shape drift degrades a field
/// instead of dropping the whole list.
pub fn parse_list_json(raw: &str) -> Result<Vec<Codespace>, String> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("invalid JSON from gh: {}", e))?;
    let entries = value
        .as_array()
        .ok_or_else(|| "expected a JSON array from gh".to_owned())?;
    let mut codespaces = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(name) = entry.get("name").and_then(|v| v.as_str()) else {
            continue; // an entry without a name can't be connected to
        };
        let display_name = entry
            .get("displayName")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(name);
        let repository = entry
            .get("repository")
            .map(|r| {
                r.as_str().map(str::to_owned).unwrap_or_else(|| {
                    r.get("nameWithOwner")
                        .or_else(|| r.get("full_name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_owned()
                })
            })
            .unwrap_or_default();
        let state = entry
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        let last_used_at = entry
            .get("lastUsedAt")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        codespaces.push(Codespace {
            name: name.to_owned(),
            display_name: display_name.to_owned(),
            repository,
            state,
            last_used_at,
        });
    }
    Ok(codespaces)
}

// ---------------------------------------------------------------------------
// /data cache (mirrors the frecency db's best-effort persistence)
// ---------------------------------------------------------------------------

/// Load the cached list from `/data`, empty on any error.
pub fn load_cache() -> Vec<Codespace> {
    load_cache_from(Path::new(CACHE_PATH))
}

fn load_cache_from(path: &Path) -> Vec<Codespace> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Persist the list to `/data`, ignoring any error (the cache is an
/// optimization, never load-bearing).
pub fn save_cache(codespaces: &[Codespace]) {
    if let Ok(raw) = serde_json::to_string(codespaces) {
        let _ = std::fs::write(CACHE_PATH, raw);
    }
}

// ---------------------------------------------------------------------------
// Ranking
// ---------------------------------------------------------------------------

/// Small bonus for a display-name hit over a repository-only hit, mirroring
/// the project ranking's name-over-path preference.
const NAME_MATCH_BONUS: i32 = 8;

/// A codespace paired with its rank and the match ranges to highlight.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedCodespace<'a> {
    pub codespace: &'a Codespace,
    pub rank: i32,
    /// Matched byte ranges within `codespace.display_name`.
    pub name_ranges: Vec<(usize, usize)>,
    /// Matched byte ranges within `codespace.repository`.
    pub repo_ranges: Vec<(usize, usize)>,
}

/// Rank `codespaces` for `query`, best-first. Non-matches are dropped for a
/// non-empty query; an empty query orders by recency (`lastUsedAt` desc).
pub fn rank<'a>(codespaces: &'a [Codespace], query: &str) -> Vec<RankedCodespace<'a>> {
    let query = query.trim();
    let mut ranked: Vec<RankedCodespace<'a>> = Vec::with_capacity(codespaces.len());
    for codespace in codespaces {
        let name_match = fuzzy_match(query, &codespace.display_name);
        let repo_match = fuzzy_match(query, &codespace.repository);
        if !query.is_empty() && name_match.is_none() && repo_match.is_none() {
            continue;
        }
        let name_score = name_match.as_ref().map(|m| m.score + NAME_MATCH_BONUS);
        let repo_score = repo_match.as_ref().map(|m| m.score);
        let rank = name_score.into_iter().chain(repo_score).max().unwrap_or(0);
        ranked.push(RankedCodespace {
            codespace,
            rank,
            name_ranges: name_match.map(|m| m.ranges).unwrap_or_default(),
            repo_ranges: repo_match.map(|m| m.ranges).unwrap_or_default(),
        });
    }
    // Best first; ties broken by recency (RFC3339 strings order correctly),
    // then name for determinism.
    ranked.sort_by(|a, b| {
        b.rank
            .cmp(&a.rank)
            .then_with(|| b.codespace.last_used_at.cmp(&a.codespace.last_used_at))
            .then_with(|| a.codespace.display_name.cmp(&b.codespace.display_name))
    });
    ranked
}

// ---------------------------------------------------------------------------
// Switch-or-create resolution
// ---------------------------------------------------------------------------

/// An existing session reduced to what the resolution needs: its name and the
/// codespace it is bound to, if any (parsed from `SessionInfo.default_command`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingSession {
    pub name: String,
    pub bound_codespace: Option<String>,
}

/// What to do when a codespace is confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenAction {
    /// A live session is already bound to this codespace; attach to it.
    Switch { name: String },
    /// No bound session; create one with this collision-safe name.
    Create { name: String },
}

/// Decide the action for `codespace` given the live `sessions`. The binding is
/// matched by codespace name (via each session's parsed `default_command`),
/// never by session name — renaming a bound session doesn't break it.
pub fn resolve_open(codespace: &Codespace, sessions: &[ExistingSession]) -> OpenAction {
    if let Some(existing) = sessions
        .iter()
        .find(|s| s.bound_codespace.as_deref() == Some(codespace.name.as_str()))
    {
        return OpenAction::Switch {
            name: existing.name.clone(),
        };
    }
    let taken: HashSet<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
    let base = sanitize_session_name(&codespace.display_name)
        .or_else(|| sanitize_session_name(&codespace.name))
        .unwrap_or_else(|| FALLBACK_NAME.to_owned());
    OpenAction::Create {
        name: disambiguate_with_suffix(&base, &taken),
    }
}

/// Reduce a codespace display name to a valid zellij session name: whitespace
/// and `/` (which zellij rejects) become `-`, other characters pass through,
/// consecutive/edge dashes collapse. `None` when nothing usable remains.
fn sanitize_session_name(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c.is_whitespace() || c == '/' {
            if !out.ends_with('-') {
                out.push('-');
            }
        } else {
            out.push(c);
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// Append `-2`, `-3`, … to `base` until the result is free. (Mirrors the
/// project resolution's suffixing in `session.rs`.)
fn disambiguate_with_suffix(base: &str, taken: &HashSet<&str>) -> String {
    if !taken.contains(base) {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{}-{}", base, n);
        if !taken.contains(candidate.as_str()) {
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST_JSON: &str = r#"[
        {
            "displayName": "fluffy spork",
            "lastUsedAt": "2026-07-01T10:00:00Z",
            "name": "abeljim8am-fluffy-spork-abc123",
            "repository": "acme/web",
            "state": "Available"
        },
        {
            "displayName": "",
            "lastUsedAt": "2026-06-20T10:00:00Z",
            "name": "abeljim8am-api-def456",
            "repository": "acme/api",
            "state": "Shutdown"
        }
    ]"#;

    fn codespace(name: &str, display_name: &str) -> Codespace {
        Codespace {
            name: name.to_owned(),
            display_name: display_name.to_owned(),
            repository: "acme/web".to_owned(),
            state: "Available".to_owned(),
            last_used_at: "2026-07-01T10:00:00Z".to_owned(),
        }
    }

    #[test]
    fn parses_gh_list_json() {
        let codespaces = parse_list_json(LIST_JSON).unwrap();
        assert_eq!(codespaces.len(), 2);
        assert_eq!(codespaces[0].display_name, "fluffy spork");
        assert_eq!(codespaces[0].state_kind(), StateKind::Running);
        // An empty displayName falls back to the codespace name.
        assert_eq!(codespaces[1].display_name, "abeljim8am-api-def456");
        assert_eq!(codespaces[1].state_kind(), StateKind::Stopped);
    }

    #[test]
    fn parses_repository_as_object_too() {
        let raw = r#"[{"name": "cs-1", "repository": {"nameWithOwner": "acme/api"}}]"#;
        let codespaces = parse_list_json(raw).unwrap();
        assert_eq!(codespaces[0].repository, "acme/api");
    }

    #[test]
    fn rejects_non_array_json() {
        assert!(parse_list_json("{}").is_err());
        assert!(parse_list_json("nonsense").is_err());
    }

    #[test]
    fn classifies_missing_scope() {
        let stderr = r#"error getting codespaces: HTTP 403: Must have admin rights to Repository.
This API operation needs the "codespace" scope. To request it, run:  gh auth refresh -h github.com -s codespace"#;
        assert_eq!(classify_error(Some(1), stderr), GhError::MissingScope);
    }

    #[test]
    fn classifies_not_authed() {
        let stderr = "To get started with GitHub CLI, please run:  gh auth login";
        assert_eq!(classify_error(Some(4), stderr), GhError::NotAuthed);
    }

    #[test]
    fn classifies_gh_missing() {
        assert_eq!(
            classify_error(Some(127), "sh: gh: command not found"),
            GhError::GhMissing
        );
    }

    #[test]
    fn classifies_other_with_first_stderr_line() {
        assert_eq!(
            classify_error(Some(1), "\nsomething broke\ndetails"),
            GhError::Other("something broke".to_owned())
        );
    }

    #[test]
    fn ssh_argv_round_trips_through_recognizer() {
        let argv = ssh_argv("my-codespace");
        assert_eq!(parse_codespace_ssh(&argv), Some("my-codespace"));
    }

    #[test]
    fn recognizer_rejects_other_commands() {
        let to_argv = |args: &[&str]| args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            parse_codespace_ssh(&to_argv(&["gh", "codespace", "list"])),
            None
        );
        assert_eq!(parse_codespace_ssh(&to_argv(&["fish"])), None);
        assert_eq!(
            parse_codespace_ssh(&to_argv(&["gh", "codespace", "ssh", "-c", "x", "extra"])),
            None
        );
    }

    #[test]
    fn layout_doc_carries_binding_and_disables_serialization() {
        let doc = layout_doc_for("my-codespace", &[], None);
        assert!(doc.contains(r#"default_command "gh" "codespace" "ssh" "-c" "my-codespace""#));
        assert!(doc.contains("session_serialization false"));
        assert!(doc.starts_with("layout {"));
    }

    #[test]
    fn layout_doc_escapes_quotes() {
        let doc = layout_doc_for(r#"we"ird"#, &[], None);
        assert!(doc.contains(r#""we\"ird""#));
    }

    #[test]
    fn layout_doc_mirrors_flock_chrome_with_sidebar_args() {
        let args = vec![
            ("root_dirs".to_owned(), "~/work;~/oss".to_owned()),
            ("cwd".to_owned(), "/".to_owned()),
        ];
        let doc = layout_doc_for("my-codespace", &args, None);
        assert!(doc.contains(r#"plugin location="tab-bar""#));
        assert!(doc.contains(r#"plugin location="status-bar""#));
        assert!(doc.contains(r#"plugin location="zellij:flock-sidebar" {"#));
        assert!(doc.contains(r#"root_dirs "~/work;~/oss""#));
        assert!(doc.contains(r#"cwd "/""#));
        assert!(doc.contains(r#"tab_template name="flock_ui""#));
        assert!(doc.contains(r#"swap_tiled_layout name="stacked""#));
        assert!(!doc.contains("__SIDEBAR_25__"));
        assert!(!doc.contains("__SIDEBAR_40__"));
    }

    /// A user base with multiple explicit tabs (like the flock-codespace
    /// layout's ai/editor/terminal) must keep ALL of them through the
    /// stringified parse the server runs at session creation.
    #[test]
    fn layout_doc_keeps_multiple_tabs_from_user_base() {
        let base = r#"layout {
    default_tab_template {
        pane size=1 borderless=true {
            plugin location="tab-bar"
        }
        children
    }
    tab focus=true name="ai" {
        pane borderless=true
    }
    tab name="editor" {
        pane borderless=true
    }
    tab name="terminal" {
        pane borderless=true
    }
}"#;
        let doc = layout_doc_for("my-codespace", &[], Some(base));
        let (layout, _config) = zellij_utils::input::layout::Layout::from_stringified_layout(
            &doc,
            zellij_utils::input::config::Config::default(),
        )
        .expect("multi-tab base must parse");
        assert!(layout.has_tabs());
        let tab_names: Vec<Option<String>> = layout
            .tabs()
            .iter()
            .map(|(name, _, _)| name.clone())
            .collect();
        assert_eq!(
            tab_names,
            vec![
                Some("ai".to_owned()),
                Some("editor".to_owned()),
                Some("terminal".to_owned())
            ]
        );
    }

    #[test]
    fn layout_doc_uses_user_base_layout_when_provided() {
        let base = "layout {\n    pane borderless=true\n}";
        let doc = layout_doc_for("my-codespace", &[], Some(base));
        assert!(doc.starts_with("layout {\n    pane borderless=true\n}\n"));
        assert!(doc.contains(r#"default_command "gh" "codespace" "ssh" "-c" "my-codespace""#));
        assert!(doc.contains("session_serialization false"));
        // The built-in mirror must not leak in alongside the user's base.
        assert!(!doc.contains("flock_ui"));

        // A user base + binding options must survive the server-side parse too.
        let (_, config) = zellij_utils::input::layout::Layout::from_stringified_layout(
            &doc,
            zellij_utils::input::config::Config::default(),
        )
        .expect("user-base layout doc must parse");
        assert_eq!(config.options.session_serialization, Some(false));
    }

    /// The generated doc must survive the exact parse the server runs on a
    /// stringified layout (`Layout::from_kdl` for validation, then
    /// `Config::from_kdl` merging the embedded options). This is what catches
    /// KDL drift between the template here and the real parser.
    #[test]
    fn layout_doc_parses_with_the_server_side_parser() {
        let args = vec![("individual_dirs".to_owned(), "/a;/b".to_owned())];
        let doc = layout_doc_for("my-codespace", &args, None);
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
            Some(ssh_argv("my-codespace").as_slice())
        );
        assert_eq!(config.options.session_serialization, Some(false));
    }

    #[test]
    fn resolve_switches_to_bound_session_regardless_of_name() {
        let sessions = vec![ExistingSession {
            name: "renamed-by-user".to_owned(),
            bound_codespace: Some("cs-name-abc".to_owned()),
        }];
        let action = resolve_open(&codespace("cs-name-abc", "fluffy"), &sessions);
        assert_eq!(
            action,
            OpenAction::Switch {
                name: "renamed-by-user".to_owned()
            }
        );
    }

    #[test]
    fn resolve_creates_with_sanitized_display_name() {
        let action = resolve_open(&codespace("cs-name-abc", "fluffy spork"), &[]);
        assert_eq!(
            action,
            OpenAction::Create {
                name: "fluffy-spork".to_owned()
            }
        );
    }

    #[test]
    fn resolve_disambiguates_against_taken_names() {
        let sessions = vec![ExistingSession {
            name: "fluffy-spork".to_owned(),
            bound_codespace: None, // a project session that happens to share the name
        }];
        let action = resolve_open(&codespace("cs-name-abc", "fluffy spork"), &sessions);
        assert_eq!(
            action,
            OpenAction::Create {
                name: "fluffy-spork-2".to_owned()
            }
        );
    }

    #[test]
    fn resolve_falls_back_to_codespace_name_then_constant() {
        let action = resolve_open(&codespace("cs-name-abc", "///"), &[]);
        assert_eq!(
            action,
            OpenAction::Create {
                name: "cs-name-abc".to_owned()
            }
        );
        let action = resolve_open(&codespace("///", "///"), &[]);
        assert_eq!(
            action,
            OpenAction::Create {
                name: FALLBACK_NAME.to_owned()
            }
        );
    }

    #[test]
    fn rank_empty_query_orders_by_recency() {
        let mut older = codespace("cs-old", "older");
        older.last_used_at = "2026-01-01T00:00:00Z".to_owned();
        let newer = codespace("cs-new", "newer");
        let codespaces = vec![older, newer];
        let ranked = rank(&codespaces, "");
        assert_eq!(ranked[0].codespace.name, "cs-new");
        assert_eq!(ranked.len(), 2);
    }

    #[test]
    fn rank_filters_non_matches_and_prefers_name_hits() {
        let by_name = codespace("cs-1", "webapp");
        let mut by_repo = codespace("cs-2", "unrelated");
        by_repo.repository = "acme/webapp".to_owned();
        let mut miss = codespace("cs-3", "zzz");
        miss.repository = "acme/zzz".to_owned();
        let codespaces = vec![by_repo, by_name, miss];
        let ranked = rank(&codespaces, "webapp");
        let names: Vec<&str> = ranked.iter().map(|r| r.codespace.name.as_str()).collect();
        assert_eq!(names, vec!["cs-1", "cs-2"]);
        assert!(!ranked[0].name_ranges.is_empty());
    }

    #[test]
    fn cache_load_missing_file_is_empty() {
        assert!(load_cache_from(Path::new("/definitely/not/here.json")).is_empty());
    }
}
