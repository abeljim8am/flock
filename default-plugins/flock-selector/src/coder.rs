//! Coder workspace support, backed by the currently authenticated `coder` CLI
//! deployment. Workspaces bind to sessions through the exact default command
//! `coder ssh owner/name`.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::fuzzy::fuzzy_match;

pub const LIST_CONTEXT_KEY: &str = "flock_coder_list";
pub const STOP_CONTEXT_KEY: &str = "flock_coder_stop";
pub const TEMPLATE_LIST_CONTEXT_KEY: &str = "flock_coder_template_list";
pub const CREATE_CONTEXT_KEY: &str = "flock_coder_create";
pub const BOOTSTRAP_CONTEXT_KEY: &str = "flock_coder_bootstrap";

pub const GATEWAY_WRAPPER_ARG0: &str = "flock-coder-gateway";
pub const RELEASE_TAG: &str = "v26.3.0";
pub const RELEASE_BASE_URL: &str = "https://github.com/abeljim8am/flock/releases/download";

/// The host-side gateway is deliberately an exact argv shape. Both Flock
/// plugins recognize this as the durable Coder binding, while the shell body
/// remains an implementation detail that can evolve without changing identity.
pub const GATEWAY_SCRIPT: &str = r#"trap 'exit 130' INT; trap 'exit 143' TERM; identifier="$1"; while :; do coder ssh -t "$identifier" -- '"$HOME/.local/share/flock/current/flock"' attach --create flock options --default-layout flock-coder-remote; status=$?; [ "$status" -eq 0 ] && exit 0; printf '\nflock: Coder connection lost; retrying in 2s (Ctrl-c to stop)\n' >&2; sleep 2 || exit "$status"; done"#;

const CACHE_PATH: &str = "/data/coder-workspaces.json";
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CoderTemplate {
    pub name: String,
    pub display_name: String,
    pub organization: String,
}

impl CoderTemplate {
    pub fn label(&self) -> &str {
        if self.display_name.is_empty() {
            &self.name
        } else {
            &self.display_name
        }
    }
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

#[cfg(test)]
pub fn ssh_argv(identifier: &str) -> Vec<String> {
    vec!["coder".into(), "ssh".into(), identifier.into()]
}

pub fn gateway_argv(identifier: &str) -> Vec<String> {
    remote_pty_argv(identifier, None, None)
}

pub fn remote_pty_argv(
    identifier: &str,
    pane_id: Option<&str>,
    executable: Option<&str>,
) -> Vec<String> {
    let mut argv = vec![
        executable.unwrap_or("flock").into(),
        "remote-agent".into(),
        "coder-pty".into(),
        "--workspace".into(),
        identifier.into(),
    ];
    if let Some(pane_id) = pane_id {
        argv.extend(["--pane-id".into(), pane_id.into()]);
    }
    argv
}

pub fn parse_gateway(argv: &[String]) -> Option<&str> {
    match argv {
        [flock, remote_agent, coder_pty, args @ ..]
            if is_flock_executable(flock)
                && remote_agent == "remote-agent"
                && coder_pty == "coder-pty" =>
        {
            parse_remote_pty_args(args)
        },
        [sh, dash_c, script, arg0, identifier]
            if sh == "sh"
                && dash_c == "-c"
                && script == GATEWAY_SCRIPT
                && arg0 == GATEWAY_WRAPPER_ARG0
                && valid_identifier(identifier) =>
        {
            Some(identifier)
        },
        _ => None,
    }
}

fn parse_remote_pty_args(args: &[String]) -> Option<&str> {
    if let [workspace] = args {
        return valid_identifier(workspace).then_some(workspace);
    }
    let mut chunks = args.chunks_exact(2);
    let mut workspace = None;
    for chunk in &mut chunks {
        match chunk {
            [flag, value] if flag == "--workspace" && workspace.is_none() => {
                workspace = Some(value.as_str());
            },
            [flag, _] if flag == "--pane-id" || flag == "--cwd" => {},
            _ => return None,
        }
    }
    if !chunks.remainder().is_empty() {
        return None;
    }
    workspace.filter(|workspace| valid_identifier(workspace))
}

fn is_flock_executable(executable: &str) -> bool {
    executable == "flock" || (Path::new(executable).is_absolute() && !executable.is_empty())
}

/// Bootstrap the fork into a versioned user directory in the workspace. The
/// static musl build is intentionally Linux/x86_64-only for v1. Installation is
/// atomic and checksum verified; repeated calls are cheap.
pub fn bootstrap_argv(identifier: &str, debug_binary: Option<&str>) -> Vec<String> {
    if let Some(debug_binary) = debug_binary {
        return debug_bootstrap_argv(identifier, debug_binary);
    }
    let script = format!(
        r#"set -eu
[ "$(uname -s)" = Linux ] && [ "$(uname -m)" = x86_64 ] || {{ echo "flock: persistent Coder sessions currently require Linux x86_64" >&2; exit 65; }}
root="$HOME/.local/share/flock"
dest="$root/{tag}"
[ -x "$dest/flock" ] && {{ mkdir -p "$root" "$HOME/.local/bin"; ln -sfn "$dest" "$root/current"; ln -sfn "$dest/flock" "$HOME/.local/bin/flock"; exit 0; }}
tmp="$root/.bootstrap.$$"
mkdir -p "$tmp" "$dest"
trap "rm -rf \"$tmp\"" EXIT HUP INT TERM
base="{base}/{tag}"
archive="$tmp/flock.tar.gz"
checksum="$tmp/flock.sha256sum"
fetch() {{ if command -v curl >/dev/null 2>&1; then curl -fsSL "$1" -o "$2"; elif command -v wget >/dev/null 2>&1; then wget -qO "$2" "$1"; elif command -v python3 >/dev/null 2>&1; then python3 -c "import sys,urllib.request; urllib.request.urlretrieve(sys.argv[1],sys.argv[2])" "$1" "$2"; else echo "flock: curl, wget, or python3 is required to install remote Zellij" >&2; exit 69; fi; }}
fetch "$base/flock-x86_64-unknown-linux-musl.tar.gz" "$archive"
fetch "$base/flock-x86_64-unknown-linux-musl.sha256sum" "$checksum"
tar -xzf "$archive" -C "$tmp"
IFS=" " read -r expected _ < "$checksum"
actual="$(sha256sum "$tmp/flock")"
actual="${{actual%% *}}"
[ -n "$expected" ] && [ "$expected" = "$actual" ] || {{ echo "flock: remote Zellij checksum verification failed" >&2; exit 74; }}
install -m 0755 "$tmp/flock" "$dest/flock.new"
mv -f "$dest/flock.new" "$dest/flock"
mkdir -p "$HOME/.local/bin"
ln -sfn "$dest" "$root/current"
ln -sfn "$dest/flock" "$HOME/.local/bin/flock""#,
        tag = RELEASE_TAG,
        base = RELEASE_BASE_URL,
    );
    vec![
        "coder".into(),
        "ssh".into(),
        identifier.into(),
        "--".into(),
        "sh".into(),
        "-c".into(),
        quote_coder_remote_arg(&script),
    ]
}

/// Stream an explicitly selected local Linux x86_64 binary over `coder ssh`.
/// The binary path and workspace are positional shell arguments, never
/// interpolated into either script. This keeps spaces and shell metacharacters
/// inert while allowing the remote `cat` to receive the executable on stdin.
fn debug_bootstrap_argv(identifier: &str, debug_binary: &str) -> Vec<String> {
    let remote_script = format!(
        r#"set -eu
[ "$(uname -s)" = Linux ] && [ "$(uname -m)" = x86_64 ] || {{ echo "flock: debug remote agent requires Linux x86_64" >&2; exit 65; }}
root="$HOME/.local/share/flock"
dest="$root/{tag}-debug"
tmp="$dest/.flock.$$"
mkdir -p "$dest" "$HOME/.local/bin"
trap "rm -f \"$tmp\"" EXIT HUP INT TERM
cat > "$tmp"
chmod 0755 "$tmp"
"$tmp" --version >/dev/null
mv -f "$tmp" "$dest/flock"
ln -sfn "$dest" "$root/current"
ln -sfn "$dest/flock" "$HOME/.local/bin/flock""#,
        tag = RELEASE_TAG,
    );
    let local_script = format!(
        r#"set -eu
binary="$1"
workspace="$2"
[ -f "$binary" ] || {{ echo "flock: debug remote agent binary not found: $binary" >&2; exit 66; }}
remote={}
remote="'"$remote"'"
coder ssh "$workspace" -- sh -c "$remote" < "$binary""#,
        quote_coder_remote_arg(&remote_script),
    );
    vec![
        "sh".into(),
        "-c".into(),
        local_script,
        "flock-debug-bootstrap".into(),
        debug_binary.into(),
        identifier.into(),
    ]
}

/// `coder ssh` joins command arguments into a command line for the workspace's
/// configured shell before invoking `sh`. A single-quoted argument is understood
/// by both POSIX shells and Fish, but there is no shared way to escape a single
/// quote inside it. Keep the generated bootstrap script free of single quotes
/// and fail loudly if a future edit violates that transport invariant. Use a
/// non-login `sh`: login-shell logout hooks can overwrite a successful exit code.
fn quote_coder_remote_arg(value: &str) -> String {
    assert!(
        !value.contains('\''),
        "Coder remote scripts must not contain single quotes"
    );
    format!("'{value}'")
}

pub fn bootstrap_context(identifier: &str) -> BTreeMap<String, String> {
    BTreeMap::from_iter([(BOOTSTRAP_CONTEXT_KEY.into(), identifier.into())])
}

pub fn stop_argv(identifier: &str) -> Vec<String> {
    vec![
        "coder".into(),
        "stop".into(),
        "-y".into(),
        identifier.into(),
    ]
}

pub fn template_list_argv() -> Vec<String> {
    vec![
        "coder".into(),
        "templates".into(),
        "list".into(),
        "--output".into(),
        "json".into(),
    ]
}

pub fn create_argv(
    name: &str,
    template: &CoderTemplate,
    dotfiles: Option<(&str, &str)>,
    dotfiles_branch: Option<(&str, &str)>,
) -> Vec<String> {
    let mut argv = vec![
        "coder".into(),
        "create".into(),
        name.into(),
        "--template".into(),
        template.name.clone(),
        "--org".into(),
        template.organization.clone(),
        "--preset".into(),
        "none".into(),
        "--use-parameter-defaults".into(),
        "--yes".into(),
        "--no-wait".into(),
    ];
    if let Some((parameter, uri)) = dotfiles {
        argv.extend([
            "--parameter-default".into(),
            format!("{}={}", parameter, uri),
        ]);
    }
    if let Some((parameter, branch)) = dotfiles_branch {
        argv.extend([
            "--parameter-default".into(),
            format!("{}={}", parameter, branch),
        ]);
    }
    argv
}

pub fn template_list_context() -> BTreeMap<String, String> {
    BTreeMap::from_iter([(TEMPLATE_LIST_CONTEXT_KEY.into(), String::new())])
}

pub fn create_context(name: &str) -> BTreeMap<String, String> {
    BTreeMap::from_iter([(CREATE_CONTEXT_KEY.into(), name.into())])
}

pub fn parse_coder_ssh(argv: &[String]) -> Option<&str> {
    match argv {
        [coder, ssh, identifier]
            if coder == "coder" && ssh == "ssh" && valid_identifier(identifier) =>
        {
            Some(identifier)
        },
        _ => parse_gateway(argv),
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
    executable: Option<&str>,
) -> String {
    let command = remote_pty_argv(identifier, None, executable);
    let backend = serde_json::json!({
        "provider": "coder",
        "workspace": identifier,
        "local_session_id": "",
        "legacy": false,
    })
    .to_string();
    let options = format!(
        "remote_backend {}\nsession_serialization true\nshow_startup_tips false\nshow_release_notes false\n",
        kdl_quote(&backend)
    );
    crate::codespaces::layout_doc_with_options(&command, sidebar_args, base_layout, &options)
}

fn kdl_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
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

pub fn parse_template_list_json(raw: &str) -> Result<Vec<CoderTemplate>, CoderError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|error| CoderError::MalformedJson(error.to_string()))?;
    let entries = value
        .as_array()
        .ok_or_else(|| CoderError::MalformedJson("expected a JSON array".into()))?;
    let mut templates = Vec::with_capacity(entries.len());
    for entry in entries {
        // Newer Coder CLI versions serialize the API response wrapper, placing
        // the template fields under a `Template` key. Older versions returned
        // the template object directly, so accept both response shapes.
        let entry = entry.get("Template").unwrap_or(entry);
        let Some(name) = string_field(entry, &["name"]).filter(|name| !name.trim().is_empty())
        else {
            continue;
        };
        let organization = string_field(entry, &["organization_name", "organizationName"])
            .or_else(|| {
                entry.get("organization").and_then(|organization| {
                    string_field(organization, &["name", "display_name", "displayName"])
                })
            })
            .unwrap_or_default();
        if organization.trim().is_empty() {
            continue;
        }
        let display_name = string_field(entry, &["display_name", "displayName"])
            .filter(|display_name| !display_name.trim().is_empty())
            .unwrap_or_else(|| name.clone());
        templates.push(CoderTemplate {
            name,
            display_name,
            organization,
        });
    }
    templates.sort_by(|a, b| {
        a.label()
            .to_ascii_lowercase()
            .cmp(&b.label().to_ascii_lowercase())
            .then_with(|| a.organization.cmp(&b.organization))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(templates)
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
    load_cache_from(Path::new(CACHE_PATH))
}

fn load_cache_from(path: &Path) -> Vec<CoderWorkspace> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<CacheEnvelope>(&raw).ok())
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateError {
    CliMissing,
    NotAuthenticated,
    DuplicateName,
    Validation(String),
    Other(String),
}

pub fn classify_create_error(exit_code: Option<i32>, stderr: &str) -> CreateError {
    let lower = stderr.to_ascii_lowercase();
    if exit_code == Some(127)
        || lower.contains("command not found")
        || lower.contains("no such file or directory")
    {
        return CreateError::CliMissing;
    }
    if lower.contains("coder login")
        || lower.contains("not logged in")
        || lower.contains("not authenticated")
        || lower.contains("unauthorized")
        || lower.contains("status code 401")
    {
        return CreateError::NotAuthenticated;
    }
    if (lower.contains("workspace") && lower.contains("already exists"))
        || lower.contains("name is already in use")
    {
        return CreateError::DuplicateName;
    }
    if lower.contains("invalid workspace name")
        || lower.contains("validation failed")
        || lower.contains("must match")
        || lower.contains("is not a valid")
    {
        return CreateError::Validation(last_nonempty_line(stderr));
    }
    CreateError::Other(last_nonempty_line(stderr))
}

fn last_nonempty_line(stderr: &str) -> String {
    stderr
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreatePhase {
    Templates,
    Name,
    Submitting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateWizard {
    pub phase: CreatePhase,
    pub templates: Vec<CoderTemplate>,
    pub templates_loading: bool,
    pub template_error: Option<CoderError>,
    pub filter: String,
    pub selected: usize,
    pub template: Option<CoderTemplate>,
    pub workspace_name: String,
    pub create_error: Option<CreateError>,
    pub dotfiles_configured: bool,
}

impl CreateWizard {
    pub fn new(templates: Option<Vec<CoderTemplate>>, dotfiles_configured: bool) -> Self {
        Self {
            phase: CreatePhase::Templates,
            templates_loading: templates.is_none(),
            templates: templates.unwrap_or_default(),
            template_error: None,
            filter: String::new(),
            selected: 0,
            template: None,
            workspace_name: String::new(),
            create_error: None,
            dotfiles_configured,
        }
    }

    pub fn set_templates(&mut self, templates: Vec<CoderTemplate>) {
        self.templates = templates;
        self.templates_loading = false;
        self.template_error = None;
        self.selected = 0;
    }

    pub fn set_template_error(&mut self, error: CoderError) {
        self.templates_loading = false;
        self.template_error = Some(error);
    }

    pub fn filtered_templates(&self) -> Vec<&CoderTemplate> {
        let query = self.filter.trim();
        let mut ranked: Vec<(i32, &CoderTemplate)> = self
            .templates
            .iter()
            .filter_map(|template| {
                let display = fuzzy_match(query, template.label());
                let name = fuzzy_match(query, &template.name);
                let organization = fuzzy_match(query, &template.organization);
                if !query.is_empty()
                    && display.is_none()
                    && name.is_none()
                    && organization.is_none()
                {
                    return None;
                }
                let score = display
                    .map(|m| m.score + 8)
                    .into_iter()
                    .chain(name.map(|m| m.score + 4))
                    .chain(organization.map(|m| m.score))
                    .max()
                    .unwrap_or(0);
                Some((score, template))
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.label().cmp(b.1.label()))
                .then_with(|| a.1.organization.cmp(&b.1.organization))
        });
        ranked.into_iter().map(|(_, template)| template).collect()
    }

    pub fn select_current_template(&mut self) -> bool {
        let selected = self
            .filtered_templates()
            .get(self.selected)
            .map(|template| (*template).clone());
        if let Some(template) = selected {
            self.template = Some(template);
            self.phase = CreatePhase::Name;
            self.create_error = None;
            true
        } else {
            false
        }
    }

    pub fn back(&mut self) -> bool {
        match self.phase {
            CreatePhase::Templates => false,
            CreatePhase::Name => {
                self.phase = CreatePhase::Templates;
                self.create_error = None;
                true
            },
            CreatePhase::Submitting => true,
        }
    }

    pub fn begin_submit(&mut self) -> Option<(String, CoderTemplate)> {
        if self.phase != CreatePhase::Name || self.workspace_name.trim().is_empty() {
            return None;
        }
        let template = self.template.clone()?;
        let name = self.workspace_name.trim().to_owned();
        self.workspace_name = name.clone();
        self.phase = CreatePhase::Submitting;
        self.create_error = None;
        Some((name, template))
    }

    pub fn fail_submit(&mut self, error: CreateError) {
        self.phase = CreatePhase::Name;
        self.create_error = Some(error);
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

    fn template(org: &str, name: &str, display_name: &str) -> CoderTemplate {
        CoderTemplate {
            organization: org.into(),
            name: name.into(),
            display_name: display_name.into(),
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
    fn parses_templates_with_field_variants_and_duplicate_names() {
        let parsed = parse_template_list_json(
            r#"[
          {"name":"rust","display_name":"Rust Dev","organization_name":"acme"},
          {"name":"rust","displayName":"Rust Platform","organizationName":"platform"},
          {"name":"web","organization":{"name":"labs"}},
          {"name":"missing-org"}
        ]"#,
        )
        .unwrap();
        assert_eq!(parsed.len(), 3);
        assert!(parsed.contains(&template("acme", "rust", "Rust Dev")));
        assert!(parsed.contains(&template("platform", "rust", "Rust Platform")));
        assert!(parsed.contains(&template("labs", "web", "web")));
        assert!(parse_template_list_json("[]").unwrap().is_empty());
        assert!(matches!(
            parse_template_list_json("{"),
            Err(CoderError::MalformedJson(_))
        ));
    }

    #[test]
    fn parses_templates_wrapped_by_current_coder_cli() {
        let parsed = parse_template_list_json(
            r#"[
              {
                "Template": {
                  "id": "31715e2d-421e-4477-97c7-3468c34197ae",
                  "organization_name": "coder",
                  "organization_display_name": "Coder",
                  "name": "wooli",
                  "display_name": ""
                }
              }
            ]"#,
        )
        .unwrap();

        assert_eq!(parsed, vec![template("coder", "wooli", "wooli")]);
    }

    #[test]
    fn create_argv_is_exact_and_values_remain_single_arguments() {
        let selected = template("my org; echo nope", "rust template", "Rust");
        assert_eq!(
            create_argv("my workspace $(false)", &selected, None, None),
            vec![
                "coder",
                "create",
                "my workspace $(false)",
                "--template",
                "rust template",
                "--org",
                "my org; echo nope",
                "--preset",
                "none",
                "--use-parameter-defaults",
                "--yes",
                "--no-wait"
            ]
        );
        let uri = "https://example.test/dot files.git?x=$(nope)&y=1";
        let default_parameter = create_argv(
            "demo",
            &selected,
            Some(("dotfiles_uri", uri)),
            Some(("dotfiles_branch", "main")),
        );
        assert_eq!(
            &default_parameter[12..],
            &[
                "--parameter-default",
                &format!("dotfiles_uri={uri}"),
                "--parameter-default",
                "dotfiles_branch=main",
            ]
        );
        let custom = create_argv(
            "demo",
            &selected,
            Some(("personal_dotfiles", uri)),
            Some(("personal_branch", "release; $(nope)")),
        );
        assert_eq!(
            &custom[12..],
            &[
                "--parameter-default",
                &format!("personal_dotfiles={uri}"),
                "--parameter-default",
                "personal_branch=release; $(nope)",
            ]
        );
    }

    #[test]
    fn creation_wizard_filters_navigates_submits_once_and_retries() {
        let mut wizard = CreateWizard::new(None, true);
        assert!(wizard.templates_loading);
        wizard.set_templates(vec![
            template("acme", "rust", "Rust Dev"),
            template("labs", "web", "Web"),
        ]);
        wizard.filter = "web".into();
        assert_eq!(wizard.filtered_templates()[0].organization, "labs");
        assert!(wizard.select_current_template());
        wizard.workspace_name = "demo".into();
        assert_eq!(
            wizard.begin_submit(),
            Some(("demo".into(), template("labs", "web", "Web")))
        );
        assert_eq!(
            wizard.begin_submit(),
            None,
            "duplicate Enter is ignored while submitting"
        );
        wizard.fail_submit(CreateError::DuplicateName);
        assert_eq!(wizard.phase, CreatePhase::Name);
        assert_eq!(wizard.workspace_name, "demo");
        assert_eq!(wizard.begin_submit().unwrap().0, "demo");
        wizard.fail_submit(CreateError::Validation("bad name".into()));
        assert!(wizard.back());
        assert_eq!(wizard.phase, CreatePhase::Templates);
        assert_eq!(wizard.template.as_ref().unwrap().organization, "labs");
        assert!(!wizard.back(), "Esc from templates cancels the wizard");
    }

    #[test]
    fn create_errors_are_actionable() {
        assert_eq!(
            classify_create_error(Some(1), "workspace already exists"),
            CreateError::DuplicateName
        );
        assert_eq!(
            classify_create_error(Some(1), "please run coder login"),
            CreateError::NotAuthenticated
        );
        assert!(matches!(
            classify_create_error(Some(1), "invalid workspace name: nope"),
            CreateError::Validation(_)
        ));
        assert_eq!(
            classify_create_error(Some(1), "first\nCLI exploded"),
            CreateError::Other("CLI exploded".into())
        );
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
    fn cache_loads_stale_results_for_background_refresh() {
        let path =
            std::env::temp_dir().join(format!("flock-coder-cache-{}.json", std::process::id()));
        let cache = CacheEnvelope {
            saved_at: 100,
            workspaces: vec![workspace("alice", "api")],
        };
        std::fs::write(&path, serde_json::to_string(&cache).unwrap()).unwrap();
        assert_eq!(load_cache_from(&path).len(), 1);
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
        let doc = layout_doc_for("alice/api", &args, None, None);
        let (_, config) = zellij_utils::input::layout::Layout::from_stringified_layout(
            &doc,
            zellij_utils::input::config::Config::default(),
        )
        .expect("coder layout must parse");
        assert_eq!(
            config.options.default_command.as_deref(),
            Some(gateway_argv("alice/api").as_slice())
        );
        assert!(matches!(
            config.options.remote_backend,
            Some(zellij_tile::prelude::RemoteBackend::Coder {
                ref workspace,
                legacy: false,
                ..
            }) if workspace == "alice/api"
        ));
        assert_eq!(config.options.session_serialization, Some(true));
        assert_eq!(config.options.show_startup_tips, Some(false));
        assert_eq!(config.options.show_release_notes, Some(false));
        assert!(doc.contains("coder_enabled \"true\""));
    }

    #[test]
    fn generated_gateway_layout_preserves_remote_content_base() {
        let base = "layout {\n    pane borderless=true {\n        plugin location=\"zellij:custom-topbar\"\n    }\n}";
        let doc = layout_doc_for("alice/api", &[], Some(base), None);
        assert!(doc.starts_with(base));
        assert!(doc.contains("zellij:custom-topbar"));
        assert!(doc.contains("remote-agent"));
        let (_, config) = zellij_utils::input::layout::Layout::from_stringified_layout(
            &doc,
            zellij_utils::input::config::Config::default(),
        )
        .expect("custom Coder layout must parse");
        assert_eq!(
            config.options.default_command.as_deref(),
            Some(gateway_argv("alice/api").as_slice())
        );
        assert_eq!(config.options.session_serialization, Some(true));
    }

    #[test]
    fn gateway_binding_and_bootstrap_are_exact_and_safe() {
        let gateway = gateway_argv("alice/api");
        assert_eq!(parse_gateway(&gateway), Some("alice/api"));
        assert_eq!(parse_coder_ssh(&gateway), Some("alice/api"));
        assert_eq!(gateway[0], "flock");
        assert_eq!(gateway[1], "remote-agent");
        assert_eq!(gateway[2], "coder-pty");
        let debug_gateway =
            remote_pty_argv("alice/api", None, Some("/workspace/target/debug/flock"));
        assert_eq!(debug_gateway[0], "/workspace/target/debug/flock");
        assert_eq!(parse_gateway(&debug_gateway), Some("alice/api"));
        let mut gateway_with_cwd = debug_gateway.clone();
        gateway_with_cwd.extend(["--cwd".into(), "/workspace/api".into()]);
        assert_eq!(parse_gateway(&gateway_with_cwd), Some("alice/api"));
        let bootstrap = bootstrap_argv("alice/api", None);
        assert_eq!(&bootstrap[..3], &["coder", "ssh", "alice/api"]);
        assert_eq!(&bootstrap[3..6], &["--", "sh", "-c"]);
        assert!(bootstrap
            .last()
            .unwrap()
            .contains("x86_64-unknown-linux-musl"));
        assert!(bootstrap.last().unwrap().contains("sha256sum"));
        assert!(!bootstrap.last().unwrap().contains("alice/api"));
        assert!(bootstrap.last().unwrap().starts_with("'set -eu"));
        assert!(bootstrap.last().unwrap().ends_with('\''));
        assert_eq!(bootstrap.last().unwrap().matches('\'').count(), 2);
        assert_eq!(quote_coder_remote_arg("printf %s"), "'printf %s'");

        let debug = bootstrap_argv(
            "alice/api",
            Some("/workspace/target/debug/flock with spaces"),
        );
        assert_eq!(&debug[..2], &["sh", "-c"]);
        assert_eq!(debug[3], "flock-debug-bootstrap");
        assert_eq!(debug[4], "/workspace/target/debug/flock with spaces");
        assert_eq!(debug[5], "alice/api");
        assert!(debug[2].contains("coder ssh \"$workspace\""));
        assert!(debug[2].contains("remote=\"'\"$remote\"'\""));
        assert!(debug[2].contains("< \"$binary\""));
        assert!(!debug[2].contains("/workspace/target/debug"));
        assert!(!debug[2].contains("alice/api"));
    }

    #[test]
    #[should_panic(expected = "Coder remote scripts must not contain single quotes")]
    fn coder_remote_arg_rejects_fish_incompatible_single_quotes() {
        quote_coder_remote_arg("printf '%s'");
    }
}
