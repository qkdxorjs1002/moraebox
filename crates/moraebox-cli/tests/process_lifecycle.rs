#![forbid(unsafe_code)]

use std::process::Command;

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
    command.args(["run", "--backend", "process", "--timeout", "20ms", "--"]);
    command.args(long_running_command());
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(124));
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
