use crate::consts::{session_info_cache_file_name, ZELLIJ_CACHE_DIR};
use crate::data::{RemoteBackend, SessionInfo};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoderCloseTarget {
    pub workspace: String,
    pub pane_uuid: String,
}

pub fn coder_close_targets(session: &SessionInfo) -> Vec<CoderCloseTarget> {
    let Some(RemoteBackend::Coder { workspace, .. }) = session.remote_backend.as_ref() else {
        return Vec::new();
    };
    session
        .remote_panes
        .values()
        .map(|pane| CoderCloseTarget {
            workspace: workspace.clone(),
            pane_uuid: pane.pane_uuid.clone(),
        })
        .collect()
}

pub fn queue_saved_coder_pane_closes(session_name: &str) -> io::Result<usize> {
    let metadata_path = session_info_cache_file_name(session_name);
    let raw = match fs::read_to_string(&metadata_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let targets = coder_close_targets_from_saved_metadata(&raw)?;
    for target in &targets {
        queue_coder_close(&target.workspace, &target.pane_uuid)?;
    }
    Ok(targets.len())
}

fn coder_close_targets_from_saved_metadata(raw: &str) -> io::Result<Vec<CoderCloseTarget>> {
    let session = SessionInfo::from_string(raw, "")
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(coder_close_targets(&session))
}

pub fn queue_coder_close(workspace: &str, pane_uuid: &str) -> io::Result<()> {
    let pending_path = pending_dir().join(format!("{pane_uuid}.close-pending"));
    persist_pending(&pending_path, workspace)?;
    spawn_coder_close(workspace, pane_uuid);
    Ok(())
}

/// Retry durable requests left behind when a close worker or the host was
/// interrupted. Remote close is idempotent, so duplicate workers are safe.
pub fn recover_pending_coder_closes() -> io::Result<usize> {
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
        let Ok(workspace) = fs::read_to_string(&path) else {
            continue;
        };
        if workspace.trim().is_empty() {
            continue;
        }
        spawn_coder_close(workspace.trim(), pane_uuid);
        recovered += 1;
    }
    Ok(recovered)
}

fn pending_dir() -> PathBuf {
    ZELLIJ_CACHE_DIR.join("remote-panes")
}

fn persist_pending(path: &Path, workspace: &str) -> io::Result<()> {
    let parent = path.parent().expect("pending close path has a parent");
    fs::create_dir_all(parent)?;
    if let Ok(existing) = fs::read_to_string(path) {
        if existing == workspace {
            return Ok(());
        }
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "remote pane UUID is already queued for another Coder workspace",
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
    file.write_all(workspace.as_bytes())?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn spawn_coder_close(workspace: &str, pane_uuid: &str) {
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let _ = Command::new(executable)
        .args([
            "remote-agent",
            "coder-close",
            "--workspace",
            workspace,
            "--pane-id",
            pane_uuid,
        ])
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
            legacy: false,
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
        assert_eq!(
            coder_close_targets(&session),
            vec![
                CoderCloseTarget {
                    workspace: "alice/api".into(),
                    pane_uuid: "uuid-one".into(),
                },
                CoderCloseTarget {
                    workspace: "alice/api".into(),
                    pane_uuid: "uuid-two".into(),
                },
            ]
        );
        assert_eq!(
            coder_close_targets_from_saved_metadata(&session.to_string()).unwrap(),
            coder_close_targets(&session)
        );
        session.remote_backend = None;
        assert!(coder_close_targets(&session).is_empty());
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
        persist_pending(&path, "alice/api").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "alice/api");
        let _ = fs::remove_dir_all(root);
    }
}
