#![forbid(unsafe_code)]

use std::process::Command;

#[cfg(unix)]
use std::{
    thread,
    time::{Duration, Instant},
};

#[test]
fn propagates_exit_code_and_separate_output() {
    let mut command = Command::new(env!("CARGO_BIN_EXE_morae"));
    command.args(["run", "--backend", "process", "--"]);
    command.args(output_and_exit_command());
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, expected_stdout());
    assert_eq!(output.stderr, expected_stderr());
}

#[test]
fn timeout_uses_the_conventional_exit_code() {
    let mut command = Command::new(env!("CARGO_BIN_EXE_morae"));
    command.args(["run", "--backend", "process", "--timeout", "500ms", "--"]);
    command.args(long_running_command());
    let output = command.output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(124),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
fn interrupt_non_interactive_process(script: &str) -> (std::process::Output, Duration) {
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };

    let temporary = tempfile::tempdir().unwrap();
    let ready = temporary.path().join("ready");
    let mut child = Command::new(env!("CARGO_BIN_EXE_morae"))
        .args([
            "run",
            "--backend",
            "process",
            "--json",
            "--timeout",
            "10s",
            "--kill-grace",
            "250ms",
            "--env",
        ])
        .arg(format!("MORAE_READY_FILE={}", ready.display()))
        .args(["--", "/bin/sh", "-c", script])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "morae exited before the process command became ready"
        );
        assert!(
            Instant::now() < deadline,
            "process command did not become ready"
        );
        thread::sleep(Duration::from_millis(10));
    }

    let interrupted = Instant::now();
    kill(
        Pid::from_raw(i32::try_from(child.id()).unwrap()),
        Signal::SIGINT,
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();
    (output, interrupted.elapsed())
}

#[cfg(unix)]
#[test]
fn non_interactive_sigint_waits_for_cleanup_and_reports_signal() {
    let (output, _) = interrupt_non_interactive_process(
        "printf ready > \"$MORAE_READY_FILE\"; exec /bin/sleep 5",
    );

    assert_eq!(
        output.status.code(),
        Some(130),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["state"], "dead");
    assert_eq!(report["signal"], 2);
    assert_eq!(report["timed_out"], false);
    assert_eq!(
        report["trace"]
            .as_array()
            .and_then(|trace| trace.last())
            .and_then(|event| event["kind"].as_str()),
        Some("cleanup_complete")
    );
}

#[cfg(unix)]
#[test]
fn non_interactive_sigint_forces_an_ignoring_command_after_the_grace_period() {
    let (output, elapsed) = interrupt_non_interactive_process(
        "trap '' INT TERM; printf ready > \"$MORAE_READY_FILE\"; exec /bin/sleep 5",
    );

    assert!(
        elapsed < Duration::from_secs(3),
        "ignored SIGINT cleanup took {elapsed:?}"
    );
    assert_eq!(
        output.status.code(),
        Some(137),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["state"], "dead");
    assert_eq!(report["termination_reason"], "cancelled");
    assert_eq!(report["signal"], 9);
    assert_eq!(report["timed_out"], false);
    let trace = report["trace"].as_array().unwrap();
    assert!(trace.iter().any(|event| event["kind"] == "forced_stop"));
    assert_eq!(
        trace.last().and_then(|event| event["kind"].as_str()),
        Some("cleanup_complete")
    );
}

#[test]
fn process_backend_rejects_the_vm_network_option() {
    let output = Command::new(env!("CARGO_BIN_EXE_morae"))
        .args([
            "run",
            "--backend",
            "process",
            "--network",
            "--",
            "not-executed",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--network requires --backend libkrun")
    );
}

#[test]
fn json_execution_errors_use_the_stable_envelope_on_stdout() {
    let output = Command::new(env!("CARGO_BIN_EXE_morae"))
        .args([
            "run",
            "--backend",
            "process",
            "--network",
            "--json",
            "--",
            "not-executed",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        document.pointer("/error/code"),
        Some(&serde_json::json!("execution_failed"))
    );
    assert_eq!(
        document.pointer("/error/stage"),
        Some(&serde_json::json!("run"))
    );
    assert_eq!(
        document.pointer("/error/retryable"),
        Some(&serde_json::json!(false))
    );
    assert!(
        document
            .pointer("/error/message")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|message| message.contains("--network requires --backend libkrun"))
    );
    assert!(
        document
            .pointer("/error/remediation")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|remediation| !remediation.is_empty())
    );
}

#[test]
fn json_execution_result_exposes_resolved_image_digest_slot() {
    let mut command = Command::new(env!("CARGO_BIN_EXE_morae"));
    command.args(["run", "--backend", "process", "--json", "--"]);
    command.args(output_and_exit_command());
    let output = command.output().unwrap();

    assert_eq!(output.status.code(), Some(7));
    assert!(output.stderr.is_empty());
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        document.pointer("/startup/resolved_image_digest"),
        Some(&serde_json::Value::Null)
    );
}

#[test]
fn process_backend_rejects_tty_from_typed_capabilities() {
    let output = Command::new(env!("CARGO_BIN_EXE_morae"))
        .args(["run", "--backend", "process", "--tty", "--", "not-executed"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("TTY support"));
}

#[test]
fn process_backend_rejects_a_guest_rootfs() {
    let output = Command::new(env!("CARGO_BIN_EXE_morae"))
        .args([
            "run",
            "--backend",
            "process",
            "--rootfs",
            "ignored-rootfs",
            "--",
            "not-executed",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--rootfs requires --backend libkrun")
    );
}

#[cfg(unix)]
#[test]
fn inherited_environment_is_resolved_once_with_explicit_precedence() {
    let output = Command::new(env!("CARGO_BIN_EXE_morae"))
        .env("MORAE_HOST_ONLY", "host")
        .env("MORAE_OVERRIDE", "host")
        .args([
            "run",
            "--backend",
            "process",
            "--inherit-env",
            "--env",
            "MORAE_OVERRIDE=explicit",
            "--",
            "/bin/sh",
            "-c",
            "printf '%s|%s' \"$MORAE_HOST_ONLY\" \"$MORAE_OVERRIDE\"",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(output.stdout, b"host|explicit");
}

#[cfg(unix)]
fn output_and_exit_command() -> Vec<String> {
    ["/bin/sh", "-c", "printf stdout; printf stderr >&2; exit 7"]
        .map(String::from)
        .into()
}

#[cfg(windows)]
fn output_and_exit_command() -> Vec<String> {
    vec![
        windows_system_executable("cmd.exe"),
        "/D".into(),
        "/C".into(),
        "echo stdout&echo stderr>&2&exit /b 7".into(),
    ]
}

#[cfg(unix)]
fn expected_stdout() -> &'static [u8] {
    b"stdout"
}

#[cfg(windows)]
fn expected_stdout() -> &'static [u8] {
    b"stdout\r\n"
}

#[cfg(unix)]
fn expected_stderr() -> &'static [u8] {
    b"stderr"
}

#[cfg(windows)]
fn expected_stderr() -> &'static [u8] {
    b"stderr\r\n"
}

#[cfg(unix)]
fn long_running_command() -> Vec<String> {
    ["/bin/sh", "-c", "sleep 30"].map(String::from).into()
}

#[cfg(windows)]
fn long_running_command() -> Vec<String> {
    vec![
        windows_system_executable("ping.exe"),
        "-n".into(),
        "31".into(),
        "127.0.0.1".into(),
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
