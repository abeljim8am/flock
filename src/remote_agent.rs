//! Persistent remote PTY agent.
//!
//! This module intentionally has no dependency on the Flock server, config or
//! plugin machinery. `connect` is a byte-for-byte bridge between SSH stdio and
//! a detached per-user daemon; `serve` owns PTYs and their replay buffers.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 1;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const HISTORY_LIMIT_BYTES: usize = 4 * 1024 * 1024;

fn ensure_protocol_version(protocol: u16) -> Result<()> {
    if protocol != PROTOCOL_VERSION {
        bail!("incompatible protocol version {protocol}; expected {PROTOCOL_VERSION}");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        protocol: u16,
        client_version: String,
    },
    CreatePane {
        pane_id: Option<Uuid>,
        cols: u16,
        rows: u16,
        env: HashMap<String, String>,
    },
    AttachPane {
        pane_id: Uuid,
        after_sequence: u64,
    },
    Input {
        pane_id: Uuid,
        data: Vec<u8>,
    },
    Resize {
        pane_id: Uuid,
        cols: u16,
        rows: u16,
    },
    Acknowledge {
        pane_id: Uuid,
        sequence: u64,
    },
    ForegroundProcess {
        pane_id: Uuid,
    },
    ClosePane {
        pane_id: Uuid,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello {
        protocol: u16,
        agent_version: String,
    },
    Error {
        message: String,
    },
    PaneCreated {
        pane_id: Uuid,
        pid: u32,
    },
    Attached {
        pane_id: Uuid,
        next_sequence: u64,
    },
    Output {
        pane_id: Uuid,
        sequence: u64,
        data: Vec<u8>,
    },
    ReplayTruncated {
        pane_id: Uuid,
        first_available: u64,
    },
    ForegroundProcess {
        pane_id: Uuid,
        argv: Vec<String>,
    },
    Exited {
        pane_id: Uuid,
        status: Option<i32>,
    },
    PaneClosed {
        pane_id: Uuid,
    },
}

#[derive(Clone)]
struct OutputChunk {
    sequence: u64,
    data: Vec<u8>,
}

struct PaneState {
    id: Uuid,
    pid: libc::pid_t,
    master: Mutex<File>,
    history: Mutex<VecDeque<OutputChunk>>,
    history_bytes: Mutex<usize>,
    next_sequence: Mutex<u64>,
    subscribers: Mutex<Vec<mpsc::Sender<ServerMessage>>>,
    exit_status: Mutex<Option<Option<i32>>>,
}

impl PaneState {
    fn publish(&self, message: ServerMessage) {
        self.subscribers
            .lock()
            .unwrap()
            .retain(|subscriber| subscriber.send(message.clone()).is_ok());
    }

    fn record_output(&self, data: Vec<u8>) {
        let sequence = {
            let mut next = self.next_sequence.lock().unwrap();
            let current = *next;
            *next += 1;
            current
        };
        let mut history = self.history.lock().unwrap();
        let mut bytes = self.history_bytes.lock().unwrap();
        *bytes += data.len();
        history.push_back(OutputChunk {
            sequence,
            data: data.clone(),
        });
        while *bytes > HISTORY_LIMIT_BYTES && history.len() > 1 {
            if let Some(removed) = history.pop_front() {
                *bytes -= removed.data.len();
            }
        }
        drop(bytes);
        drop(history);
        self.publish(ServerMessage::Output {
            pane_id: self.id,
            sequence,
            data,
        });
    }
}

type Panes = Arc<Mutex<HashMap<Uuid, Arc<PaneState>>>>;

pub fn serve(socket: Option<PathBuf>, foreground: bool) -> Result<()> {
    require_supported_platform()?;
    let socket = socket.unwrap_or(default_socket_path()?);
    if !foreground {
        if daemon_is_live(&socket) {
            return Ok(());
        }
        let executable = std::env::current_exe().context("locate the Flock executable")?;
        let child = Command::new(executable)
            .args(["remote-agent", "serve", "--foreground", "--socket"])
            .arg(&socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("start the detached remote-agent daemon")?;
        let _ = child.id();
        wait_for_socket(&socket, Duration::from_secs(3))?;
        return Ok(());
    }

    if let Some(parent) = socket.parent() {
        let parent_existed = parent.exists();
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        if !parent_existed {
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
    }
    if socket.exists() {
        if daemon_is_live(&socket) {
            bail!("another remote-agent daemon is already listening");
        }
        fs::remove_file(&socket).with_context(|| format!("remove stale {}", socket.display()))?;
    }
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("bind user-only socket {}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let panes: Panes = Arc::new(Mutex::new(HashMap::new()));
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let panes = panes.clone();
                thread::spawn(move || {
                    if let Err(error) = handle_client(stream, panes) {
                        eprintln!("flock remote-agent client: {error:#}");
                    }
                });
            },
            Err(error) => eprintln!("flock remote-agent accept: {error}"),
        }
    }
    Ok(())
}

pub fn connect(socket: Option<PathBuf>) -> Result<()> {
    require_supported_platform()?;
    let socket = socket.unwrap_or(default_socket_path()?);
    if !daemon_is_live(&socket) {
        serve(Some(socket.clone()), false)?;
    }
    let mut stream = UnixStream::connect(&socket)
        .with_context(|| format!("connect to remote-agent at {}", socket.display()))?;
    proxy_stdio(&mut stream)
}

/// Proxy SSH stdio and the daemon socket from one thread. In particular, do
/// not read SSH stdin from a helper thread: Coder command sessions can leave
/// that reader asleep while stdout is waiting for the protocol handshake.
fn proxy_stdio(stream: &mut UnixStream) -> Result<()> {
    let mut stdin_open = true;
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let mut descriptors = [
            libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: if stdin_open { libc::POLLIN } else { 0 },
                revents: 0,
            },
            libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, -1) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("poll remote-agent bridge");
        }

        let socket_events = descriptors[1].revents;
        if socket_events & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match stream.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(count) => {
                    let mut stdout = io::stdout().lock();
                    stdout.write_all(&buffer[..count])?;
                    stdout.flush()?;
                },
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {},
                Err(error) => return Err(error).context("read remote-agent daemon output"),
            }
        }

        let stdin_events = descriptors[0].revents;
        if stdin_open && stdin_events & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            let count =
                unsafe { libc::read(libc::STDIN_FILENO, buffer.as_mut_ptr().cast(), buffer.len()) };
            if count == 0 {
                stdin_open = false;
                stream.shutdown(Shutdown::Write)?;
            } else if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error).context("read remote-agent SSH input");
                }
            } else {
                stream.write_all(&buffer[..count as usize])?;
                stream.flush()?;
            }
        }
    }
}

/// Present one remote pane as an ordinary local PTY process. The Flock server
/// can render it using its normal terminal path while the actual shell remains
/// owned by the workspace daemon. Transport failure only ends this SSH child;
/// the bridge reconnects and attaches to the same UUID.
pub fn coder_pty(workspace: &str, pane_id: Option<&str>) -> Result<()> {
    let requested_id = pane_id
        .map(Uuid::parse_str)
        .transpose()
        .context("invalid pane UUID")?;
    let pane_id = match requested_id {
        Some(id) => id,
        None => {
            let session = std::env::var("FLOCK_SESSION_NAME").unwrap_or_else(|_| "flock".into());
            let local_pane = std::env::var("FLOCK_PANE_ID")
                .ok()
                .and_then(|id| id.parse().ok())
                .unwrap_or(0);
            Uuid::parse_str(&zellij_utils::data::stable_remote_pane_uuid(
                workspace,
                &session,
                zellij_utils::data::PaneId::Terminal(local_pane),
            ))?
        },
    };
    let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buffer = [0; 8192];
        while let Ok(count) = stdin.read(&mut buffer) {
            if count == 0 || input_tx.send(buffer[..count].to_vec()).is_err() {
                break;
            }
        }
    });
    let input_rx = Arc::new(Mutex::new(input_rx));
    // Stable IDs always attach first. An unknown-pane response creates it,
    // making resurrection idempotent without duplicating shells.
    let mut created = true;
    let cursor_path = local_cursor_path(pane_id)?;
    persist_connection(&cursor_path, "connecting")?;
    let mut cursor = fs::read_to_string(&cursor_path)
        .ok()
        .and_then(|cursor| cursor.trim().parse().ok())
        .unwrap_or(0);
    let mut delay = Duration::from_millis(250);

    loop {
        match run_coder_transport(
            workspace,
            pane_id,
            created,
            cursor,
            &cursor_path,
            input_rx.clone(),
        ) {
            Ok(TransportEnd::Exited(status)) => {
                persist_connection(&cursor_path, "disconnected")?;
                std::process::exit(status.unwrap_or_default())
            },
            Ok(TransportEnd::Attached(last_cursor)) => {
                created = true;
                cursor = last_cursor;
            },
            Err(error) => {
                writeln!(
                    io::stderr(),
                    "\r\nflock: Coder connection lost ({error}); reconnecting…"
                )?;
            },
        }
        persist_connection(&cursor_path, "reconnecting")?;
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}

pub fn coder_close(workspace: &str, pane_id: &str) -> Result<()> {
    let pane_id = Uuid::parse_str(pane_id).context("invalid pane UUID")?;
    let cursor_path = local_cursor_path(pane_id)?;
    let pending_path = cursor_path.with_extension("close-pending");
    if let Some(parent) = pending_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&pending_path, workspace)?;
    let mut delay = Duration::from_millis(250);
    loop {
        match send_remote_close(workspace, pane_id) {
            Ok(()) => {
                let _ = fs::remove_file(&pending_path);
                let _ = fs::remove_file(&cursor_path);
                let _ = fs::remove_file(cursor_path.with_extension("foreground"));
                let _ = fs::remove_file(cursor_path.with_extension("connection"));
                return Ok(());
            },
            Err(error) => {
                eprintln!("flock: remote pane close pending ({error}); retrying");
                thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(10));
            },
        }
    }
}

fn send_remote_close(workspace: &str, pane_id: Uuid) -> Result<()> {
    let remote = r#"exec "$HOME/.local/share/flock/current/flock" remote-agent connect"#;
    let mut child = Command::new("coder")
        .args(["ssh", workspace, "--", "sh", "-c", &format!("'{remote}'")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut writer = child.stdin.take().context("open coder close stdin")?;
    let mut reader = child.stdout.take().context("open coder close stdout")?;
    write_frame(
        &mut writer,
        &ClientMessage::Hello {
            protocol: PROTOCOL_VERSION,
            client_version: env!("CARGO_PKG_VERSION").into(),
        },
    )?;
    match read_frame::<_, ServerMessage>(&mut reader)? {
        ServerMessage::Hello {
            protocol: PROTOCOL_VERSION,
            ..
        } => {},
        response => bail!("unexpected close handshake: {response:?}"),
    }
    write_frame(&mut writer, &ClientMessage::ClosePane { pane_id })?;
    loop {
        match read_frame::<_, ServerMessage>(&mut reader)? {
            ServerMessage::PaneClosed { pane_id: closed } if closed == pane_id => return Ok(()),
            ServerMessage::Error { message } => bail!("{message}"),
            _ => {},
        }
    }
}

enum TransportEnd {
    Attached(u64),
    Exited(Option<i32>),
}

fn run_coder_transport(
    workspace: &str,
    pane_id: Uuid,
    attach_first: bool,
    cursor: u64,
    cursor_path: &Path,
    input_rx: Arc<Mutex<mpsc::Receiver<Vec<u8>>>>,
) -> Result<TransportEnd> {
    let remote = r#"exec "$HOME/.local/share/flock/current/flock" remote-agent connect"#;
    let mut child = Command::new("coder")
        .args(["ssh", workspace, "--", "sh", "-c", &format!("'{remote}'")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("start coder ssh remote-agent transport")?;
    let child_stdin = Arc::new(Mutex::new(child.stdin.take().context("open coder stdin")?));
    let mut child_stdout = child.stdout.take().context("open coder stdout")?;
    write_frame(
        &mut *child_stdin.lock().unwrap(),
        &ClientMessage::Hello {
            protocol: PROTOCOL_VERSION,
            client_version: env!("CARGO_PKG_VERSION").into(),
        },
    )?;
    match read_frame::<_, ServerMessage>(&mut child_stdout)? {
        ServerMessage::Hello {
            protocol: PROTOCOL_VERSION,
            ..
        } => {},
        ServerMessage::Error { message } => bail!("{message}"),
        message => bail!("unexpected handshake response: {message:?}"),
    }

    let (cols, rows) = local_terminal_size();
    if attach_first {
        write_frame(
            &mut *child_stdin.lock().unwrap(),
            &ClientMessage::AttachPane {
                pane_id,
                after_sequence: cursor,
            },
        )?;
    } else {
        write_frame(
            &mut *child_stdin.lock().unwrap(),
            &ClientMessage::CreatePane {
                pane_id: Some(pane_id),
                cols,
                rows,
                env: minimal_terminal_env(),
            },
        )?;
    }
    // Do not forward queued keystrokes until the daemon confirms that this
    // pane exists. Otherwise an eager Input can overtake the unknown-pane
    // response to AttachPane, producing duplicate CreatePane requests and
    // permanently discarding the first keystrokes.
    let mut create_sent = !attach_first;
    let mut last_cursor = cursor;
    loop {
        match read_frame::<_, ServerMessage>(&mut child_stdout)? {
            ServerMessage::Attached {
                pane_id: attached, ..
            } if attached == pane_id => break,
            ServerMessage::PaneCreated {
                pane_id: created, ..
            } if created == pane_id => break,
            ServerMessage::Output {
                pane_id: output_pane,
                sequence,
                data,
            } if output_pane == pane_id => {
                io::stdout().lock().write_all(&data)?;
                io::stdout().lock().flush()?;
                last_cursor = sequence;
                write_frame(
                    &mut *child_stdin.lock().unwrap(),
                    &ClientMessage::Acknowledge { pane_id, sequence },
                )?;
                persist_cursor(cursor_path, sequence)?;
            },
            ServerMessage::ReplayTruncated {
                first_available, ..
            } => {
                write!(io::stdout().lock(), "\x1b[!p\x1b[2J\x1b[H\r\n[flock: remote output before sequence {first_available} was truncated]\r\n")?;
            },
            ServerMessage::Error { message }
                if attach_first && !create_sent && message.contains("unknown pane") =>
            {
                write_frame(
                    &mut *child_stdin.lock().unwrap(),
                    &ClientMessage::CreatePane {
                        pane_id: Some(pane_id),
                        cols,
                        rows,
                        env: minimal_terminal_env(),
                    },
                )?;
                create_sent = true;
            },
            ServerMessage::Error { message } => bail!("{message}"),
            ServerMessage::Exited { status, .. } => {
                return Ok(TransportEnd::Exited(status));
            },
            _ => {},
        }
    }
    persist_connection(cursor_path, "connected")?;

    let writer = child_stdin.clone();
    thread::spawn(move || {
        while let Ok(data) = input_rx.lock().unwrap().recv() {
            if write_frame(
                &mut *writer.lock().unwrap(),
                &ClientMessage::Input { pane_id, data },
            )
            .is_err()
            {
                break;
            }
        }
    });

    let resize_writer = child_stdin.clone();
    let resize_active = Arc::new(AtomicBool::new(true));
    let resize_thread_active = resize_active.clone();
    thread::spawn(move || {
        let mut last_size = (cols, rows);
        let mut foreground_tick = 0u8;
        while resize_thread_active.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(250));
            let size = local_terminal_size();
            if size != last_size {
                last_size = size;
                if write_frame(
                    &mut *resize_writer.lock().unwrap(),
                    &ClientMessage::Resize {
                        pane_id,
                        cols: size.0,
                        rows: size.1,
                    },
                )
                .is_err()
                {
                    break;
                }
            }
            foreground_tick = foreground_tick.wrapping_add(1);
            if foreground_tick % 4 == 0
                && write_frame(
                    &mut *resize_writer.lock().unwrap(),
                    &ClientMessage::ForegroundProcess { pane_id },
                )
                .is_err()
            {
                break;
            }
        }
    });

    loop {
        match read_frame::<_, ServerMessage>(&mut child_stdout) {
            Ok(ServerMessage::Output {
                pane_id: output_pane,
                sequence,
                data,
            }) if output_pane == pane_id => {
                io::stdout().lock().write_all(&data)?;
                io::stdout().lock().flush()?;
                last_cursor = sequence;
                write_frame(
                    &mut *child_stdin.lock().unwrap(),
                    &ClientMessage::Acknowledge { pane_id, sequence },
                )?;
                persist_cursor(cursor_path, sequence)?;
            },
            Ok(ServerMessage::ReplayTruncated {
                first_available, ..
            }) => {
                write!(io::stdout().lock(), "\x1b[!p\x1b[2J\x1b[H\r\n[flock: remote output before sequence {first_available} was truncated]\r\n")?;
            },
            Ok(ServerMessage::Exited { status, .. }) => {
                resize_active.store(false, Ordering::Relaxed);
                return Ok(TransportEnd::Exited(status));
            },
            Ok(ServerMessage::ForegroundProcess { argv, .. }) => {
                persist_foreground(cursor_path, &argv)?;
            },
            Ok(ServerMessage::Error { message }) => bail!("{message}"),
            Ok(_) => {},
            Err(_) => {
                resize_active.store(false, Ordering::Relaxed);
                let _ = child.kill();
                return Ok(TransportEnd::Attached(last_cursor));
            },
        }
    }
}

fn local_cursor_path(pane_id: Uuid) -> Result<PathBuf> {
    let root = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or_else(|| anyhow!("HOME or XDG_CACHE_HOME is required"))?;
    Ok(root
        .join("flock")
        .join("remote-panes")
        .join(format!("{pane_id}.cursor")))
}

fn persist_cursor(path: &Path, cursor: u64) -> Result<()> {
    let parent = path.parent().context("remote cursor parent")?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let temporary = path.with_extension(format!("cursor.{}", std::process::id()));
    fs::write(&temporary, cursor.to_string())?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn persist_foreground(cursor_path: &Path, argv: &[String]) -> Result<()> {
    let path = cursor_path.with_extension("foreground");
    let parent = path.parent().context("remote foreground parent")?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("foreground.{}", std::process::id()));
    fs::write(&temporary, argv.join("\0"))?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn persist_connection(cursor_path: &Path, state: &str) -> Result<()> {
    let path = cursor_path.with_extension("connection");
    let parent = path.parent().context("remote connection parent")?;
    fs::create_dir_all(parent)?;
    fs::write(path, state)?;
    Ok(())
}

fn minimal_terminal_env() -> HashMap<String, String> {
    ["TERM", "COLORTERM", "LANG", "LC_ALL", "LC_CTYPE"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok().map(|value| (key.to_owned(), value)))
        .collect()
}

fn local_terminal_size() -> (u16, u16) {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut size) } == 0
        && size.ws_col > 0
        && size.ws_row > 0
    {
        (size.ws_col, size.ws_row)
    } else {
        (80, 24)
    }
}

fn require_supported_platform() -> Result<()> {
    if std::env::consts::OS != "linux" || std::env::consts::ARCH != "x86_64" {
        bail!("unsupported platform: remote Coder workspaces require Linux x86_64");
    }
    Ok(())
}

fn default_socket_path() -> Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or_else(|| anyhow!("HOME or XDG_RUNTIME_DIR is required"))?;
    Ok(runtime
        .join("flock")
        .join(format!("remote-agent-v{PROTOCOL_VERSION}.sock")))
}

fn daemon_is_live(socket: &Path) -> bool {
    UnixStream::connect(socket).is_ok()
}

fn wait_for_socket(socket: &Path, timeout: Duration) -> Result<()> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if daemon_is_live(socket) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    bail!("remote-agent daemon did not become ready")
}

fn handle_client(stream: UnixStream, panes: Panes) -> Result<()> {
    let mut reader = stream.try_clone()?;
    let mut direct_writer = stream;

    let hello: ClientMessage = read_frame(&mut reader)?;
    match hello {
        ClientMessage::Hello { protocol, .. } if protocol == PROTOCOL_VERSION => {
            write_frame(
                &mut direct_writer,
                &ServerMessage::Hello {
                    protocol: PROTOCOL_VERSION,
                    agent_version: env!("CARGO_PKG_VERSION").to_owned(),
                },
            )?;
        },
        ClientMessage::Hello { protocol, .. } => {
            let error = ensure_protocol_version(protocol).unwrap_err().to_string();
            write_frame(&mut direct_writer, &ServerMessage::Error { message: error })?;
            bail!("incompatible protocol version {protocol}");
        },
        _ => bail!("the first frame must be a hello"),
    }

    let (tx, rx) = mpsc::channel::<ServerMessage>();
    thread::spawn(move || -> Result<()> {
        for message in rx {
            write_frame(&mut direct_writer, &message)?;
        }
        Ok(())
    });

    while let Ok(message) = read_frame::<_, ClientMessage>(&mut reader) {
        match message {
            ClientMessage::Hello { .. } => {
                tx.send(ServerMessage::Error {
                    message: "duplicate hello".into(),
                })?;
            },
            ClientMessage::CreatePane {
                pane_id,
                cols,
                rows,
                env,
            } => {
                let id = pane_id.unwrap_or_else(Uuid::new_v4);
                if panes.lock().unwrap().contains_key(&id) {
                    tx.send(ServerMessage::Error {
                        message: format!("pane {id} already exists"),
                    })?;
                    continue;
                }
                let pane = spawn_pane(id, cols, rows, env)?;
                pane.subscribers.lock().unwrap().push(tx.clone());
                panes.lock().unwrap().insert(id, pane.clone());
                tx.send(ServerMessage::PaneCreated {
                    pane_id: id,
                    pid: pane.pid as u32,
                })?;
                start_pane_threads(pane);
            },
            ClientMessage::AttachPane {
                pane_id,
                after_sequence,
            } => {
                let Some(pane) = panes.lock().unwrap().get(&pane_id).cloned() else {
                    tx.send(ServerMessage::Error {
                        message: format!("unknown pane {pane_id}"),
                    })?;
                    continue;
                };
                pane.subscribers.lock().unwrap().push(tx.clone());
                let history = pane.history.lock().unwrap().clone();
                if let Some(first) = history.front().map(|chunk| chunk.sequence) {
                    if after_sequence.saturating_add(1) < first {
                        tx.send(ServerMessage::ReplayTruncated {
                            pane_id,
                            first_available: first,
                        })?;
                    }
                }
                for chunk in history
                    .iter()
                    .filter(|chunk| chunk.sequence > after_sequence)
                {
                    tx.send(ServerMessage::Output {
                        pane_id,
                        sequence: chunk.sequence,
                        data: chunk.data.clone(),
                    })?;
                }
                let next_sequence = *pane.next_sequence.lock().unwrap();
                tx.send(ServerMessage::Attached {
                    pane_id,
                    next_sequence,
                })?;
                let exit_status = *pane.exit_status.lock().unwrap();
                if let Some(status) = exit_status {
                    tx.send(ServerMessage::Exited { pane_id, status })?;
                }
            },
            ClientMessage::Input { pane_id, data } => with_pane(&panes, pane_id, &tx, |pane| {
                pane.master
                    .lock()
                    .unwrap()
                    .write_all(&data)
                    .context("write PTY input")
            })?,
            ClientMessage::Resize {
                pane_id,
                cols,
                rows,
            } => with_pane(&panes, pane_id, &tx, |pane| {
                set_winsize(pane.master.lock().unwrap().as_raw_fd(), cols, rows)
            })?,
            ClientMessage::Acknowledge { .. } => {},
            ClientMessage::ForegroundProcess { pane_id } => {
                with_pane(&panes, pane_id, &tx, |pane| {
                    tx.send(ServerMessage::ForegroundProcess {
                        pane_id,
                        argv: foreground_argv(pane.master.lock().unwrap().as_raw_fd()),
                    })?;
                    Ok(())
                })?
            },
            ClientMessage::ClosePane { pane_id } => {
                let pane = panes.lock().unwrap().remove(&pane_id);
                if let Some(pane) = pane {
                    unsafe { libc::kill(-pane.pid, libc::SIGHUP) };
                    tx.send(ServerMessage::PaneClosed { pane_id })?;
                } else {
                    tx.send(ServerMessage::PaneClosed { pane_id })?;
                }
            },
        }
    }
    drop(tx);
    Ok(())
}

fn with_pane<F>(panes: &Panes, id: Uuid, tx: &mpsc::Sender<ServerMessage>, f: F) -> Result<()>
where
    F: FnOnce(&Arc<PaneState>) -> Result<()>,
{
    if let Some(pane) = panes.lock().unwrap().get(&id).cloned() {
        f(&pane)
    } else {
        tx.send(ServerMessage::Error {
            message: format!("unknown pane {id}"),
        })?;
        Ok(())
    }
}

fn spawn_pane(
    id: Uuid,
    cols: u16,
    rows: u16,
    env: HashMap<String, String>,
) -> Result<Arc<PaneState>> {
    let mut master_fd = -1;
    let mut slave_fd = -1;
    let winsize = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null(),
            &winsize,
        )
    } < 0
    {
        return Err(io::Error::last_os_error()).context("open PTY");
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe {
            libc::close(master_fd);
            libc::close(slave_fd);
        }
        return Err(io::Error::last_os_error()).context("fork PTY shell");
    }
    if pid == 0 {
        unsafe {
            libc::close(master_fd);
            libc::setsid();
            libc::ioctl(slave_fd, libc::TIOCSCTTY, 0);
            libc::dup2(slave_fd, libc::STDIN_FILENO);
            libc::dup2(slave_fd, libc::STDOUT_FILENO);
            libc::dup2(slave_fd, libc::STDERR_FILENO);
            if slave_fd > libc::STDERR_FILENO {
                libc::close(slave_fd);
            }
        }
        for (key, value) in env.into_iter().filter(|(key, _)| allowed_env(key)) {
            std::env::set_var(key, value);
        }
        let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
        let error = Command::new(&shell).arg("-l").exec();
        eprintln!("flock remote-agent: failed to exec {:?}: {error}", shell);
        unsafe { libc::_exit(127) }
    }
    unsafe {
        libc::close(slave_fd);
    }
    let master = unsafe { File::from_raw_fd(master_fd) };
    Ok(Arc::new(PaneState {
        id,
        pid,
        master: Mutex::new(master),
        history: Mutex::new(VecDeque::new()),
        history_bytes: Mutex::new(0),
        next_sequence: Mutex::new(1),
        subscribers: Mutex::new(Vec::new()),
        exit_status: Mutex::new(None),
    }))
}

use std::os::unix::process::CommandExt;

fn allowed_env(key: &str) -> bool {
    matches!(key, "TERM" | "COLORTERM" | "LANG" | "LC_ALL" | "LC_CTYPE")
}

fn start_pane_threads(pane: Arc<PaneState>) {
    let reader_pane = pane.clone();
    thread::spawn(move || {
        let mut reader = match reader_pane.master.lock().unwrap().try_clone() {
            Ok(reader) => reader,
            Err(_) => return,
        };
        let mut buf = vec![0; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(count) => reader_pane.record_output(buf[..count].to_vec()),
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(_) => break,
            }
        }
        let mut status = 0;
        let waited = unsafe { libc::waitpid(reader_pane.pid, &mut status, 0) };
        let exit = if waited > 0 && libc::WIFEXITED(status) {
            Some(libc::WEXITSTATUS(status))
        } else if waited > 0 && libc::WIFSIGNALED(status) {
            Some(128 + libc::WTERMSIG(status))
        } else {
            None
        };
        *reader_pane.exit_status.lock().unwrap() = Some(exit);
        reader_pane.publish(ServerMessage::Exited {
            pane_id: reader_pane.id,
            status: exit,
        });
    });
}

fn set_winsize(fd: i32, cols: u16, rows: u16) -> Result<()> {
    let winsize = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &winsize) } < 0 {
        return Err(io::Error::last_os_error()).context("resize PTY");
    }
    Ok(())
}

fn foreground_argv(fd: i32) -> Vec<String> {
    let pgrp = unsafe { libc::tcgetpgrp(fd) };
    if pgrp <= 0 {
        return Vec::new();
    }
    fs::read(format!("/proc/{pgrp}/cmdline"))
        .map(|bytes| {
            bytes
                .split(|byte| *byte == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, message: &T) -> Result<()> {
    let payload = serde_json::to_vec(message)?;
    if payload.len() > MAX_FRAME_BYTES {
        bail!("frame exceeds {MAX_FRAME_BYTES} bytes");
    }
    writer.write_all(&(payload.len() as u32).to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(reader: &mut R) -> Result<T> {
    let mut length = [0; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        bail!("invalid frame length {length}");
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload).context("decode remote-agent frame")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    #[test]
    fn frames_round_trip_and_reject_malformed_lengths() {
        let message = ClientMessage::Hello {
            protocol: 1,
            client_version: "26.0.0".into(),
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &message).unwrap();
        assert_eq!(
            read_frame::<_, ClientMessage>(&mut bytes.as_slice()).unwrap(),
            message
        );
        assert!(read_frame::<_, ClientMessage>(&mut [0, 0, 0, 0].as_slice()).is_err());
        assert!(read_frame::<_, ClientMessage>(&mut u32::MAX.to_be_bytes().as_slice()).is_err());
    }

    #[test]
    fn bounded_history_drops_oldest_complete_chunks() {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        let pane = PaneState {
            id: Uuid::new_v4(),
            pid: 1,
            master: Mutex::new(file),
            history: Mutex::new(VecDeque::new()),
            history_bytes: Mutex::new(0),
            next_sequence: Mutex::new(1),
            subscribers: Mutex::new(Vec::new()),
            exit_status: Mutex::new(None),
        };
        pane.record_output(vec![1; HISTORY_LIMIT_BYTES]);
        pane.record_output(vec![2; 16]);
        let history = pane.history.lock().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].sequence, 2);
    }

    #[test]
    fn incompatible_protocol_is_rejected() {
        let error = ensure_protocol_version(PROTOCOL_VERSION + 1).unwrap_err();
        assert!(error.to_string().contains("incompatible protocol version"));
        assert!(ensure_protocol_version(PROTOCOL_VERSION).is_ok());
    }

    #[test]
    fn pty_survives_detached_output_and_panes_are_isolated() {
        let first_id = Uuid::new_v4();
        let second_id = Uuid::new_v4();
        let first = spawn_pane(first_id, 80, 24, HashMap::new()).unwrap();
        let second = spawn_pane(second_id, 100, 30, HashMap::new()).unwrap();
        let first_pid = first.pid;
        start_pane_threads(first.clone());
        start_pane_threads(second.clone());
        first
            .master
            .lock()
            .unwrap()
            .write_all(b"printf flock-first\\n")
            .unwrap();
        second
            .master
            .lock()
            .unwrap()
            .write_all(b"printf flock-second\\n")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let first_output = first
                .history
                .lock()
                .unwrap()
                .iter()
                .flat_map(|c| c.data.clone())
                .collect::<Vec<_>>();
            let second_output = second
                .history
                .lock()
                .unwrap()
                .iter()
                .flat_map(|c| c.data.clone())
                .collect::<Vec<_>>();
            if first_output.windows(11).any(|w| w == b"flock-first")
                && second_output.windows(12).any(|w| w == b"flock-second")
            {
                assert_eq!(first.pid, first_pid);
                assert!(!first_output.windows(12).any(|w| w == b"flock-second"));
                set_winsize(first.master.lock().unwrap().as_raw_fd(), 120, 40).unwrap();
                unsafe {
                    libc::kill(-first.pid, libc::SIGHUP);
                    libc::kill(-second.pid, libc::SIGHUP);
                }
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        unsafe {
            libc::kill(-first.pid, libc::SIGKILL);
            libc::kill(-second.pid, libc::SIGKILL);
        }
        panic!("PTY shells did not produce isolated output before timeout");
    }

    #[test]
    fn environment_forwarding_is_minimal() {
        assert!(allowed_env("TERM"));
        assert!(allowed_env("LANG"));
        assert!(!allowed_env("ZELLIJ_CONFIG_DIR"));
        assert!(!allowed_env("HOME"));
    }
}
