//! Coder workspace support, backed by the currently authenticated `coder` CLI
//! deployment. Workspaces bind to sessions through the exact default command
//! `coder ssh owner/name`.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::codespaces::layout_doc_with_binding;
use crate::fuzzy::fuzzy_match;

pub const LIST_CONTEXT_KEY: &str = "flock_coder_list";
pub const STOP_CONTEXT_KEY: &str = "flock_coder_stop";

const CACHE_PATH: &str = "/data/coder-workspaces.json";
const CACHE_TTL_SECS: u64 = 300;
const FALLBACK_NAME: &str = "coder";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CoderWorkspace {
    pub owner: String,
    pub name: String,
    pub template: String,
    pub status: String,
    pub favorite: bool,
    pub last_used_at: String,
}

impl CoderWorkspace {
    pub fn identifier(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    pub fn state_kind(&self) -> StateKind {
        match self.status.to_ascii_lowercase().as_str() {
            "running" => StateKind::Running,
            "stopped" | "deleted" | "canceled" | "cancelled" | "failed" => StateKind::Stopped,
            "pending" | "starting" | "stopping" | "deleting" => StateKind::Busy,
            _ => StateKind::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateKind {
    Running,
    Stopped,
    Busy,
    Unknown,
}

pub fn list_argv() -> Vec<String> {
    vec![
        "coder".into(),
        "list".into(),
        "--output".into(),
        "json".into(),
    ]
}

pub fn ssh_argv(identifier: &str) -> Vec<String> {
    vec!["coder".into(), "ssh".into(), identifier.into()]
}

pub fn stop_argv(identifier: &str) -> Vec<String> {
    vec![
        "coder".into(),
        "stop".into(),
        "-y".into(),
        identifier.into(),
    ]
}

pub fn parse_coder_ssh(argv: &[String]) -> Option<&str> {
    match argv {
        [coder, ssh, identifier]
            if coder == "coder" && ssh == "ssh" && valid_identifier(identifier) =>
        {
            Some(identifier)
        },
        _ => None,
    }
}

fn valid_identifier(identifier: &str) -> bool {
    let mut parts = identifier.split('/');
    parts.next().is_some_and(|part| !part.is_empty())
        && parts.next().is_some_and(|part| !part.is_empty())
        && parts.next().is_none()
}

pub fn layout_doc_for(
    identifier: &str,
    sidebar_args: &[(String, String)],
    base_layout: Option<&str>,
) -> String {
    layout_doc_with_binding(&ssh_argv(identifier), sidebar_args, base_layout)
}

pub fn list_context() -> BTreeMap<String, String> {
    BTreeMap::from_iter([(LIST_CONTEXT_KEY.into(), String::new())])
}

pub fn stop_context(identifier: &str) -> BTreeMap<String, String> {
    BTreeMap::from_iter([(STOP_CONTEXT_KEY.into(), identifier.into())])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoderError {
    CliMissing,
    NotConfigured,
    NotAuthenticated,
    MalformedJson(String),
    Other(String),
}

pub fn classify_error(exit_code: Option<i32>, stderr: &str) -> CoderError {
    let lower = stderr.to_ascii_lowercase();
    if exit_code == Some(127)
        || lower.contains("command not found")
        || lower.contains("no such file or directory")
    {
        return CoderError::CliMissing;
    }
    if lower.contains("coder_url")
        || lower.contains("no url")
        || lower.contains("url is not configured")
        || lower.contains("coder url not found")
        || lower.contains("no deployment")
    {
        return CoderError::NotConfigured;
    }
    if lower.contains("coder login")
        || lower.contains("not logged in")
        || lower.contains("not authenticated")
        || lower.contains("unauthorized")
        || lower.contains("status code 401")
    {
        return CoderError::NotAuthenticated;
    }
    let first = stderr
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    CoderError::Other(first.trim().to_owned())
}

pub fn parse_list_json(raw: &str) -> Result<Vec<CoderWorkspace>, CoderError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|error| CoderError::MalformedJson(error.to_string()))?;
    let entries = value
        .as_array()
        .ok_or_else(|| CoderError::MalformedJson("expected a JSON array".into()))?;
    let mut workspaces = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(name) = string_field(entry, &["name"]) else {
            continue;
        };
        let Some(owner) = string_field(entry, &["owner_name", "ownerName"]) else {
            continue;
        };
        if name.trim().is_empty() || owner.trim().is_empty() {
            continue;
        }
        let template = string_field(entry, &["template_display_name", "templateDisplayName"])
            .filter(|template| !template.trim().is_empty())
            .or_else(|| string_field(entry, &["template_name", "templateName"]))
            .unwrap_or_default();
        let status = workspace_status(entry);
        let favorite = entry
            .get("favorite")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let last_used_at =
            string_field(entry, &["last_used_at", "lastUsedAt"]).unwrap_or_else(|| {
                string_field(entry, &["updated_at", "updatedAt"]).unwrap_or_default()
            });
        workspaces.push(CoderWorkspace {
            owner,
            name,
            template,
            status,
            favorite,
            last_used_at,
        });
    }
    Ok(workspaces)
}

fn workspace_status(entry: &serde_json::Value) -> String {
    let Some(build) = entry
        .get("latest_build")
        .or_else(|| entry.get("latestBuild"))
    else {
        return string_field(entry, &["status"]).unwrap_or_default();
    };
    let transition = string_field(build, &["transition"])
        .unwrap_or_default()
        .to_ascii_lowercase();
    let job_status = build
        .get("job")
        .and_then(|job| string_field(job, &["status"]))
        .or_else(|| string_field(build, &["status"]))
        .unwrap_or_default()
        .to_ascii_lowercase();
    match (transition.as_str(), job_status.as_str()) {
        ("start", "succeeded") => "Running".into(),
        ("stop", "succeeded") => "Stopped".into(),
        ("delete", "succeeded") => "Deleted".into(),
        ("start", "pending" | "running") => "Starting".into(),
        ("stop", "pending" | "running") => "Stopping".into(),
        ("delete", "pending" | "running") => "Deleting".into(),
        (_, "failed" | "canceled" | "cancelled") => "Failed".into(),
        _ => job_status,
    }
}

fn string_field(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(|value| value.as_str()))
        .map(str::to_owned)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CacheEnvelope {
    saved_at: u64,
    workspaces: Vec<CoderWorkspace>,
}

pub fn load_cache() -> Vec<CoderWorkspace> {
    load_cache_from(Path::new(CACHE_PATH), now_secs())
}

fn load_cache_from(path: &Path, now: u64) -> Vec<CoderWorkspace> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<CacheEnvelope>(&raw).ok())
        .filter(|cache| now.saturating_sub(cache.saved_at) <= CACHE_TTL_SECS)
        .map(|cache| cache.workspaces)
        .unwrap_or_default()
}

pub fn save_cache(workspaces: &[CoderWorkspace]) {
    let cache = CacheEnvelope {
        saved_at: now_secs(),
        workspaces: workspaces.to_vec(),
    };
    if let Ok(raw) = serde_json::to_string(&cache) {
        let _ = std::fs::write(CACHE_PATH, raw);
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

const NAME_MATCH_BONUS: i32 = 8;

#[derive(Debug, Clone, PartialEq)]
pub struct RankedCoderWorkspace<'a> {
    pub workspace: &'a CoderWorkspace,
    pub rank: i32,
    pub identifier_ranges: Vec<(usize, usize)>,
    pub template_ranges: Vec<(usize, usize)>,
}

pub fn rank<'a>(workspaces: &'a [CoderWorkspace], query: &str) -> Vec<RankedCoderWorkspace<'a>> {
    let query = query.trim();
    let mut ranked = Vec::with_capacity(workspaces.len());
    for workspace in workspaces {
        let identifier = workspace.identifier();
        let identifier_match = fuzzy_match(query, &identifier);
        let template_match = fuzzy_match(query, &workspace.template);
        if !query.is_empty() && identifier_match.is_none() && template_match.is_none() {
            continue;
        }
        let rank = identifier_match
            .as_ref()
            .map(|result| result.score + NAME_MATCH_BONUS)
            .into_iter()
            .chain(template_match.as_ref().map(|result| result.score))
            .max()
            .unwrap_or(0);
        ranked.push(RankedCoderWorkspace {
            workspace,
            rank,
            identifier_ranges: identifier_match
                .map(|result| result.ranges)
                .unwrap_or_default(),
            template_ranges: template_match
                .map(|result| result.ranges)
                .unwrap_or_default(),
        });
    }
    ranked.sort_by(|a, b| {
        b.rank
            .cmp(&a.rank)
            .then_with(|| b.workspace.favorite.cmp(&a.workspace.favorite))
            .then_with(|| b.workspace.last_used_at.cmp(&a.workspace.last_used_at))
            .then_with(|| a.workspace.identifier().cmp(&b.workspace.identifier()))
    });
    ranked
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingSession {
    pub name: String,
    pub bound_workspace: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenAction {
    Switch { name: String },
    Create { name: String },
}

pub fn resolve_open(workspace: &CoderWorkspace, sessions: &[ExistingSession]) -> OpenAction {
    let identifier = workspace.identifier();
    if let Some(session) = sessions
        .iter()
        .find(|session| session.bound_workspace.as_deref() == Some(identifier.as_str()))
    {
        return OpenAction::Switch {
            name: session.name.clone(),
        };
    }
    let taken: HashSet<&str> = sessions
        .iter()
        .map(|session| session.name.as_str())
        .collect();
    let base = sanitize_session_name(&workspace.name)
        .or_else(|| sanitize_session_name(&identifier))
        .unwrap_or_else(|| FALLBACK_NAME.into());
    OpenAction::Create {
        name: disambiguate_with_suffix(&base, &taken),
    }
}

fn sanitize_session_name(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    for character in raw.chars() {
        if character.is_whitespace() || character == '/' {
            if !out.ends_with('-') {
                out.push('-');
            }
        } else {
            out.push(character);
        }
    }
    let trimmed = out.trim_matches('-');
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn disambiguate_with_suffix(base: &str, taken: &HashSet<&str>) -> String {
    if !taken.contains(base) {
        return base.into();
    }
    let mut suffix = 2;
    loop {
        let candidate = format!("{}-{}", base, suffix);
        if !taken.contains(candidate.as_str()) {
            return candidate;
        }
        suffix += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST_JSON: &str = r#"[
      {
        "name": "api",
        "owner_name": "alice",
        "template_display_name": "",
        "template_name": "rust",
        "favorite": true,
        "last_used_at": "2026-07-17T12:00:00Z",
        "latest_build": { "transition": "start", "job": { "status": "succeeded" } }
      },
      {
        "name": "web",
        "ownerName": "bob",
        "templateDisplayName": "Web App",
        "lastUsedAt": "2026-07-16T12:00:00Z",
        "latestBuild": { "transition": "stop", "job": { "status": "succeeded" } }
      }
    ]"#;

    fn workspace(owner: &str, name: &str) -> CoderWorkspace {
        CoderWorkspace {
            owner: owner.into(),
            name: name.into(),
            template: "rust".into(),
            status: "running".into(),
            favorite: false,
            last_used_at: "2026-07-17T12:00:00Z".into(),
        }
    }

    #[test]
    fn parses_workspace_fixture_and_states() {
        let parsed = parse_list_json(LIST_JSON).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].identifier(), "alice/api");
        assert_eq!(parsed[0].template, "rust");
        assert!(parsed[0].favorite);
        assert_eq!(parsed[0].state_kind(), StateKind::Running);
        assert_eq!(parsed[1].state_kind(), StateKind::Stopped);
    }

    #[test]
    fn malformed_json_is_distinct() {
        assert!(matches!(
            parse_list_json("{"),
            Err(CoderError::MalformedJson(_))
        ));
        assert!(matches!(
            parse_list_json("{}"),
            Err(CoderError::MalformedJson(_))
        ));
    }

    #[test]
    fn commands_and_binding_are_exact() {
        assert_eq!(list_argv(), vec!["coder", "list", "--output", "json"]);
        assert_eq!(
            stop_argv("alice/api"),
            vec!["coder", "stop", "-y", "alice/api"]
        );
        let ssh = ssh_argv("alice/api");
        assert_eq!(ssh, vec!["coder", "ssh", "alice/api"]);
        assert_eq!(parse_coder_ssh(&ssh), Some("alice/api"));
        assert_eq!(
            parse_coder_ssh(&["coder".into(), "ssh".into(), "api".into()]),
            None
        );
    }

    #[test]
    fn classifies_actionable_errors() {
        assert_eq!(
            classify_error(Some(127), "coder: command not found"),
            CoderError::CliMissing
        );
        assert_eq!(
            classify_error(Some(1), "please run coder login"),
            CoderError::NotAuthenticated
        );
        assert_eq!(
            classify_error(Some(1), "CODER_URL is not configured"),
            CoderError::NotConfigured
        );
    }

    #[test]
    fn ranking_matches_identifier_and_template() {
        let mut by_template = workspace("alice", "api");
        by_template.template = "kubernetes".into();
        let by_name = workspace("bob", "kube-tools");
        let workspaces = vec![by_template, by_name];
        let ranked = rank(&workspaces, "kube");
        assert_eq!(ranked.len(), 2);
        assert!(ranked
            .iter()
            .find(|ranked| ranked.workspace.identifier() == "bob/kube-tools")
            .is_some_and(|ranked| !ranked.identifier_ranges.is_empty()));
        assert!(ranked
            .iter()
            .find(|ranked| ranked.workspace.identifier() == "alice/api")
            .is_some_and(|ranked| !ranked.template_ranges.is_empty()));
    }

    #[test]
    fn cache_expires() {
        let path =
            std::env::temp_dir().join(format!("flock-coder-cache-{}.json", std::process::id()));
        let cache = CacheEnvelope {
            saved_at: 100,
            workspaces: vec![workspace("alice", "api")],
        };
        std::fs::write(&path, serde_json::to_string(&cache).unwrap()).unwrap();
        assert_eq!(load_cache_from(&path, 100 + CACHE_TTL_SECS).len(), 1);
        assert!(load_cache_from(&path, 101 + CACHE_TTL_SECS).is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn resolution_switches_and_creates_unique_names() {
        let target = workspace("alice", "my workspace");
        let sessions = vec![ExistingSession {
            name: "renamed".into(),
            bound_workspace: Some("alice/my workspace".into()),
        }];
        assert_eq!(
            resolve_open(&target, &sessions),
            OpenAction::Switch {
                name: "renamed".into()
            }
        );

        let sessions = vec![ExistingSession {
            name: "my-workspace".into(),
            bound_workspace: None,
        }];
        assert_eq!(
            resolve_open(&target, &sessions),
            OpenAction::Create {
                name: "my-workspace-2".into()
            }
        );
    }

    #[test]
    fn generated_layout_parses_and_carries_coder_binding() {
        let args = vec![("coder_enabled".into(), "true".into())];
        let doc = layout_doc_for("alice/api", &args, None);
        let (_, config) = zellij_utils::input::layout::Layout::from_stringified_layout(
            &doc,
            zellij_utils::input::config::Config::default(),
        )
        .expect("coder layout must parse");
        assert_eq!(
            config.options.default_command.as_deref(),
            Some(ssh_argv("alice/api").as_slice())
        );
        assert_eq!(config.options.session_serialization, Some(false));
        assert!(doc.contains("coder_enabled \"true\""));
    }

    #[test]
    fn generated_layout_inherits_custom_base() {
        let base = "layout {\n    pane borderless=true\n}";
        let doc = layout_doc_for("alice/api", &[], Some(base));
        assert!(doc.starts_with(base));
        assert!(doc.contains("default_command \"coder\" \"ssh\" \"alice/api\""));
        let (_, config) = zellij_utils::input::layout::Layout::from_stringified_layout(
            &doc,
            zellij_utils::input::config::Config::default(),
        )
        .expect("custom Coder layout must parse");
        assert_eq!(
            config.options.default_command.as_deref(),
            Some(ssh_argv("alice/api").as_slice())
        );
        assert_eq!(config.options.session_serialization, Some(false));
    }
}
