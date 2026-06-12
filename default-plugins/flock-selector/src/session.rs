//! Resolving a confirmed project into a switch-or-create action.
//!
//! One folder maps to exactly one session. When a project is confirmed:
//! - if a session already roots at that folder (matched against each
//!   `SessionInfo.workspace_root`, the Phase 6 fork field), attach to it by name
//!   ([`OpenAction::Switch`]); otherwise
//! - create a fresh session rooted there, named after the folder's basename and
//!   disambiguated against the live session names ([`OpenAction::Create`]).
//!
//! The decision is pure (it takes the project path and the reduced session list)
//! so the naming/collision logic is unit-testable without constructing a full
//! `SessionInfo`. The caller in [`crate::State`] turns the action into the
//! matching `switch_session_with_cwd` / `switch_session_with_layout` shim call.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use crate::config::normalize;

/// Fallback session name when a path has no usable folder components (e.g. `/`).
const FALLBACK_NAME: &str = "session";

/// An existing session reduced to the fields the resolution needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingSession {
    /// The session's name (the switch target, and a collision source for naming).
    pub name: String,
    /// The folder the session was rooted in, normalized (no trailing slash).
    pub workspace_root: PathBuf,
    /// Not a real project session — the picker's own cold-shell entry session,
    /// whose `workspace_root` is just the folder `zellij` happened to be
    /// launched from. Hidden sessions never match a folder (switching into one
    /// strands the user in a pane-less throwaway), but their names still count
    /// as taken so a new session can't collide with them.
    pub hidden: bool,
}

/// What to do when a project is confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenAction {
    /// A session already roots at this folder; attach to it by name.
    Switch { name: String },
    /// No session here yet; create one rooted at the folder with this
    /// collision-safe name.
    Create { name: String },
}

/// Decide the action for `project_path` given the current `sessions`.
///
/// Switches when a session's `workspace_root` already equals the (normalized)
/// project path; otherwise creates one with a unique basename-derived name.
pub fn resolve_open(project_path: &Path, sessions: &[ExistingSession]) -> OpenAction {
    let project_path = normalize(project_path);

    if let Some(existing) = sessions
        .iter()
        .find(|s| !s.hidden && s.workspace_root == project_path)
    {
        return OpenAction::Switch {
            name: existing.name.clone(),
        };
    }

    let taken: HashSet<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
    OpenAction::Create {
        name: unique_session_name(&project_path, &taken),
    }
}

/// Pick a session name for `path` that isn't already in `taken`.
///
/// Starts from the folder basename and, on collision with a *different* path's
/// session, prepends successive parent-dir segments (`api` → `backend-api` →
/// `me-backend-api`) until unique. The plan describes this as `backend/api`, but
/// zellij forbids `/` in session names, so segments are joined with `-` — the
/// faithful, valid analog. If even the fully-qualified path collides, a numeric
/// suffix (`api-2`, `api-3`, …) guarantees termination.
fn unique_session_name(path: &Path, taken: &HashSet<&str>) -> String {
    // Folder components from root → leaf, dropping `/`, `.`, `..` etc.
    let segments: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().to_string()),
            _ => None,
        })
        .collect();

    if segments.is_empty() {
        return disambiguate_with_suffix(FALLBACK_NAME, taken);
    }

    // Progressively widen the name leaf-first: basename, then parent-basename, …
    let mut candidate = String::new();
    for (depth, segment) in segments.iter().rev().enumerate() {
        candidate = if depth == 0 {
            segment.clone()
        } else {
            format!("{}-{}", segment, candidate)
        };
        if !taken.contains(candidate.as_str()) {
            return candidate;
        }
    }

    // The whole path is somehow already taken — fall back to a numeric suffix on
    // the fully-qualified name.
    disambiguate_with_suffix(&candidate, taken)
}

/// Append `-2`, `-3`, … to `base` until the result is free.
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

    fn session(name: &str, root: &str) -> ExistingSession {
        ExistingSession {
            name: name.to_string(),
            workspace_root: PathBuf::from(root),
            hidden: false,
        }
    }

    fn hidden_session(name: &str, root: &str) -> ExistingSession {
        ExistingSession {
            hidden: true,
            ..session(name, root)
        }
    }

    #[test]
    fn switches_when_a_session_roots_at_the_folder() {
        let sessions = vec![session("myproj", "/home/me/myproj")];
        let action = resolve_open(Path::new("/home/me/myproj"), &sessions);
        assert_eq!(action, OpenAction::Switch { name: "myproj".into() });
    }

    #[test]
    fn matches_root_ignoring_trailing_slash() {
        let sessions = vec![session("myproj", "/home/me/myproj")];
        let action = resolve_open(Path::new("/home/me/myproj/"), &sessions);
        assert_eq!(action, OpenAction::Switch { name: "myproj".into() });
    }

    #[test]
    fn creates_with_basename_when_no_session_exists() {
        let action = resolve_open(Path::new("/home/me/fresh"), &[]);
        assert_eq!(action, OpenAction::Create { name: "fresh".into() });
    }

    #[test]
    fn creates_with_basename_when_other_sessions_unrelated() {
        let sessions = vec![session("other", "/home/me/other")];
        let action = resolve_open(Path::new("/home/me/fresh"), &sessions);
        assert_eq!(action, OpenAction::Create { name: "fresh".into() });
    }

    #[test]
    fn disambiguates_same_basename_with_parent_segment() {
        // A different folder already owns the `api` session name.
        let sessions = vec![session("api", "/home/me/frontend/api")];
        let action = resolve_open(Path::new("/home/me/backend/api"), &sessions);
        assert_eq!(action, OpenAction::Create { name: "backend-api".into() });
    }

    #[test]
    fn disambiguation_climbs_multiple_parents() {
        let sessions = vec![
            session("api", "/a/api"),
            session("backend-api", "/x/backend/api"),
        ];
        let action = resolve_open(Path::new("/home/backend/api"), &sessions);
        assert_eq!(
            action,
            OpenAction::Create {
                name: "home-backend-api".into()
            }
        );
    }

    #[test]
    fn fully_qualified_collision_falls_back_to_numeric_suffix() {
        let sessions = vec![
            session("api", "/other1"),
            session("backend-api", "/other2"),
        ];
        // Path "/backend/api": basename `api` taken, then `backend-api` taken,
        // and there are no further parents → numeric suffix on the widest name.
        let action = resolve_open(Path::new("/backend/api"), &sessions);
        assert_eq!(
            action,
            OpenAction::Create {
                name: "backend-api-2".into()
            }
        );
    }

    #[test]
    fn rootless_path_uses_fallback_name() {
        let action = resolve_open(Path::new("/"), &[]);
        assert_eq!(action, OpenAction::Create { name: FALLBACK_NAME.into() });
    }

    #[test]
    fn hidden_session_never_matches_its_root() {
        // The cold-shell entry session claims the folder zellij was launched
        // from; picking that folder must create a real session there, not
        // switch into the throwaway.
        let sessions = vec![hidden_session("flock-selector", "/home/me/myproj")];
        let action = resolve_open(Path::new("/home/me/myproj"), &sessions);
        assert_eq!(action, OpenAction::Create { name: "myproj".into() });
    }

    #[test]
    fn hidden_session_name_still_blocks_the_namespace() {
        let sessions = vec![hidden_session("api", "/elsewhere")];
        let action = resolve_open(Path::new("/home/backend/api"), &sessions);
        assert_eq!(action, OpenAction::Create { name: "backend-api".into() });
    }
}
