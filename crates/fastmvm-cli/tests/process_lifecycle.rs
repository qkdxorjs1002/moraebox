#![forbid(unsafe_code)]

use std::process::Command;

#[test]
fn propagates_exit_code_and_separate_output() {
    let output = Command::new(env!("CARGO_BIN_EXE_fastmvm"))
        .args([
            "run",
            "--backend",
            "process",
            "--",
            "/bin/sh",
            "-c",
            "printf stdout; printf stderr >&2; exit 7",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"stdout");
    assert_eq!(output.stderr, b"stderr");
}

#[test]
fn timeout_uses_the_conventional_exit_code() {
    let output = Command::new(env!("CARGO_BIN_EXE_fastmvm"))
        .args([
            "run",
            "--backend",
            "process",
            "--timeout",
            "20ms",
            "--",
            "/bin/sh",
            "-c",
            "sleep 30",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(124));
}
