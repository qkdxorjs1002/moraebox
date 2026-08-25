#![forbid(unsafe_code)]

use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn interactive_streams_output_before_stdin_eof() {
    let mut child = interactive_command(interactive_echo_command())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut stdout = BufReader::new(stdout);
        let mut ready = String::new();
        stdout.read_line(&mut ready).unwrap();
        sender.send(ready).unwrap();
        let mut remainder = String::new();
        stdout.read_to_string(&mut remainder).unwrap();
        sender.send(remainder).unwrap();
    });

    let ready = receive_or_kill(&receiver, &mut child, "initial interactive output");
    assert_eq!(ready.trim_end(), "ready");
    stdin.write_all(b"hello\n").unwrap();
    drop(stdin);

    let remainder = receive_or_kill(&receiver, &mut child, "interactive response");
    assert_eq!(remainder.trim_end(), "done:hello");
    let status = wait_or_kill(&mut child);
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    reader.join().unwrap();
    assert!(status.success(), "interactive command failed: {stderr}");
}

#[cfg(unix)]
#[test]
fn interactive_ignores_host_eof_during_command_cleanup() {
    let mut child = interactive_command(interactive_cleanup_race_command())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    stdout.read_line(&mut ready).unwrap();
    assert_eq!(ready.trim_end(), "ready");

    stdin.write_all(b"hello\n").unwrap();
    thread::sleep(Duration::from_millis(200));
    drop(stdin);

    let mut remainder = String::new();
    stdout.read_to_string(&mut remainder).unwrap();
    let status = wait_or_kill(&mut child);
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(status.success(), "interactive command failed: {stderr}");
    assert_eq!(remainder.trim_end(), "done:hello");
}

#[test]
fn interactive_keeps_session_open_between_input_rounds() {
    let mut child = interactive_command(interactive_two_round_command())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut stdout = BufReader::new(stdout);
        for _ in 0..3 {
            let mut line = String::new();
            stdout.read_line(&mut line).unwrap();
            sender.send(line).unwrap();
        }
    });

    assert_eq!(
        receive_or_kill(&receiver, &mut child, "initial interactive output").trim_end(),
        "ready"
    );
    stdin.write_all(b"first\n").unwrap();
    assert_eq!(
        receive_or_kill(&receiver, &mut child, "first interactive response").trim_end(),
        "round:first"
    );
    assert!(child.try_wait().unwrap().is_none());
    stdin.write_all(b"second\n").unwrap();
    drop(stdin);
    assert_eq!(
        receive_or_kill(&receiver, &mut child, "second interactive response").trim_end(),
        "round:second"
    );

    let status = wait_or_kill(&mut child);
    reader.join().unwrap();
    assert!(status.success());
}

#[test]
fn interactive_streams_stderr_before_stdin_eof() {
    let mut child = interactive_command(interactive_stderr_command())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut stderr = BufReader::new(stderr);
        let mut ready = String::new();
        stderr.read_line(&mut ready).unwrap();
        sender.send(ready).unwrap();
    });

    let ready = receive_or_kill(&receiver, &mut child, "initial interactive stderr");
    assert_eq!(ready.trim_end(), "ready-error");
    stdin.write_all(b"continue\n").unwrap();
    drop(stdin);
    let status = wait_or_kill(&mut child);
    reader.join().unwrap();
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn interactive_shared_pty_backpressure_does_not_abort_output() {
    use std::fs::File;

    use nix::{
        fcntl::{FcntlArg, OFlag, fcntl},
        pty::openpty,
    };

    let pty = openpty(None, None).unwrap();
    let mut master = File::from(pty.master);
    let slave = File::from(pty.slave);
    let original_flags = OFlag::from_bits_truncate(fcntl(&slave, FcntlArg::F_GETFL).unwrap());
    let master_flags = OFlag::from_bits_truncate(fcntl(&master, FcntlArg::F_GETFL).unwrap());
    fcntl(&master, FcntlArg::F_SETFL(master_flags | OFlag::O_NONBLOCK)).unwrap();

    let mut child = interactive_command(interactive_backpressure_command())
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave.try_clone().unwrap()))
        .spawn()
        .unwrap();
    let mut output = Vec::new();
    read_pty_until(&mut master, &mut output, "ready");
    let active_flags = OFlag::from_bits_truncate(fcntl(&slave, FcntlArg::F_GETFL).unwrap());

    master.write_all(b"go\n").unwrap();
    thread::sleep(Duration::from_millis(100));
    read_pty_until(&mut master, &mut output, "marker:go");
    let status = wait_or_kill(&mut child);

    assert_eq!(
        active_flags, original_flags,
        "interactive input must not change flags shared by PTY output descriptors"
    );
    assert!(status.success(), "interactive command failed: {status}");
    assert!(output.len() >= 256 * 1024, "large output was truncated");
}

#[cfg(unix)]
#[test]
fn interactive_forwards_sigint_to_the_command() {
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };

    let mut child = interactive_command(interrupt_command())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    stdout.read_line(&mut ready).unwrap();
    assert_eq!(ready.trim_end(), "ready");

    let pid = i32::try_from(child.id()).unwrap();
    kill(Pid::from_raw(pid), Signal::SIGINT).unwrap();
    let status = wait_or_kill(&mut child);
    let mut remainder = String::new();
    stdout.read_to_string(&mut remainder).unwrap();
    assert_eq!(status.code(), Some(7));
    assert_eq!(remainder.trim_end(), "interrupted");
}

#[cfg(unix)]
#[test]
fn interactive_forwards_sigterm_to_the_command() {
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };

    let mut child = interactive_command(terminate_command())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    stdout.read_line(&mut ready).unwrap();
    assert_eq!(ready.trim_end(), "ready");

    let pid = i32::try_from(child.id()).unwrap();
    kill(Pid::from_raw(pid), Signal::SIGTERM).unwrap();
    let status = wait_or_kill(&mut child);
    let mut remainder = String::new();
    stdout.read_to_string(&mut remainder).unwrap();
    assert_eq!(status.code(), Some(8));
    assert_eq!(remainder.trim_end(), "terminated");
}

#[cfg(unix)]
#[test]
fn interactive_early_exit_does_not_wait_for_host_stdin() {
    let mut child = interactive_command(vec!["/usr/bin/true".into()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _stdin_kept_open = child.stdin.take().unwrap();
    let status = wait_or_kill(&mut child);
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn raw_terminal_is_restored_when_backend_start_fails() {
    use std::fs::File;

    use nix::{
        pty::openpty,
        sys::termios::{LocalFlags, tcgetattr},
    };

    let pty = openpty(None, None).unwrap();
    let master = File::from(pty.master);
    let slave = File::from(pty.slave);
    let original = tcgetattr(&slave).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_morae"))
        .args([
            "run",
            "--backend",
            "process",
            "--tty",
            "--interactive",
            "--",
            "/usr/bin/true",
        ])
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave.try_clone().unwrap()))
        .spawn()
        .unwrap();
    let status = wait_or_kill(&mut child);
    assert_eq!(status.code(), Some(1));
    let restored = tcgetattr(&slave).unwrap();
    assert_eq!(restored.input_flags, original.input_flags);
    assert_eq!(restored.output_flags, original.output_flags);
    assert_eq!(restored.control_flags, original.control_flags);
    assert_eq!(restored.control_chars, original.control_chars);
    assert_eq!(
        restored.local_flags - LocalFlags::PENDIN,
        original.local_flags - LocalFlags::PENDIN
    );
    drop((master, slave));
}

#[test]
fn interactive_rejects_json_output() {
    let output = Command::new(env!("CARGO_BIN_EXE_morae"))
        .args([
            "run",
            "--backend",
            "process",
            "--interactive",
            "--json",
            "--",
        ])
        .args(immediate_success_command())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        document
            .pointer("/error/message")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|message| message.contains("--interactive cannot be combined with --json"))
    );
}

fn interactive_command(command: Vec<String>) -> Command {
    let mut process = Command::new(env!("CARGO_BIN_EXE_morae"));
    process.args(["run", "--backend", "process", "--interactive", "--"]);
    process.args(command);
    process
}

fn receive_or_kill(
    receiver: &mpsc::Receiver<String>,
    child: &mut Child,
    description: &str,
) -> String {
    receiver.recv_timeout(TEST_TIMEOUT).unwrap_or_else(|error| {
        let _ = child.kill();
        let _ = child.wait();
        panic!("timed out waiting for {description}: {error}");
    })
}

fn wait_or_kill(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + TEST_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let status = child.wait().unwrap();
            panic!("interactive command timed out; forced status: {status}");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn read_pty_until(reader: &mut std::fs::File, output: &mut Vec<u8>, marker: &str) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    let mut buffer = [0_u8; 4096];
    while !String::from_utf8_lossy(output).contains(marker) {
        match reader.read(&mut buffer) {
            Ok(0) => panic!("PTY closed before output contained {marker:?}"),
            Ok(count) => output.extend_from_slice(&buffer[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "PTY output did not contain {marker:?}: {}",
                    String::from_utf8_lossy(output)
                );
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("read PTY output while waiting for {marker:?}: {error}"),
        }
    }
}

#[cfg(unix)]
fn interactive_echo_command() -> Vec<String> {
    [
        "/bin/sh",
        "-c",
        "printf 'ready\\n'; read line; printf 'done:%s\\n' \"$line\"",
    ]
    .map(String::from)
    .to_vec()
}

#[cfg(unix)]
fn interactive_two_round_command() -> Vec<String> {
    [
        "/bin/sh",
        "-c",
        "printf 'ready\\n'; IFS= read -r first; printf 'round:%s\\n' \"$first\"; IFS= read -r second; printf 'round:%s\\n' \"$second\"",
    ]
    .map(String::from)
    .to_vec()
}

#[cfg(unix)]
fn interactive_cleanup_race_command() -> Vec<String> {
    [
        "/bin/sh",
        "-c",
        "printf 'ready\\n'; IFS= read -r line; sleep 1 & printf 'done:%s\\n' \"$line\"",
    ]
    .map(String::from)
    .to_vec()
}

#[cfg(windows)]
fn interactive_two_round_command() -> Vec<String> {
    vec![
        windows_system_executable("cmd.exe"),
        "/D".into(),
        "/V:ON".into(),
        "/C".into(),
        "echo ready&set /p first=&echo round:!first!&set /p second=&echo round:!second!".into(),
    ]
}

#[cfg(unix)]
fn interactive_stderr_command() -> Vec<String> {
    ["/bin/sh", "-c", "printf 'ready-error\\n' >&2; read line"]
        .map(String::from)
        .to_vec()
}

#[cfg(unix)]
fn interactive_backpressure_command() -> Vec<String> {
    [
        "/bin/sh",
        "-c",
        "printf 'ready\\n'; IFS= read -r line; i=0; while [ \"$i\" -lt 8192 ]; do printf 0123456789abcdef0123456789abcdef; i=$((i + 1)); done; printf '\\nmarker:%s\\n' \"$line\"",
    ]
    .map(String::from)
    .to_vec()
}

#[cfg(windows)]
fn interactive_stderr_command() -> Vec<String> {
    vec![
        windows_system_executable("cmd.exe"),
        "/D".into(),
        "/C".into(),
        "echo ready-error>&2&set /p line=".into(),
    ]
}

#[cfg(windows)]
fn interactive_echo_command() -> Vec<String> {
    vec![
        windows_system_executable("cmd.exe"),
        "/D".into(),
        "/V:ON".into(),
        "/C".into(),
        "echo ready&set /p line=&echo done:!line!".into(),
    ]
}

#[cfg(unix)]
fn interrupt_command() -> Vec<String> {
    [
        "/bin/sh",
        "-c",
        "trap 'echo interrupted; exit 7' INT; printf 'ready\\n'; while :; do sleep 1; done",
    ]
    .map(String::from)
    .to_vec()
}

#[cfg(unix)]
fn terminate_command() -> Vec<String> {
    [
        "/bin/sh",
        "-c",
        "trap 'echo terminated; exit 8' TERM; printf 'ready\\n'; while :; do sleep 1; done",
    ]
    .map(String::from)
    .to_vec()
}

#[cfg(unix)]
fn immediate_success_command() -> Vec<String> {
    vec!["/usr/bin/true".into()]
}

#[cfg(windows)]
fn immediate_success_command() -> Vec<String> {
    vec![
        windows_system_executable("cmd.exe"),
        "/D".into(),
        "/C".into(),
        "exit /b 0".into(),
    ]
}

#[cfg(windows)]
fn windows_system_executable(name: &str) -> String {
    std::path::PathBuf::from(
        std::env::var_os("SystemRoot").expect("Windows must define SystemRoot"),
    )
    .join("System32")
    .join(name)
    .to_string_lossy()
    .into_owned()
}
