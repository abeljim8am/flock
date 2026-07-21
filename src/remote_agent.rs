//! Persistent remote PTY agent.
//!
//! This module intentionally has no dependency on the Flock server, config or
//! plugin machinery. `connect` is a byte-for-byte bridge between SSH stdio and
//! a detached per-user daemon; `serve` owns PTYs and their replay buffers.

use anyhow::{anyhow, bail, Context, Result};
use nix::errno::Errno;
use nix::sys::termios::{self, SetArg, Termios};
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
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 2;
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
        #[serde(default)]
        cwd: Option<String>,
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
    ReportAgentState {
        pane_id: Uuid,
        state: RemoteAgentRunState,
        agent: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAgentRunState {
    Working,
    Idle,
    Blocked,
    Release,
}

impl RemoteAgentRunState {
    fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "working" => Ok(Self::Working),
            "idle" => Ok(Self::Idle),
            "blocked" => Ok(Self::Blocked),
            "release" => Ok(Self::Release),
            _ => {
                bail!("invalid agent state {value:?}; expected working, idle, blocked, or release")
            },
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Blocked => "blocked",
            Self::Release => "release",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteAgentStateEvent {
    pub pane_id: Uuid,
    pub state: RemoteAgentRunState,
    pub agent: String,
}

/// Provider-specific carrier for the framed remote-agent protocol. Everything
/// past process spawn (framing, replay, cursor persistence) is shared; only
/// how we reach the remote daemon differs per provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteTransport {
    Coder {
        workspace: String,
    },
    Ssh {
        destination: String,
        ssh_args: Vec<String>,
    },
}

impl RemoteTransport {
    /// The provider-scoped identity string fed to `stable_remote_pane_uuid`.
    pub fn identity(&self) -> &str {
        match self {
            Self::Coder { workspace } => workspace,
            Self::Ssh { destination, .. } => destination,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Coder { .. } => "Coder",
            Self::Ssh { .. } => "SSH",
        }
    }

    fn connect_command(&self) -> Command {
        // Both transports hand the trailing words to the remote login shell as
        // one space-joined string, so the script must ride inside single
        // quotes to survive that second parse.
        let remote = r#"exec "$HOME/.local/share/flock/current/flock" remote-agent connect"#;
        match self {
            Self::Coder { workspace } => {
                let mut command = Command::new("coder");
                command.args(["ssh", workspace, "--", "sh", "-c", &format!("'{remote}'")]);
                command
            },
            Self::Ssh {
                destination,
                ssh_args,
            } => {
                // BatchMode forbids password/host-key prompts: a prompt reads
                // /dev/tty while the bridge holds it raw and is already
                // draining stdin, so it could never be answered anyway.
                let mut command = Command::new("ssh");
                command.args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "StrictHostKeyChecking=accept-new",
                    "-o",
                    "ServerAliveInterval=15",
                    "-o",
                    "ServerAliveCountMax=4",
                ]);
                command.args(ssh_args);
                command.arg("--");
                command.arg(destination);
                command.args(["sh", "-c", &format!("'{remote}'")]);
                command
            },
        }
    }

    fn close_spec(&self) -> zellij_utils::remote_session_cleanup::RemoteCloseTransport {
        use zellij_utils::remote_session_cleanup::RemoteCloseTransport;
        match self {
            Self::Coder { workspace } => RemoteCloseTransport::Coder {
                workspace: workspace.clone(),
            },
            Self::Ssh {
                destination,
                ssh_args,
            } => RemoteCloseTransport::Ssh {
                destination: destination.clone(),
                extra_args: ssh_args.clone(),
            },
        }
    }
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
        #[serde(default)]
        cwd: Option<String>,
    },
    Exited {
        pane_id: Uuid,
        status: Option<i32>,
    },
    PaneClosed {
        pane_id: Uuid,
    },
    AgentStateChanged {
        event: RemoteAgentStateEvent,
    },
    AgentStateAccepted {
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
    latest_agent_state: Mutex<Option<RemoteAgentStateEvent>>,
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
        let mut next = self.next_sequence.lock().unwrap();
        let sequence = *next;
        *next += 1;
        let mut history = self.history.lock().unwrap();
        let mut bytes = self.history_bytes.lock().unwrap();
        let mut subscribers = self.subscribers.lock().unwrap();
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
        let message = ServerMessage::Output {
            pane_id: self.id,
            sequence,
            data,
        };
        subscribers.retain(|subscriber| subscriber.send(message.clone()).is_ok());
    }

    /// Send a coherent history snapshot before making a client live. Holding
    /// the sequence lock prevents new output from being recorded between the
    /// replay watermark and subscriber registration.
    fn subscribe_with_replay(
        &self,
        after_sequence: u64,
        tx: &mpsc::Sender<ServerMessage>,
    ) -> Result<()> {
        let next_sequence = self.next_sequence.lock().unwrap();
        let history = self.history.lock().unwrap();
        let mut subscribers = self.subscribers.lock().unwrap();
        if let Some(first) = history.front().map(|chunk| chunk.sequence) {
            if after_sequence.saturating_add(1) < first {
                tx.send(ServerMessage::ReplayTruncated {
                    pane_id: self.id,
                    first_available: first,
                })?;
            }
        }
        for chunk in history
            .iter()
            .filter(|chunk| chunk.sequence > after_sequence)
        {
            tx.send(ServerMessage::Output {
                pane_id: self.id,
                sequence: chunk.sequence,
                data: chunk.data.clone(),
            })?;
        }
        tx.send(ServerMessage::Attached {
            pane_id: self.id,
            next_sequence: *next_sequence,
        })?;
        if let Some(event) = self.latest_agent_state.lock().unwrap().clone() {
            tx.send(ServerMessage::AgentStateChanged { event })?;
        }
        subscribers.push(tx.clone());
        Ok(())
    }

    fn record_agent_state(&self, event: RemoteAgentStateEvent) {
        let mut subscribers = self.subscribers.lock().unwrap();
        *self.latest_agent_state.lock().unwrap() = Some(event.clone());
        let message = ServerMessage::AgentStateChanged { event };
        subscribers.retain(|subscriber| subscriber.send(message.clone()).is_ok());
    }
}

type TransportWriter = Arc<Mutex<Box<dyn Write + Send>>>;

struct InputRouter {
    pane_id: Uuid,
    writer: Mutex<Option<TransportWriter>>,
    writer_ready: Condvar,
}

impl InputRouter {
    fn new(pane_id: Uuid) -> Self {
        Self {
            pane_id,
            writer: Mutex::new(None),
            writer_ready: Condvar::new(),
        }
    }

    fn activate(self: &Arc<Self>, writer: TransportWriter) -> ActiveInputWriter {
        *self.writer.lock().unwrap() = Some(writer.clone());
        self.writer_ready.notify_all();
        ActiveInputWriter {
            router: self.clone(),
            writer,
        }
    }

    fn forward(&self, data: Vec<u8>) {
        loop {
            let writer = {
                let mut writer = self.writer.lock().unwrap();
                while writer.is_none() {
                    writer = self.writer_ready.wait(writer).unwrap();
                }
                writer.as_ref().unwrap().clone()
            };
            if write_frame(
                &mut *writer.lock().unwrap(),
                &ClientMessage::Input {
                    pane_id: self.pane_id,
                    data: data.clone(),
                },
            )
            .is_ok()
            {
                return;
            }
            self.clear_if_current(&writer);
        }
    }

    fn clear_if_current(&self, writer: &TransportWriter) {
        let mut current = self.writer.lock().unwrap();
        if current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, writer))
        {
            *current = None;
        }
    }
}

struct ActiveInputWriter {
    router: Arc<InputRouter>,
    writer: TransportWriter,
}

struct RawTerminalGuard {
    fd: i32,
    original: Termios,
}

impl RawTerminalGuard {
    fn enter(fd: i32) -> Result<Option<Self>> {
        let original = match termios::tcgetattr(fd) {
            Ok(original) => original,
            Err(error) if error == Errno::ENOTTY => return Ok(None),
            Err(error) => return Err(error).context("read local terminal settings"),
        };
        let mut raw = original.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(fd, SetArg::TCSANOW, &raw)
            .context("put local Coder bridge terminal in raw mode")?;
        Ok(Some(Self { fd, original }))
    }
}

impl Drop for RawTerminalGuard {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(self.fd, SetArg::TCSANOW, &self.original);
    }
}

impl Drop for ActiveInputWriter {
    fn drop(&mut self) {
        self.router.clear_if_current(&self.writer);
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
/// owned by the remote daemon. Transport failure only ends the transport
/// child; the bridge reconnects and attaches to the same UUID.
pub fn remote_pty(
    transport: RemoteTransport,
    pane_id: Option<&str>,
    cwd: Option<PathBuf>,
) -> Result<()> {
    // Like ssh, the local bridge must bypass its own PTY line discipline. The
    // remote shell already owns a PTY and is solely responsible for echo,
    // completion and control-key handling.
    let _raw_terminal = RawTerminalGuard::enter(libc::STDIN_FILENO)?;
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
                transport.identity(),
                &session,
                zellij_utils::data::PaneId::Terminal(local_pane),
            ))?
        },
    };
    let agent_state_tx = start_local_agent_state_forwarder();
    let input_router = Arc::new(InputRouter::new(pane_id));
    let stdin_router = input_router.clone();
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buffer = [0; 8192];
        while let Ok(count) = stdin.read(&mut buffer) {
            if count == 0 {
                break;
            }
            stdin_router.forward(buffer[..count].to_vec());
        }
    });
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
        match run_remote_transport(
            &transport,
            pane_id,
            created,
            cursor,
            &cursor_path,
            cwd.as_deref(),
            input_router.clone(),
            agent_state_tx.as_ref(),
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
                    "\r\nflock: {} connection lost ({error}); reconnecting…",
                    transport.label(),
                )?;
            },
        }
        persist_connection(&cursor_path, "reconnecting")?;
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}

/// Submit one integration hook event to the per-user daemon. This command runs
/// inside the remote workspace and exits only after the daemon has retained the
/// event, so a short-lived hook cannot race its own process exit.
pub fn report_state(
    pane_id: &str,
    state: &str,
    agent: &str,
    socket: Option<PathBuf>,
) -> Result<()> {
    let pane_id = Uuid::parse_str(pane_id).context("invalid pane UUID")?;
    let state = RemoteAgentRunState::parse(state)?;
    validate_agent_label(agent)?;
    let socket = socket.unwrap_or(default_socket_path()?);
    let mut stream = UnixStream::connect(&socket)
        .with_context(|| format!("connect to remote-agent at {}", socket.display()))?;
    let mut reader = stream.try_clone()?;
    write_frame(
        &mut stream,
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
        ServerMessage::Error { message } => bail!("{message}"),
        response => bail!("unexpected report-state handshake: {response:?}"),
    }
    write_frame(
        &mut stream,
        &ClientMessage::ReportAgentState {
            pane_id,
            state,
            agent: agent.to_owned(),
        },
    )?;
    loop {
        match read_frame::<_, ServerMessage>(&mut reader)? {
            ServerMessage::AgentStateAccepted { pane_id: accepted } if accepted == pane_id => {
                return Ok(())
            },
            ServerMessage::Error { message } => bail!("{message}"),
            _ => {},
        }
    }
}

fn validate_agent_label(agent: &str) -> Result<()> {
    if agent.trim().is_empty() || agent.contains([',', '=']) {
        bail!("agent label must be non-empty and cannot contain ',' or '='");
    }
    Ok(())
}

pub fn remote_close(transport: RemoteTransport, pane_id: &str) -> Result<()> {
    let pane_id = Uuid::parse_str(pane_id).context("invalid pane UUID")?;
    let cursor_path = local_cursor_path(pane_id)?;
    let pending_path = cursor_path.with_extension("close-pending");
    if let Some(parent) = pending_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if !pending_path.exists() {
        let mut pending = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&pending_path)?;
        pending.write_all(serde_json::to_string(&transport.close_spec())?.as_bytes())?;
        pending.sync_all()?;
    }
    let _worker = match CloseWorkerGuard::acquire(cursor_path.with_extension("close-running"))? {
        Some(worker) => worker,
        None => return Ok(()),
    };
    let mut delay = Duration::from_millis(250);
    loop {
        match send_remote_close(&transport, pane_id) {
            Ok(()) => {
                let _ = fs::remove_file(&pending_path);
                let _ = fs::remove_file(&cursor_path);
                let _ = fs::remove_file(cursor_path.with_extension("foreground"));
                let _ = fs::remove_file(cursor_path.with_extension("cwd"));
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

struct CloseWorkerGuard {
    path: PathBuf,
}

impl CloseWorkerGuard {
    fn acquire(path: PathBuf) -> Result<Option<Self>> {
        for _ in 0..3 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    write!(file, "{}", std::process::id())?;
                    file.sync_all()?;
                    return Ok(Some(Self { path }));
                },
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let live = fs::read_to_string(&path)
                        .ok()
                        .and_then(|pid| pid.trim().parse::<i32>().ok())
                        .is_some_and(|pid| close_worker_is_live(pid, &path));
                    if live {
                        return Ok(None);
                    }
                    match fs::remove_file(&path) {
                        Ok(()) => {},
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {},
                        Err(error) => return Err(error.into()),
                    }
                },
                Err(error) => return Err(error.into()),
            }
        }
        Ok(None)
    }
}

fn close_worker_is_live(pid: i32, lock_path: &Path) -> bool {
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    let pane_uuid = lock_path.file_stem().and_then(|stem| stem.to_str());
    fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .map(|command| String::from_utf8_lossy(&command).replace('\0', " "))
        .is_some_and(|command| {
            command.contains("remote-close")
                && pane_uuid.is_some_and(|pane_uuid| command.contains(pane_uuid))
        })
}

impl Drop for CloseWorkerGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn send_remote_close(transport: &RemoteTransport, pane_id: Uuid) -> Result<()> {
    let mut child = transport
        .connect_command()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut writer = child.stdin.take().context("open remote close stdin")?;
    let mut reader = child.stdout.take().context("open remote close stdout")?;
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

fn run_remote_transport(
    transport: &RemoteTransport,
    pane_id: Uuid,
    attach_first: bool,
    cursor: u64,
    cursor_path: &Path,
    cwd: Option<&Path>,
    input_router: Arc<InputRouter>,
    agent_state_tx: Option<&mpsc::Sender<RemoteAgentStateEvent>>,
) -> Result<TransportEnd> {
    let mut child = transport
        .connect_command()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("start {} remote-agent transport", transport.label()))?;
    let child_stdin: TransportWriter = Arc::new(Mutex::new(Box::new(
        child.stdin.take().context("open transport stdin")?,
    )));
    let mut child_stdout = child.stdout.take().context("open transport stdout")?;
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
                cwd: cwd.map(|cwd| cwd.to_string_lossy().into_owned()),
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
                        cwd: cwd.map(|cwd| cwd.to_string_lossy().into_owned()),
                    },
                )?;
                create_sent = true;
            },
            ServerMessage::Error { message } => bail!("{message}"),
            ServerMessage::Exited { status, .. } => {
                return Ok(TransportEnd::Exited(status));
            },
            ServerMessage::AgentStateChanged { event } if event.pane_id == pane_id => {
                if let Some(agent_state_tx) = agent_state_tx {
                    let _ = agent_state_tx.send(event);
                }
            },
            _ => {},
        }
    }
    persist_connection(cursor_path, "connected")?;
    write_frame(
        &mut *child_stdin.lock().unwrap(),
        &ClientMessage::ForegroundProcess { pane_id },
    )?;

    let _active_input_writer = input_router.activate(child_stdin.clone());

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
            Ok(ServerMessage::ForegroundProcess { argv, cwd, .. }) => {
                persist_foreground(cursor_path, &argv)?;
                if let Some(cwd) = cwd {
                    persist_cwd(cursor_path, &cwd)?;
                }
            },
            Ok(ServerMessage::AgentStateChanged { event }) if event.pane_id == pane_id => {
                if let Some(agent_state_tx) = agent_state_tx {
                    let _ = agent_state_tx.send(event);
                }
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

fn persist_cwd(cursor_path: &Path, cwd: &str) -> Result<()> {
    let path = cursor_path.with_extension("cwd");
    let parent = path.parent().context("remote cwd parent")?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("cwd.{}", std::process::id()));
    fs::write(&temporary, cwd.as_bytes())?;
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

fn start_local_agent_state_forwarder() -> Option<mpsc::Sender<RemoteAgentStateEvent>> {
    let pane_id = std::env::var("FLOCK_PANE_ID").ok()?;
    pane_id.parse::<u32>().ok()?;
    let executable = std::env::var_os("FLOCK_EXECUTABLE")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())?;
    let (tx, rx) = mpsc::channel::<RemoteAgentStateEvent>();
    thread::spawn(move || {
        for event in rx {
            if let Err(error) = forward_agent_state_to_local_plugin(&executable, &pane_id, &event) {
                eprintln!("flock: failed to forward remote agent state: {error:#}");
            }
        }
    });
    Some(tx)
}

fn local_agent_state_args(pane_id: &str, event: &RemoteAgentStateEvent) -> String {
    format!(
        "pane_id={pane_id},state={},agent={},source=flock:coder-remote",
        event.state.as_str(),
        event.agent
    )
}

fn forward_agent_state_to_local_plugin(
    executable: &Path,
    pane_id: &str,
    event: &RemoteAgentStateEvent,
) -> Result<()> {
    let args = local_agent_state_args(pane_id, event);
    let mut child = Command::new(executable)
        .args(["pipe", "--name", "flock-state", "--args", &args])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start local flock-state publisher")?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            bail!("local flock-state publisher exited with {status}");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("local flock-state publisher timed out");
        }
        thread::sleep(Duration::from_millis(20));
    }
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
    if std::env::consts::OS != "linux"
        || !matches!(std::env::consts::ARCH, "x86_64" | "aarch64")
    {
        bail!("unsupported platform: remote sessions require Linux x86_64 or aarch64");
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
                cwd,
            } => {
                let id = pane_id.unwrap_or_else(Uuid::new_v4);
                let mut panes = panes.lock().unwrap();
                if panes.contains_key(&id) {
                    tx.send(ServerMessage::Error {
                        message: format!("pane {id} already exists"),
                    })?;
                    continue;
                }
                let pane = spawn_pane(id, cols, rows, env, cwd.as_deref().map(Path::new))?;
                pane.subscribers.lock().unwrap().push(tx.clone());
                panes.insert(id, pane.clone());
                drop(panes);
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
                pane.subscribe_with_replay(after_sequence, &tx)?;
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
                    let (argv, cwd) = foreground_process(pane.master.lock().unwrap().as_raw_fd());
                    tx.send(ServerMessage::ForegroundProcess { pane_id, argv, cwd })?;
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
            ClientMessage::ReportAgentState {
                pane_id,
                state,
                agent,
            } => {
                if let Err(error) = validate_agent_label(&agent) {
                    tx.send(ServerMessage::Error {
                        message: error.to_string(),
                    })?;
                    continue;
                }
                with_pane(&panes, pane_id, &tx, |pane| {
                    pane.record_agent_state(RemoteAgentStateEvent {
                        pane_id,
                        state,
                        agent,
                    });
                    tx.send(ServerMessage::AgentStateAccepted { pane_id })?;
                    Ok(())
                })?
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
    cwd: Option<&Path>,
) -> Result<Arc<PaneState>> {
    spawn_pane_with_shell(id, cols, rows, env, cwd, None)
}

fn spawn_pane_with_shell(
    id: Uuid,
    cols: u16,
    rows: u16,
    env: HashMap<String, String>,
    cwd: Option<&Path>,
    shell_override: Option<&Path>,
) -> Result<Arc<PaneState>> {
    let mut master_fd = -1;
    let mut slave_fd = -1;
    let mut winsize = libc::winsize {
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
            std::ptr::null_mut(),
            &mut winsize,
        )
    } < 0
    {
        return Err(io::Error::last_os_error()).context("open PTY");
    }
    let master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    let shell = shell_override
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("SHELL").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/bin/sh"));
    let mut command = Command::new(&shell);
    command.arg("-l");
    command.envs(env.into_iter().filter(|(key, _)| allowed_env(key)));
    command.envs(controlled_remote_pane_env(id)?);
    if let Some(cwd) = cwd.filter(|cwd| cwd.is_dir()) {
        command.current_dir(cwd);
    }
    command
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    unsafe {
        command.pre_exec(move || {
            if libc::close(master_fd) < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY.into(), 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command
        .spawn()
        .with_context(|| format!("spawn PTY shell {:?}", shell))?;
    let pid = child.id() as libc::pid_t;
    drop(child);
    Ok(Arc::new(PaneState {
        id,
        pid,
        master: Mutex::new(master),
        history: Mutex::new(VecDeque::new()),
        history_bytes: Mutex::new(0),
        next_sequence: Mutex::new(1),
        subscribers: Mutex::new(Vec::new()),
        latest_agent_state: Mutex::new(None),
        exit_status: Mutex::new(None),
    }))
}

use std::os::unix::process::CommandExt;

fn allowed_env(key: &str) -> bool {
    matches!(key, "TERM" | "COLORTERM" | "LANG" | "LC_ALL" | "LC_CTYPE")
}

fn controlled_remote_pane_env(id: Uuid) -> Result<HashMap<String, String>> {
    let executable = std::env::current_exe()
        .context("resolve remote Flock executable")?
        .to_string_lossy()
        .into_owned();
    Ok(HashMap::from_iter([
        ("FLOCK_PANE_ID".into(), id.to_string()),
        ("FLOCK_STATE_CHANNEL".into(), "remote-agent".into()),
        ("FLOCK_EXECUTABLE".into(), executable),
    ]))
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

fn foreground_process(fd: i32) -> (Vec<String>, Option<String>) {
    let pgrp = unsafe { libc::tcgetpgrp(fd) };
    if pgrp <= 0 {
        return (Vec::new(), None);
    }
    let argv = fs::read(format!("/proc/{pgrp}/cmdline"))
        .map(|bytes| {
            bytes
                .split(|byte| *byte == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect()
        })
        .unwrap_or_default();
    let cwd = fs::read_link(format!("/proc/{pgrp}/cwd"))
        .ok()
        .map(|cwd| cwd.to_string_lossy().into_owned());
    (argv, cwd)
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
    use nix::sys::termios::{InputFlags, LocalFlags, OutputFlags};
    use std::fs::OpenOptions;

    struct BrokenWriter;

    impl Write for BrokenWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn frames_round_trip_and_reject_malformed_lengths() {
        let message = ClientMessage::Hello {
            protocol: PROTOCOL_VERSION,
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

        let state_message = ServerMessage::AgentStateChanged {
            event: RemoteAgentStateEvent {
                pane_id: Uuid::new_v4(),
                state: RemoteAgentRunState::Working,
                agent: "opencode".into(),
            },
        };
        let mut state_bytes = Vec::new();
        write_frame(&mut state_bytes, &state_message).unwrap();
        assert_eq!(
            read_frame::<_, ServerMessage>(&mut state_bytes.as_slice()).unwrap(),
            state_message
        );
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
            latest_agent_state: Mutex::new(None),
            exit_status: Mutex::new(None),
        };
        pane.record_output(vec![1; HISTORY_LIMIT_BYTES]);
        pane.record_output(vec![2; 16]);
        let history = pane.history.lock().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].sequence, 2);
    }

    #[test]
    fn replay_is_delivered_once_before_live_output() {
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
            latest_agent_state: Mutex::new(None),
            exit_status: Mutex::new(None),
        };
        pane.record_output(b"replay".to_vec());
        let (tx, rx) = mpsc::channel();
        pane.subscribe_with_replay(0, &tx).unwrap();
        pane.record_output(b"live".to_vec());

        let messages: Vec<_> = rx.try_iter().collect();
        assert!(matches!(
            &messages[0],
            ServerMessage::Output { sequence: 1, data, .. } if data == b"replay"
        ));
        assert!(matches!(
            messages[1],
            ServerMessage::Attached {
                next_sequence: 2,
                ..
            }
        ));
        assert!(matches!(
            &messages[2],
            ServerMessage::Output { sequence: 2, data, .. } if data == b"live"
        ));
        assert_eq!(messages.len(), 3);
    }

    #[test]
    fn latest_agent_state_and_release_are_replayed_after_attach() {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        let pane_id = Uuid::new_v4();
        let pane = PaneState {
            id: pane_id,
            pid: 1,
            master: Mutex::new(file),
            history: Mutex::new(VecDeque::new()),
            history_bytes: Mutex::new(0),
            next_sequence: Mutex::new(1),
            subscribers: Mutex::new(Vec::new()),
            latest_agent_state: Mutex::new(None),
            exit_status: Mutex::new(None),
        };
        pane.record_agent_state(RemoteAgentStateEvent {
            pane_id,
            state: RemoteAgentRunState::Working,
            agent: "opencode".into(),
        });
        pane.record_agent_state(RemoteAgentStateEvent {
            pane_id,
            state: RemoteAgentRunState::Release,
            agent: "opencode".into(),
        });
        let (tx, rx) = mpsc::channel();
        pane.subscribe_with_replay(0, &tx).unwrap();
        let messages: Vec<_> = rx.try_iter().collect();
        assert!(matches!(messages[0], ServerMessage::Attached { .. }));
        assert!(matches!(
            &messages[1],
            ServerMessage::AgentStateChanged {
                event: RemoteAgentStateEvent {
                    state: RemoteAgentRunState::Release,
                    agent,
                    ..
                }
            } if agent == "opencode"
        ));
    }

    #[test]
    fn daemon_acknowledges_and_broadcasts_agent_state_reports() {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        let pane_id = Uuid::new_v4();
        let pane = Arc::new(PaneState {
            id: pane_id,
            pid: 1,
            master: Mutex::new(file),
            history: Mutex::new(VecDeque::new()),
            history_bytes: Mutex::new(0),
            next_sequence: Mutex::new(1),
            subscribers: Mutex::new(Vec::new()),
            latest_agent_state: Mutex::new(None),
            exit_status: Mutex::new(None),
        });
        let panes = Arc::new(Mutex::new(HashMap::from_iter([(pane_id, pane.clone())])));
        let (mut client, server) = UnixStream::pair().unwrap();
        let daemon = thread::spawn(move || handle_client(server, panes));
        let mut reader = client.try_clone().unwrap();
        write_frame(
            &mut client,
            &ClientMessage::Hello {
                protocol: PROTOCOL_VERSION,
                client_version: "test".into(),
            },
        )
        .unwrap();
        assert!(matches!(
            read_frame::<_, ServerMessage>(&mut reader).unwrap(),
            ServerMessage::Hello {
                protocol: PROTOCOL_VERSION,
                ..
            }
        ));
        let (subscriber_tx, subscriber_rx) = mpsc::channel();
        pane.subscribers.lock().unwrap().push(subscriber_tx);
        write_frame(
            &mut client,
            &ClientMessage::ReportAgentState {
                pane_id,
                state: RemoteAgentRunState::Blocked,
                agent: "claude".into(),
            },
        )
        .unwrap();
        assert_eq!(
            read_frame::<_, ServerMessage>(&mut reader).unwrap(),
            ServerMessage::AgentStateAccepted { pane_id }
        );
        assert!(matches!(
            subscriber_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            ServerMessage::AgentStateChanged {
                event: RemoteAgentStateEvent {
                    state: RemoteAgentRunState::Blocked,
                    ref agent,
                    ..
                }
            } if agent == "claude"
        ));
        client.shutdown(Shutdown::Both).unwrap();
        daemon.join().unwrap().unwrap();
    }

    #[test]
    fn agent_state_values_and_local_pipe_args_are_stable() {
        assert_eq!(
            RemoteAgentRunState::parse("BLOCKED").unwrap(),
            RemoteAgentRunState::Blocked
        );
        assert!(RemoteAgentRunState::parse("unknown").is_err());
        assert!(validate_agent_label("").is_err());
        assert!(validate_agent_label("bad,label").is_err());
        let event = RemoteAgentStateEvent {
            pane_id: Uuid::new_v4(),
            state: RemoteAgentRunState::Idle,
            agent: "opencode".into(),
        };
        assert_eq!(
            local_agent_state_args("7", &event),
            "pane_id=7,state=idle,agent=opencode,source=flock:coder-remote"
        );
    }

    #[test]
    fn bundled_integrations_use_one_asset_with_runtime_transport_selection() {
        let opencode =
            include_str!("../default-plugins/flock-sidebar/assets/opencode/flock-agent-state.js");
        let codex =
            include_str!("../default-plugins/flock-sidebar/assets/codex/flock-agent-state.sh");
        let claude =
            include_str!("../default-plugins/flock-sidebar/assets/claude/flock-agent-state.sh");
        for integration in [opencode, codex, claude] {
            assert!(integration.contains("FLOCK_STATE_CHANNEL"));
            assert!(integration.contains("remote-agent"));
            assert!(integration.contains("report-state"));
            assert!(integration.contains("flock-state"));
        }
        assert!(opencode.contains("useFileChannel"));
    }

    #[test]
    fn input_router_retries_a_chunk_on_the_replacement_transport() {
        let pane_id = Uuid::new_v4();
        let router = Arc::new(InputRouter::new(pane_id));
        let stale: TransportWriter = Arc::new(Mutex::new(Box::new(BrokenWriter)));
        let _stale = router.activate(stale);

        let forwarding_router = router.clone();
        let forwarding = thread::spawn(move || forwarding_router.forward(b"kept".to_vec()));
        let deadline = Instant::now() + Duration::from_secs(3);
        while router.writer.lock().unwrap().is_some() && Instant::now() < deadline {
            // Avoid continuously reacquiring the router mutex and starving the
            // forwarding thread on heavily loaded CI runners.
            thread::sleep(Duration::from_millis(10));
        }
        assert!(router.writer.lock().unwrap().is_none());

        let mut replacement = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let replacement_stdin: TransportWriter =
            Arc::new(Mutex::new(Box::new(replacement.stdin.take().unwrap())));
        let active = router.activate(replacement_stdin.clone());
        forwarding.join().unwrap();
        drop(active);
        drop(replacement_stdin);
        let output = replacement.wait_with_output().unwrap().stdout;
        let message = read_frame::<_, ClientMessage>(&mut output.as_slice()).unwrap();
        assert_eq!(
            message,
            ClientMessage::Input {
                pane_id,
                data: b"kept".to_vec(),
            }
        );
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
        let first = spawn_pane(first_id, 80, 24, HashMap::new(), None).unwrap();
        let second = spawn_pane(second_id, 100, 30, HashMap::new(), None).unwrap();
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
    fn pty_shell_starts_in_requested_remote_cwd() {
        let cwd = std::env::temp_dir().join(format!("flock-remote-cwd-{}", Uuid::new_v4()));
        fs::create_dir(&cwd).unwrap();
        let pane = spawn_pane_with_shell(
            Uuid::new_v4(),
            80,
            24,
            HashMap::new(),
            Some(&cwd),
            Some(Path::new("/bin/sh")),
        )
        .unwrap();
        start_pane_threads(pane.clone());
        pane.master.lock().unwrap().write_all(b"pwd\n").unwrap();
        let expected = cwd.to_string_lossy().into_owned();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let output = pane
                .history
                .lock()
                .unwrap()
                .iter()
                .flat_map(|chunk| chunk.data.clone())
                .collect::<Vec<_>>();
            if String::from_utf8_lossy(&output).contains(&expected) {
                unsafe { libc::kill(-pane.pid, libc::SIGHUP) };
                let _ = fs::remove_dir(&cwd);
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        unsafe { libc::kill(-pane.pid, libc::SIGKILL) };
        let _ = fs::remove_dir(&cwd);
        panic!("PTY shell did not start in {}", cwd.display());
    }

    #[test]
    fn environment_forwarding_is_minimal() {
        assert!(allowed_env("TERM"));
        assert!(allowed_env("LANG"));
        assert!(!allowed_env("ZELLIJ_CONFIG_DIR"));
        assert!(!allowed_env("HOME"));
        assert!(!allowed_env("FLOCK_PANE_ID"));
        assert!(!allowed_env("FLOCK_EXECUTABLE"));
        assert!(!allowed_env("FLOCK_STATE_CHANNEL"));

        let pane_id = Uuid::new_v4();
        let controlled = controlled_remote_pane_env(pane_id).unwrap();
        assert_eq!(controlled.get("FLOCK_PANE_ID"), Some(&pane_id.to_string()));
        assert_eq!(
            controlled.get("FLOCK_STATE_CHANNEL").map(String::as_str),
            Some("remote-agent")
        );
        assert!(controlled
            .get("FLOCK_EXECUTABLE")
            .is_some_and(|executable| !executable.is_empty()));
    }

    #[test]
    fn coder_bridge_uses_raw_terminal_mode_and_restores_it() {
        let mut master_fd = -1;
        let mut slave_fd = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let original = termios::tcgetattr(slave_fd).unwrap();
        let guard = RawTerminalGuard::enter(slave_fd).unwrap().unwrap();
        let raw = termios::tcgetattr(slave_fd).unwrap();
        assert!(!raw.local_flags.intersects(
            LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ISIG | LocalFlags::IEXTEN
        ));
        assert!(!raw
            .input_flags
            .intersects(InputFlags::ICRNL | InputFlags::IXON));
        assert!(!raw.output_flags.contains(OutputFlags::OPOST));
        drop(guard);
        let restored = termios::tcgetattr(slave_fd).unwrap();
        assert_eq!(restored.input_flags, original.input_flags);
        assert_eq!(restored.output_flags, original.output_flags);
        assert_eq!(restored.control_flags, original.control_flags);
        // macOS may add PENDIN while applying otherwise identical settings.
        assert_eq!(
            restored.local_flags - LocalFlags::PENDIN,
            original.local_flags - LocalFlags::PENDIN
        );
        assert_eq!(restored.control_chars, original.control_chars);
        unsafe {
            libc::close(master_fd);
            libc::close(slave_fd);
        }
    }
}
