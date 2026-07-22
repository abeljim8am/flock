//! Saved SSH host support. Unlike the coder/codespaces lists, which cache CLI
//! output, the host list here is user-authored source of truth: entries are
//! created and edited in the picker's wizard and persisted to the plugin's
//! data mount. A host binds to a session through the unified
//! `flock remote-agent remote-pty --provider ssh` pane command.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::coder::{ExistingSession, OpenAction};
use crate::fuzzy::fuzzy_match;
use crate::remote_bootstrap;

pub const BOOTSTRAP_CONTEXT_KEY: &str = "flock_ssh_bootstrap";

const HOSTS_PATH: &str = "/data/ssh-hosts.json";
const FALLBACK_NAME: &str = "ssh";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SshHost {
    /// Display label; identity keys on `destination`, so renaming never
    /// orphans a bound session's panes.
    pub name: String,
    /// `user@host` or a `~/.ssh/config` alias.
    pub destination: String,
    /// Extra `ssh` arguments, one token each (e.g. `-p 2222` is two tokens).
    /// Tokens must stay whitespace-free: the pane argv is re-tokenized on
    /// whitespace when sessions are serialized and recognized.
    #[serde(default)]
    pub extra_args: Vec<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct HostsEnvelope {
    saved_at: u64,
    hosts: Vec<SshHost>,
}

pub fn load_hosts() -> Vec<SshHost> {
    load_hosts_from(Path::new(HOSTS_PATH))
}

fn load_hosts_from(path: &Path) -> Vec<SshHost> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<HostsEnvelope>(&raw).ok())
        .map(|envelope| envelope.hosts)
        .unwrap_or_default()
}

pub fn save_hosts(hosts: &[SshHost]) {
    let envelope = HostsEnvelope {
        saved_at: now_secs(),
        hosts: hosts.to_vec(),
    };
    if let Ok(raw) = serde_json::to_string(&envelope) {
        let _ = std::fs::write(HOSTS_PATH, raw);
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
pub struct RankedSshHost<'a> {
    pub host: &'a SshHost,
    pub rank: i32,
    pub name_ranges: Vec<(usize, usize)>,
    pub destination_ranges: Vec<(usize, usize)>,
}

pub fn rank<'a>(hosts: &'a [SshHost], query: &str) -> Vec<RankedSshHost<'a>> {
    let query = query.trim();
    let mut ranked = Vec::with_capacity(hosts.len());
    for host in hosts {
        let name_match = fuzzy_match(query, &host.name);
        let destination_match = fuzzy_match(query, &host.destination);
        if !query.is_empty() && name_match.is_none() && destination_match.is_none() {
            continue;
        }
        let rank = name_match
            .as_ref()
            .map(|result| result.score + NAME_MATCH_BONUS)
            .into_iter()
            .chain(destination_match.as_ref().map(|result| result.score))
            .max()
            .unwrap_or(0);
        ranked.push(RankedSshHost {
            host,
            rank,
            name_ranges: name_match.map(|result| result.ranges).unwrap_or_default(),
            destination_ranges: destination_match
                .map(|result| result.ranges)
                .unwrap_or_default(),
        });
    }
    ranked.sort_by(|a, b| {
        b.rank
            .cmp(&a.rank)
            .then_with(|| a.host.name.cmp(&b.host.name))
            .then_with(|| a.host.destination.cmp(&b.host.destination))
    });
    ranked
}

pub fn remote_pty_argv(
    host: &SshHost,
    pane_id: Option<&str>,
    executable: Option<&str>,
) -> Vec<String> {
    let mut argv = vec![
        executable.unwrap_or("flock").into(),
        "remote-agent".into(),
        "remote-pty".into(),
        "--provider".into(),
        "ssh".into(),
        "--destination".into(),
        host.destination.clone(),
    ];
    for arg in &host.extra_args {
        argv.extend(["--ssh-arg".into(), arg.clone()]);
    }
    if let Some(pane_id) = pane_id {
        argv.extend(["--pane-id".into(), pane_id.into()]);
    }
    argv
}

/// Inverse of [`remote_pty_argv`]: the bound destination, or `None` when the
/// command is anything else. Must stay in lockstep with the recognizers in
/// `zellij-server` — a session's `default_command` is matched byte-for-byte.
pub fn parse_gateway(argv: &[String]) -> Option<&str> {
    match argv {
        [flock, remote_agent, remote_pty, args @ ..]
            if is_flock_executable(flock)
                && remote_agent == "remote-agent"
                && remote_pty == "remote-pty" =>
        {
            let mut chunks = args.chunks_exact(2);
            let mut provider = None;
            let mut destination = None;
            for chunk in &mut chunks {
                match chunk {
                    [flag, value] if flag == "--provider" && provider.is_none() => {
                        provider = Some(value.as_str());
                    },
                    [flag, value] if flag == "--destination" && destination.is_none() => {
                        destination = Some(value.as_str());
                    },
                    [flag, _] if flag == "--ssh-arg" || flag == "--pane-id" || flag == "--cwd" => {
                    },
                    _ => return None,
                }
            }
            if !chunks.remainder().is_empty() || provider != Some("ssh") {
                return None;
            }
            destination.filter(|destination| valid_destination(destination))
        },
        _ => None,
    }
}

fn is_flock_executable(executable: &str) -> bool {
    executable == "flock" || (Path::new(executable).is_absolute() && !executable.is_empty())
}

pub fn valid_destination(destination: &str) -> bool {
    !destination.is_empty()
        && !destination.starts_with('-')
        && !destination.chars().any(char::is_whitespace)
}

pub fn bootstrap_argv(host: &SshHost, debug_binary: Option<&str>) -> Vec<String> {
    if let Some(debug_binary) = debug_binary {
        return debug_bootstrap_argv(host, debug_binary);
    }
    let mut argv = base_ssh_argv(host);
    argv.extend([
        "--".into(),
        host.destination.clone(),
        "sh".into(),
        "-c".into(),
        remote_bootstrap::quote_remote_script_arg(&remote_bootstrap::install_script()),
    ]);
    argv
}

/// Stream an explicitly selected local binary over `ssh`. The binary path and
/// destination are positional shell arguments, never interpolated into either
/// script; extra ssh args are appended as discrete argv words after them.
fn debug_bootstrap_argv(host: &SshHost, debug_binary: &str) -> Vec<String> {
    let local_script = format!(
        r#"set -eu
binary="$1"
destination="$2"
shift 2
[ -f "$binary" ] || {{ echo "flock: debug remote agent binary not found: $binary" >&2; exit 66; }}
remote={}
remote="'"$remote"'"
ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new "$@" -- "$destination" sh -c "$remote" < "$binary""#,
        remote_bootstrap::quote_remote_script_arg(&remote_bootstrap::debug_install_script()),
    );
    let mut argv = vec![
        "sh".into(),
        "-c".into(),
        local_script,
        "flock-debug-bootstrap".into(),
        debug_binary.into(),
        host.destination.clone(),
    ];
    argv.extend(host.extra_args.iter().cloned());
    argv
}

fn base_ssh_argv(host: &SshHost) -> Vec<String> {
    let mut argv = vec![
        "ssh".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
    ];
    argv.extend(host.extra_args.iter().cloned());
    argv
}

pub fn bootstrap_context(destination: &str) -> BTreeMap<String, String> {
    BTreeMap::from_iter([(BOOTSTRAP_CONTEXT_KEY.into(), destination.into())])
}

pub fn layout_doc_for(
    host: &SshHost,
    sidebar_args: &[(String, String)],
    base_layout: Option<&str>,
    executable: Option<&str>,
) -> String {
    let command = remote_pty_argv(host, None, executable);
    let backend = serde_json::json!({
        "provider": "ssh",
        "name": host.name,
        "destination": host.destination,
        "extra_args": host.extra_args,
        "local_session_id": "",
    })
    .to_string();
    let options = format!(
        "remote_backend {}\nshow_startup_tips false\nshow_release_notes false\n",
        kdl_quote(&backend)
    );
    crate::codespaces::layout_doc_with_options(&command, sidebar_args, base_layout, &options)
}

fn kdl_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn resolve_open(host: &SshHost, sessions: &[ExistingSession]) -> OpenAction {
    if let Some(session) = sessions
        .iter()
        .find(|session| session.bound_workspace.as_deref() == Some(host.destination.as_str()))
    {
        return OpenAction::Switch {
            name: session.name.clone(),
        };
    }
    let taken: HashSet<&str> = sessions
        .iter()
        .map(|session| session.name.as_str())
        .collect();
    let base = sanitize_session_name(&host.name)
        .or_else(|| sanitize_session_name(&host.destination))
        .unwrap_or_else(|| FALLBACK_NAME.into());
    OpenAction::Create {
        name: disambiguate_with_suffix(&base, &taken),
    }
}

fn sanitize_session_name(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    for character in raw.chars() {
        if character.is_whitespace() || character == '/' || character == '@' {
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

/// The add/edit form. Purely local (no CLI round trips), phase per field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HostPhase {
    Name,
    Destination,
    ExtraArgs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostWizard {
    pub phase: HostPhase,
    /// Original name of the entry being edited; `None` when adding.
    pub editing: Option<String>,
    pub name: String,
    pub destination: String,
    pub extra_args_input: String,
    pub error: Option<String>,
}

impl HostWizard {
    pub fn add() -> Self {
        Self {
            phase: HostPhase::Name,
            editing: None,
            name: String::new(),
            destination: String::new(),
            extra_args_input: String::new(),
            error: None,
        }
    }

    pub fn edit(host: &SshHost) -> Self {
        Self {
            phase: HostPhase::Name,
            editing: Some(host.name.clone()),
            name: host.name.clone(),
            destination: host.destination.clone(),
            extra_args_input: host.extra_args.join(" "),
            error: None,
        }
    }

    pub fn field_mut(&mut self) -> &mut String {
        match self.phase {
            HostPhase::Name => &mut self.name,
            HostPhase::Destination => &mut self.destination,
            HostPhase::ExtraArgs => &mut self.extra_args_input,
        }
    }

    /// Advance past the current field. Returns the finished host on the last
    /// phase; `None` (possibly with `self.error` set) otherwise.
    pub fn advance(&mut self, existing: &[SshHost]) -> Option<SshHost> {
        self.error = None;
        match self.phase {
            HostPhase::Name => {
                let name = self.name.trim().to_owned();
                if name.is_empty() {
                    self.error = Some("name cannot be empty".into());
                    return None;
                }
                let duplicate = existing
                    .iter()
                    .any(|host| host.name == name && self.editing.as_deref() != Some(&host.name));
                if duplicate {
                    self.error = Some(format!("a host named {name:?} already exists"));
                    return None;
                }
                self.name = name;
                self.phase = HostPhase::Destination;
                None
            },
            HostPhase::Destination => {
                let destination = self.destination.trim().to_owned();
                if !valid_destination(&destination) {
                    self.error = Some(
                        "destination must be non-empty, without spaces, and cannot start with -"
                            .into(),
                    );
                    return None;
                }
                self.destination = destination;
                self.phase = HostPhase::ExtraArgs;
                None
            },
            HostPhase::ExtraArgs => {
                let extra_args: Vec<String> = self
                    .extra_args_input
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect();
                Some(SshHost {
                    name: self.name.clone(),
                    destination: self.destination.clone(),
                    extra_args,
                })
            },
        }
    }

    /// Step back one phase. Returns `false` when already on the first phase
    /// (the caller closes the wizard).
    pub fn back(&mut self) -> bool {
        self.error = None;
        match self.phase {
            HostPhase::Name => false,
            HostPhase::Destination => {
                self.phase = HostPhase::Name;
                true
            },
            HostPhase::ExtraArgs => {
                self.phase = HostPhase::Destination;
                true
            },
        }
    }
}

/// Apply a finished wizard: replace the edited entry (matched by its original
/// name) or append a new one. Returns the updated list for persistence.
pub fn apply_wizard(
    hosts: &[SshHost],
    wizard_result: SshHost,
    editing: Option<&str>,
) -> Vec<SshHost> {
    let mut hosts = hosts.to_vec();
    match editing {
        Some(original) => {
            if let Some(entry) = hosts.iter_mut().find(|host| host.name == original) {
                *entry = wizard_result;
            } else {
                hosts.push(wizard_result);
            }
        },
        None => hosts.push(wizard_result),
    }
    hosts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, destination: &str, extra: &[&str]) -> SshHost {
        SshHost {
            name: name.into(),
            destination: destination.into(),
            extra_args: extra.iter().map(|arg| arg.to_string()).collect(),
        }
    }

    #[test]
    fn gateway_binding_round_trips_and_is_exact() {
        let entry = host("dev", "abel@dev.example.com", &["-p", "2222"]);
        let gateway = remote_pty_argv(&entry, None, None);
        assert_eq!(
            gateway,
            vec![
                "flock",
                "remote-agent",
                "remote-pty",
                "--provider",
                "ssh",
                "--destination",
                "abel@dev.example.com",
                "--ssh-arg",
                "-p",
                "--ssh-arg",
                "2222",
            ]
        );
        assert_eq!(parse_gateway(&gateway), Some("abel@dev.example.com"));

        let debug_gateway = remote_pty_argv(&entry, Some("uuid-1"), Some("/tmp/debug/flock"));
        assert_eq!(debug_gateway[0], "/tmp/debug/flock");
        assert_eq!(parse_gateway(&debug_gateway), Some("abel@dev.example.com"));

        let mut with_cwd = debug_gateway.clone();
        with_cwd.extend(["--cwd".into(), "/workspace".into()]);
        assert_eq!(parse_gateway(&with_cwd), Some("abel@dev.example.com"));

        // Coder bindings and non-flock commands never match.
        assert_eq!(
            parse_gateway(&crate::coder::remote_pty_argv("alice/api", None, None)),
            None
        );
        assert_eq!(
            parse_gateway(&["ssh".into(), "abel@dev.example.com".into()]),
            None
        );
    }

    #[test]
    fn bootstrap_is_batch_mode_and_quote_safe() {
        let entry = host("dev", "abel@dev.example.com", &["-p", "2222"]);
        let bootstrap = bootstrap_argv(&entry, None);
        assert_eq!(
            &bootstrap[..7],
            &[
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-p",
                "2222",
            ]
        );
        assert_eq!(&bootstrap[7..9], &["--", "abel@dev.example.com"]);
        assert_eq!(&bootstrap[9..11], &["sh", "-c"]);
        let script = bootstrap.last().unwrap();
        assert!(script.contains("x86_64-unknown-linux-musl"));
        assert!(script.contains("aarch64-unknown-linux-musl"));
        assert!(script.starts_with("'set -eu"));
        assert!(script.ends_with('\''));
        assert_eq!(script.matches('\'').count(), 2);
        assert!(!script.contains("abel@dev.example.com"));

        let debug = bootstrap_argv(&entry, Some("/tmp/flock with spaces"));
        assert_eq!(&debug[..2], &["sh", "-c"]);
        assert_eq!(debug[3], "flock-debug-bootstrap");
        assert_eq!(debug[4], "/tmp/flock with spaces");
        assert_eq!(debug[5], "abel@dev.example.com");
        assert_eq!(&debug[6..], &["-p", "2222"]);
        assert!(debug[2].contains("ssh -o BatchMode=yes"));
        assert!(debug[2].contains("\"$@\" -- \"$destination\""));
        assert!(debug[2].contains("< \"$binary\""));
        assert!(!debug[2].contains("/tmp/flock with spaces"));
    }

    #[test]
    fn generated_layout_parses_and_carries_ssh_binding() {
        let entry = host("dev", "abel@dev.example.com", &["-p", "2222"]);
        let args = vec![("ssh_enabled".into(), "true".into())];
        let doc = layout_doc_for(&entry, &args, None, None);
        let (_, config) = zellij_utils::input::layout::Layout::from_stringified_layout(
            &doc,
            zellij_utils::input::config::Config::default(),
        )
        .expect("ssh layout must parse");
        assert_eq!(
            config.options.default_command.as_deref(),
            Some(remote_pty_argv(&entry, None, None).as_slice())
        );
        assert!(matches!(
            config.options.remote_backend,
            Some(zellij_tile::prelude::RemoteBackend::Ssh {
                ref name,
                ref destination,
                ref extra_args,
                ..
            }) if name == "dev"
                && destination == "abel@dev.example.com"
                && extra_args == &["-p".to_owned(), "2222".to_owned()]
        ));
        // Serialization is left unset so the user's local config wins.
        assert_eq!(config.options.session_serialization, None);
    }

    #[test]
    fn resolve_open_switches_to_bound_session_or_creates_unique_name() {
        let entry = host("Dev Box", "abel@dev.example.com", &[]);
        let bound = vec![ExistingSession {
            name: "dev-box".into(),
            bound_workspace: Some("abel@dev.example.com".into()),
        }];
        assert_eq!(
            resolve_open(&entry, &bound),
            OpenAction::Switch {
                name: "dev-box".into()
            }
        );
        let taken = vec![ExistingSession {
            name: "Dev-Box".into(),
            bound_workspace: None,
        }];
        assert_eq!(
            resolve_open(&entry, &taken),
            OpenAction::Create {
                name: "Dev-Box-2".into()
            }
        );
        let unnameable = host("@@@", "abel@dev.example.com", &[]);
        assert_eq!(
            resolve_open(&unnameable, &[]),
            OpenAction::Create {
                name: "abel-dev.example.com".into()
            }
        );
    }

    #[test]
    fn wizard_validates_and_produces_hosts() {
        let existing = vec![host("dev", "abel@dev.example.com", &[])];
        let mut wizard = HostWizard::add();
        assert_eq!(wizard.advance(&existing), None);
        assert!(wizard.error.is_some()); // empty name

        wizard.name = "dev".into();
        assert_eq!(wizard.advance(&existing), None);
        assert!(wizard.error.as_deref().unwrap().contains("already exists"));

        wizard.name = "staging".into();
        assert_eq!(wizard.advance(&existing), None);
        assert_eq!(wizard.phase, HostPhase::Destination);

        wizard.destination = "-oProxyCommand=evil".into();
        assert_eq!(wizard.advance(&existing), None);
        assert!(wizard.error.is_some());
        wizard.destination = "bad destination".into();
        assert_eq!(wizard.advance(&existing), None);
        assert!(wizard.error.is_some());

        wizard.destination = "abel@staging.example.com".into();
        assert_eq!(wizard.advance(&existing), None);
        assert_eq!(wizard.phase, HostPhase::ExtraArgs);

        wizard.extra_args_input = "  -p 2222   -i ~/.ssh/key  ".into();
        let finished = wizard.advance(&existing).unwrap();
        assert_eq!(
            finished,
            host(
                "staging",
                "abel@staging.example.com",
                &["-p", "2222", "-i", "~/.ssh/key"]
            )
        );

        // Editing keeps its own name valid and replaces in place.
        let mut edit = HostWizard::edit(&existing[0]);
        assert_eq!(edit.advance(&existing), None);
        assert_eq!(edit.phase, HostPhase::Destination);
        assert_eq!(edit.advance(&existing), None);
        let finished = edit.advance(&existing).unwrap();
        let updated = apply_wizard(&existing, finished, Some("dev"));
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].name, "dev");

        assert!(edit.back());
        assert_eq!(edit.phase, HostPhase::Destination);
        assert!(edit.back());
        assert_eq!(edit.phase, HostPhase::Name);
        assert!(!edit.back());
    }

    #[test]
    fn hosts_envelope_round_trips() {
        let hosts = vec![host("dev", "abel@dev.example.com", &["-p", "2222"])];
        let envelope = HostsEnvelope {
            saved_at: 1,
            hosts: hosts.clone(),
        };
        let raw = serde_json::to_string(&envelope).unwrap();
        let parsed: HostsEnvelope = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.hosts, hosts);
    }

    #[test]
    fn ranks_by_name_with_bonus_then_alphabetically() {
        let hosts = vec![
            host("api", "abel@api.example.com", &[]),
            host("zeta", "api@zeta.example.com", &[]),
            host("build", "abel@build.example.com", &[]),
        ];
        let ranked = rank(&hosts, "api");
        assert_eq!(ranked[0].host.name, "api"); // name match beats destination match
        assert_eq!(ranked.len(), 2);
        let all = rank(&hosts, "");
        assert_eq!(
            all.iter()
                .map(|entry| entry.host.name.as_str())
                .collect::<Vec<_>>(),
            vec!["api", "build", "zeta"]
        );
    }
}
