use crate::consts::{session_info_cache_file_name, ZELLIJ_CACHE_DIR};
use crate::data::{RemoteBackend, SessionInfo};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Provider transport recorded in a `.close-pending` file: exactly what a
/// detached close worker needs to reach the remote daemon again, nothing more
/// (no display name, no local session id).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum RemoteCloseTransport {
    Coder {
        workspace: String,
    },
    Ssh {
        destination: String,
        #[serde(default)]
        extra_args: Vec<String>,
    },
    Devcontainer {
        workspace_folder: String,
    },
}

impl RemoteCloseTransport {
    /// The provider-scoped identity string fed to `stable_remote_pane_uuid`.
    pub fn identity(&self) -> &str {
        match self {
            Self::Coder { workspace } => workspace,
            Self::Ssh { destination, .. } => destination,
            Self::Devcontainer { workspace_folder } => workspace_folder,
        }
    }

    fn from_backend(backend: &RemoteBackend) -> Self {
        match backend {
            RemoteBackend::Coder { workspace, .. } => Self::Coder {
                workspace: workspace.clone(),
            },
            RemoteBackend::Ssh {
                destination,
                extra_args,
                ..
            } => Self::Ssh {
                destination: destination.clone(),
                extra_args: extra_args.clone(),
            },
            RemoteBackend::Devcontainer {
                workspace_folder, ..
            } => Self::Devcontainer {
                workspace_folder: workspace_folder.clone(),
            },
        }
    }

    fn close_args(&self) -> Vec<String> {
        let mut args = vec!["remote-agent".to_owned(), "remote-close".to_owned()];
        match self {
            Self::Coder { workspace } => {
                args.extend(["--provider".to_owned(), "coder".to_owned()]);
                args.extend(["--workspace".to_owned(), workspace.clone()]);
            },
            Self::Ssh {
                destination,
                extra_args,
            } => {
                args.extend(["--provider".to_owned(), "ssh".to_owned()]);
                args.extend(["--destination".to_owned(), destination.clone()]);
                for arg in extra_args {
                    args.extend(["--ssh-arg".to_owned(), arg.clone()]);
                }
            },
            Self::Devcontainer { workspace_folder } => {
                args.extend(["--provider".to_owned(), "devcontainer".to_owned()]);
                args.extend(["--workspace-folder".to_owned(), workspace_folder.clone()]);
            },
        }
        args
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCloseTarget {
    pub transport: RemoteCloseTransport,
    pub pane_uuid: String,
}

pub fn remote_close_targets(session: &SessionInfo) -> Vec<RemoteCloseTarget> {
    let Some(backend) = session.remote_backend.as_ref() else {
        return Vec::new();
    };
    let transport = RemoteCloseTransport::from_backend(backend);
    session
        .remote_panes
        .values()
        .map(|pane| RemoteCloseTarget {
            transport: transport.clone(),
            pane_uuid: pane.pane_uuid.clone(),
        })
        .collect()
}

pub fn queue_saved_remote_pane_closes(session_name: &str) -> io::Result<usize> {
    let metadata_path = session_info_cache_file_name(session_name);
    let raw = match fs::read_to_string(&metadata_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let targets = remote_close_targets_from_saved_metadata(&raw)?;
    for target in &targets {
        queue_remote_close(&target.transport, &target.pane_uuid)?;
    }
    Ok(targets.len())
}

fn remote_close_targets_from_saved_metadata(raw: &str) -> io::Result<Vec<RemoteCloseTarget>> {
    let session = SessionInfo::from_string(raw, "")
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(remote_close_targets(&session))
}

pub fn queue_remote_close(transport: &RemoteCloseTransport, pane_uuid: &str) -> io::Result<()> {
    let pending_path = pending_dir().join(format!("{pane_uuid}.close-pending"));
    let payload = serde_json::to_string(transport)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    persist_pending(&pending_path, &payload)?;
    spawn_remote_close(transport, pane_uuid);
    Ok(())
}

/// Retry durable requests left behind when a close worker or the host was
/// interrupted. Remote close is idempotent, so duplicate workers are safe.
pub fn recover_pending_remote_closes() -> io::Result<usize> {
    let directory = pending_dir();
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut recovered = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("close-pending") {
            continue;
        }
        let Some(pane_uuid) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Ok(payload) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(transport) = serde_json::from_str::<RemoteCloseTransport>(payload.trim()) else {
            continue;
        };
        spawn_remote_close(&transport, pane_uuid);
        recovered += 1;
    }
    Ok(recovered)
}

fn pending_dir() -> PathBuf {
    ZELLIJ_CACHE_DIR.join("remote-panes")
}

fn persist_pending(path: &Path, payload: &str) -> io::Result<()> {
    let parent = path.parent().expect("pending close path has a parent");
    fs::create_dir_all(parent)?;
    if let Ok(existing) = fs::read_to_string(path) {
        if existing == payload {
            return Ok(());
        }
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "remote pane UUID is already queued for another remote target",
        ));
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = path.with_extension(format!("close-pending.{}.{}", std::process::id(), nonce));
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(payload.as_bytes())?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    // Opening a directory for fsync is supported on Unix, where remote
    // sessions run, but Windows rejects directory handles opened as files.
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn spawn_remote_close(transport: &RemoteCloseTransport, pane_uuid: &str) {
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let mut args = transport.close_args();
    args.extend(["--pane-id".to_owned(), pane_uuid.to_owned()]);
    let _ = Command::new(executable)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{PaneId, RemotePaneMetadata};

    #[test]
    fn extracts_every_saved_coder_pane_and_ignores_other_sessions() {
        let mut session = SessionInfo::new("coder-api".into());
        session.remote_backend = Some(RemoteBackend::Coder {
            workspace: "alice/api".into(),
            local_session_id: String::new(),
        });
        for (id, uuid) in [(1, "uuid-one"), (2, "uuid-two")] {
            session.remote_panes.insert(
                PaneId::Terminal(id),
                RemotePaneMetadata {
                    pane_uuid: uuid.into(),
                    replay_cursor: id as u64,
                    close_pending: false,
                    foreground_argv: Vec::new(),
                },
            );
        }
        let transport = RemoteCloseTransport::Coder {
            workspace: "alice/api".into(),
        };
        assert_eq!(
            remote_close_targets(&session),
            vec![
                RemoteCloseTarget {
                    transport: transport.clone(),
                    pane_uuid: "uuid-one".into(),
                },
                RemoteCloseTarget {
                    transport: transport.clone(),
                    pane_uuid: "uuid-two".into(),
                },
            ]
        );
        assert_eq!(
            remote_close_targets_from_saved_metadata(&session.to_string()).unwrap(),
            remote_close_targets(&session)
        );
        session.remote_backend = None;
        assert!(remote_close_targets(&session).is_empty());
    }

    #[test]
    fn extracts_ssh_targets_with_transport_args() {
        let mut session = SessionInfo::new("ssh-box".into());
        session.remote_backend = Some(RemoteBackend::Ssh {
            name: "Dev Box".into(),
            destination: "abel@dev.example.com".into(),
            extra_args: vec!["-p".into(), "2222".into()],
            local_session_id: String::new(),
        });
        session.remote_panes.insert(
            PaneId::Terminal(1),
            RemotePaneMetadata {
                pane_uuid: "uuid-ssh".into(),
                replay_cursor: 1,
                close_pending: false,
                foreground_argv: Vec::new(),
            },
        );
        assert_eq!(
            remote_close_targets(&session),
            vec![RemoteCloseTarget {
                transport: RemoteCloseTransport::Ssh {
                    destination: "abel@dev.example.com".into(),
                    extra_args: vec!["-p".into(), "2222".into()],
                },
                pane_uuid: "uuid-ssh".into(),
            }]
        );
    }

    #[test]
    fn close_transport_round_trips_as_tagged_json() {
        for transport in [
            RemoteCloseTransport::Coder {
                workspace: "alice/api".into(),
            },
            RemoteCloseTransport::Ssh {
                destination: "abel@dev.example.com".into(),
                extra_args: vec!["-p".into(), "2222".into()],
            },
        ] {
            let payload = serde_json::to_string(&transport).unwrap();
            assert!(payload.contains("\"provider\""));
            assert_eq!(
                serde_json::from_str::<RemoteCloseTransport>(&payload).unwrap(),
                transport
            );
        }
    }

    #[test]
    fn pending_request_is_written_atomically() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "flock-close-pending-{}-{}",
            std::process::id(),
            nonce
        ));
        let path = root.join("pane.close-pending");
        let payload = serde_json::to_string(&RemoteCloseTransport::Coder {
            workspace: "alice/api".into(),
        })
        .unwrap();
        persist_pending(&path, &payload).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), payload);
        let _ = fs::remove_dir_all(root);
    }
}
