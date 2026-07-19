use super::*;
use zellij_utils::input::command::RunCommand;

fn make_server() -> ServerOsInputOutput {
    get_server_os_input().expect("failed to create server os input")
}

// --- Cross-platform command helpers ---

#[allow(dead_code)]
#[cfg(not(windows))]
fn long_running_cmd() -> Command {
    let mut cmd = Command::new("sleep");
    cmd.arg("60");
    cmd
}

#[allow(dead_code)]
#[cfg(windows)]
fn long_running_cmd() -> Command {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new("timeout");
    cmd.args(&["/T", "60"]);
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    cmd
}

#[allow(dead_code)]
#[cfg(not(windows))]
fn echo_cmd(msg: &str) -> Command {
    let mut cmd = Command::new("echo");
    cmd.arg(msg);
    cmd
}

#[allow(dead_code)]
#[cfg(windows)]
fn echo_cmd(msg: &str) -> Command {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new("cmd");
    cmd.args(&["/C", "echo", msg]);
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    cmd
}

#[allow(dead_code)]
#[cfg(not(windows))]
fn stdin_reader_cmd() -> Command {
    let mut cmd = Command::new("cat");
    cmd.stdin(std::process::Stdio::piped());
    cmd
}

#[allow(dead_code)]
#[cfg(windows)]
fn stdin_reader_cmd() -> Command {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new("findstr");
    cmd.arg("/R").arg(".*");
    cmd.stdin(std::process::Stdio::piped());
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    cmd
}

#[test]
fn get_cwd() {
    let server = make_server();

    let pid = std::process::id();
    assert!(
        server.get_cwd(pid).is_some(),
        "Get current working directory from PID {}",
        pid
    );
}

// --- Signal delivery tests ---

#[cfg(not(windows))]
#[test]
fn kill_sends_sighup_to_process() {
    let child = long_running_cmd()
        .spawn()
        .expect("failed to spawn long-running process");
    let pid = child.id();

    let server = make_server();

    server.kill(pid).expect("kill should succeed");

    // Give the signal time to be delivered
    std::thread::sleep(std::time::Duration::from_millis(100));
}

#[cfg(not(windows))]
#[test]
fn force_kill_sends_sigkill_to_process() {
    let child = long_running_cmd()
        .spawn()
        .expect("failed to spawn long-running process");
    let pid = child.id();

    let server = make_server();

    server.force_kill(pid).expect("force_kill should succeed");

    std::thread::sleep(std::time::Duration::from_millis(100));
}

#[cfg(not(windows))]
#[test]
fn send_sigint_to_process() {
    let child = stdin_reader_cmd()
        .spawn()
        .expect("failed to spawn stdin-reader process");
    let pid = child.id();

    let server = make_server();

    server.send_sigint(pid).expect("send_sigint should succeed");

    std::thread::sleep(std::time::Duration::from_millis(100));
}

#[test]
fn spawn_and_read_output() {
    use crate::panes::PaneId;
    use zellij_utils::input::command::TerminalAction;

    let server = make_server();
    let test_message = "hello_zellij_test";

    #[cfg(not(windows))]
    let cmd = RunCommand {
        command: PathBuf::from("echo"),
        args: vec![test_message.to_string()],
        ..Default::default()
    };
    #[cfg(windows)]
    let cmd = RunCommand {
        command: PathBuf::from("cmd"),
        args: vec![
            "/K".to_string(),
            "echo".to_string(),
            test_message.to_string(),
        ],
        ..Default::default()
    };

    let action = TerminalAction::RunCommand(cmd);
    let quit_cb: Box<dyn Fn(PaneId, Option<i32>, RunCommand) + Send> =
        Box::new(|_pane_id, _exit_status, _run_command| {});

    let (_terminal_id, mut reader, _child_pid) = server
        .spawn_terminal(action, quit_cb, None)
        .expect("spawn_terminal should succeed");

    // Read output from the spawned terminal
    let mut output = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        loop {
            if std::time::Instant::now() > deadline {
                break;
            }
            match tokio::time::timeout(std::time::Duration::from_millis(500), reader.read(&mut buf))
                .await
            {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => {
                    output.extend_from_slice(&buf[..n]);
                    let s = String::from_utf8_lossy(&output);
                    if s.contains(test_message) {
                        break;
                    }
                },
                Ok(Err(_)) => break,
                Err(_) => {
                    // timeout — check if we already have enough
                    let s = String::from_utf8_lossy(&output);
                    if s.contains(test_message) {
                        break;
                    }
                },
            }
        }
    });

    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains(test_message),
        "expected output to contain '{}', got: '{}'",
        test_message,
        output_str
    );
}

// --- Foreground command resolution through the process tree ---

fn entry(pid: u32, ppid: u32, pgid: u32, foreground: bool, cmd: &[&str]) -> ProcessEntry {
    ProcessEntry {
        pid,
        ppid,
        pgid,
        foreground,
        cmd: cmd.iter().map(|p| p.to_string()).collect(),
    }
}

#[test]
fn foreground_cmd_resolves_direct_child() {
    // fish(10) → claude(20): claude leads the tty's foreground group.
    let table = vec![entry(20, 10, 20, true, &["claude"])];
    assert_eq!(
        foreground_descendant_cmd(&table, 10),
        Some(vec!["claude".to_string()])
    );
}

#[test]
fn foreground_cmd_sees_through_devenv_wrapper() {
    // Observed live devenv topology: the wrapper chain stays in the *outer*
    // shell's process group (foreground on the outer tty but never a group
    // leader), the inner shell is a session leader on a nested pty without
    // the foreground flag, and the agent leads the nested pty's foreground
    // group. Only the agent satisfies `foreground && pid == pgid`.
    let table = vec![
        entry(
            20,
            10,
            10,
            true,
            &["fish", "--no-config", "-c", "devenv shell"],
        ),
        entry(30, 20, 10, true, &["devenv", "shell"]),
        entry(40, 30, 40, false, &["zsh", "-i"]),
        entry(50, 40, 50, true, &["claude", "--continue"]),
    ];
    assert_eq!(
        foreground_descendant_cmd(&table, 10),
        Some(vec!["claude".to_string(), "--continue".to_string()])
    );
}

#[test]
fn foreground_cmd_resolves_inner_shell_when_agent_exits_in_devenv() {
    // Same topology after the agent exits: the inner shell takes the nested
    // pty's foreground group back, so it is the deepest leader and the pane
    // reads as a plain shell again (not as the devenv wrapper).
    let table = vec![
        entry(
            20,
            10,
            10,
            true,
            &["fish", "--no-config", "-c", "devenv shell"],
        ),
        entry(30, 20, 10, true, &["devenv", "shell"]),
        entry(40, 30, 40, true, &["zsh", "-i"]),
    ];
    assert_eq!(
        foreground_descendant_cmd(&table, 10),
        Some(vec!["zsh".to_string(), "-i".to_string()])
    );
}

#[test]
fn foreground_cmd_prefers_deepest_leader_across_nested_ptys() {
    // A wrapper that proxies a nested pty (devenv painting its watch bar)
    // leads the *outer* tty's foreground group itself, while the agent leads
    // the inner pty's. The deepest leader is the interactive program.
    let table = vec![
        entry(20, 10, 20, true, &["devenv", "shell"]),
        entry(30, 20, 30, false, &["bash"]),
        entry(40, 30, 40, true, &["claude"]),
    ];
    assert_eq!(
        foreground_descendant_cmd(&table, 10),
        Some(vec!["claude".to_string()])
    );
}

#[test]
fn foreground_cmd_ignores_non_leader_group_members() {
    // claude(20) spawned git(21) inside its own process group: git carries
    // the foreground flag as a group member but is not the leader.
    let table = vec![
        entry(20, 10, 20, true, &["claude"]),
        entry(21, 20, 20, true, &["git", "status"]),
    ];
    assert_eq!(
        foreground_descendant_cmd(&table, 10),
        Some(vec!["claude".to_string()])
    );
}

#[test]
fn foreground_cmd_falls_back_to_newest_direct_child() {
    // Only a background job below the shell (or the scan missed the live
    // foreground line): fall back to the newest direct child, matching the
    // previous direct-child behavior.
    let table = vec![
        entry(20, 10, 20, false, &["cargo", "watch"]),
        entry(25, 10, 25, false, &["tail", "-f", "log"]),
    ];
    assert_eq!(
        foreground_descendant_cmd(&table, 10),
        Some(vec![
            "tail".to_string(),
            "-f".to_string(),
            "log".to_string()
        ])
    );
}

#[test]
fn foreground_cmd_none_when_shell_has_no_children() {
    let table = vec![entry(99, 1, 99, true, &["unrelated"])];
    assert_eq!(foreground_descendant_cmd(&table, 10), None);
}

#[test]
fn foreground_cmd_survives_ppid_cycles() {
    // A stale scan can pair reused pids into a parent cycle; the walk must
    // terminate and still resolve the leader.
    let table = vec![
        entry(20, 10, 20, false, &["wrapper"]),
        entry(30, 20, 30, true, &["claude"]),
        entry(10, 30, 10, false, &["ghost"]),
    ];
    assert_eq!(
        foreground_descendant_cmd(&table, 10),
        Some(vec!["claude".to_string()])
    );
}

#[test]
fn process_table_parses_ps_output() {
    let output = "\
  501   340   501  Ss   -fish
  502   501   502  S    devenv shell
  503   502   503  S+   claude --continue
  504   502   504  Z    \n";
    let table = parse_process_table(output);
    // The zombie line has no command and is dropped.
    assert_eq!(table.len(), 3);
    assert_eq!(
        table[2],
        ProcessEntry {
            pid: 503,
            ppid: 502,
            pgid: 503,
            foreground: true,
            cmd: vec!["claude".to_string(), "--continue".to_string()],
        }
    );
    assert!(!table[1].foreground);
}
