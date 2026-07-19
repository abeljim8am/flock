//! Exact recognition of Coder-bound sessions and panes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use zellij_tile::prelude::{PaneAgentStatus, PaneId};

pub const SNAPSHOT_PIPE_NAME: &str = "flock-state-snapshot-v1";
pub const SNAPSHOT_CONTEXT_KEY: &str = "flock_coder_snapshot";
pub const REMOTE_SESSION_NAME: &str = "flock";
pub const GATEWAY_WRAPPER_ARG0: &str = "flock-coder-gateway";
pub const GATEWAY_SCRIPT: &str = r#"trap 'exit 130' INT; trap 'exit 143' TERM; identifier="$1"; while :; do coder ssh -t "$identifier" -- '"$HOME/.local/share/flock/current/flock"' attach --create flock options --default-layout flock-coder-remote; status=$?; [ "$status" -eq 0 ] && exit 0; printf '\nflock: Coder connection lost; retrying in 2s (Ctrl-c to stop)\n' >&2; sleep 2 || exit "$status"; done"#;
const SNAPSHOT_CACHE_PATH: &str = "/data/coder-snapshots-v1.json";

pub fn parse_coder_ssh(argv: &[String]) -> Option<&str> {
    match argv {
        [coder, ssh, identifier]
            if coder == "coder" && ssh == "ssh" && valid_identifier(identifier) =>
        {
            Some(identifier)
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u8,
    pub generated_at_millis: u64,
    pub session: String,
    pub panes: Vec<SnapshotPane>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotPane {
    pub pane_id: PaneId,
    pub label: String,
    pub status: PaneAgentStatus,
    pub focused: bool,
}

impl Snapshot {
    pub fn from_states(
        generated_at_millis: u64,
        session: String,
        states: &BTreeMap<PaneId, PaneAgentStatus>,
    ) -> Self {
        Self {
            version: 1,
            generated_at_millis,
            session,
            panes: states
                .iter()
                .map(|(pane_id, status)| SnapshotPane {
                    pane_id: *pane_id,
                    label: status.label.clone(),
                    status: status.clone(),
                    focused: false,
                })
                .collect(),
        }
    }
}

pub fn snapshot_argv(identifier: &str) -> Vec<String> {
    vec![
        "coder".into(),
        "ssh".into(),
        identifier.into(),
        "--".into(),
        r#""$HOME/.local/share/flock/current/flock""#.into(),
        "--session".into(),
        REMOTE_SESSION_NAME.into(),
        "pipe".into(),
        "--name".into(),
        SNAPSHOT_PIPE_NAME.into(),
    ]
}

pub fn focus_argv(identifier: &str, pane_id: &str) -> Vec<String> {
    vec![
        "coder".into(),
        "ssh".into(),
        identifier.into(),
        "--".into(),
        r#""$HOME/.local/share/flock/current/flock""#.into(),
        "--session".into(),
        REMOTE_SESSION_NAME.into(),
        "action".into(),
        "focus-pane-id".into(),
        pane_id.into(),
    ]
}

pub fn snapshot_context(identifier: &str) -> BTreeMap<String, String> {
    BTreeMap::from_iter([(SNAPSHOT_CONTEXT_KEY.into(), identifier.into())])
}

pub fn parse_snapshot(raw: &str) -> Result<Snapshot, String> {
    let snapshot: Snapshot = serde_json::from_str(raw).map_err(|error| error.to_string())?;
    if snapshot.version != 1 {
        return Err(format!("unsupported snapshot version {}", snapshot.version));
    }
    Ok(snapshot)
}

pub fn load_cached(identifier: &str) -> Option<Snapshot> {
    let raw = std::fs::read_to_string(SNAPSHOT_CACHE_PATH).ok()?;
    let snapshots: BTreeMap<String, Snapshot> = serde_json::from_str(&raw).ok()?;
    snapshots.get(identifier).cloned()
}

pub fn save_cached(identifier: &str, snapshot: &Snapshot) {
    let mut snapshots: BTreeMap<String, Snapshot> = std::fs::read_to_string(SNAPSHOT_CACHE_PATH)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    snapshots.insert(identifier.to_owned(), snapshot.clone());
    if let Ok(raw) = serde_json::to_string(&snapshots) {
        let _ = std::fs::write(SNAPSHOT_CACHE_PATH, raw);
    }
}

fn valid_identifier(identifier: &str) -> bool {
    let mut parts = identifier.split('/');
    parts.next().is_some_and(|part| !part.is_empty())
        && parts.next().is_some_and(|part| !part.is_empty())
        && parts.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zellij_tile::prelude::AgentRunState;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    #[test]
    fn recognizes_exact_owner_workspace_binding() {
        assert_eq!(
            parse_coder_ssh(&argv(&["coder", "ssh", "alice/api"])),
            Some("alice/api")
        );
        assert_eq!(
            parse_coder_ssh(&argv(&[
                "sh",
                "-c",
                GATEWAY_SCRIPT,
                GATEWAY_WRAPPER_ARG0,
                "alice/api",
            ])),
            Some("alice/api")
        );
    }

    #[test]
    fn rejects_ambiguous_or_non_binding_commands() {
        assert_eq!(parse_coder_ssh(&argv(&["coder", "ssh", "api"])), None);
        assert_eq!(
            parse_coder_ssh(&argv(&["coder", "ssh", "alice/api", "ls"])),
            None
        );
        assert_eq!(parse_coder_ssh(&argv(&["coder", "list"])), None);
    }

    #[test]
    fn snapshot_round_trips_and_rejects_future_versions() {
        let states = BTreeMap::from_iter([(
            PaneId::Terminal(7),
            PaneAgentStatus {
                state: AgentRunState::Blocked,
                label: "codex".into(),
                seen: false,
            },
        )]);
        let snapshot = Snapshot::from_states(42, "flock".into(), &states);
        let raw = serde_json::to_string(&snapshot).unwrap();
        assert_eq!(parse_snapshot(&raw).unwrap(), snapshot);
        let future = raw.replacen("\"version\":1", "\"version\":2", 1);
        assert!(parse_snapshot(&future).unwrap_err().contains("version 2"));
    }

    #[test]
    fn snapshot_and_focus_commands_keep_identifiers_as_single_arguments() {
        let snapshot = snapshot_argv("alice/api");
        assert_eq!(&snapshot[..3], &["coder", "ssh", "alice/api"]);
        assert_eq!(
            &snapshot[4..],
            &[
                r#""$HOME/.local/share/flock/current/flock""#,
                "--session",
                REMOTE_SESSION_NAME,
                "pipe",
                "--name",
                SNAPSHOT_PIPE_NAME,
            ]
        );
        let focus = focus_argv("alice/api", "terminal_7");
        assert_eq!(focus[4], r#""$HOME/.local/share/flock/current/flock""#);
        assert_eq!(focus.last().map(String::as_str), Some("terminal_7"));
    }
}
