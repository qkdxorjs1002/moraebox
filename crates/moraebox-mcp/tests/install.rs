use std::process::{Command, Output};

use serde_json::{Value, json};

fn run_mcp(args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_morae-mcp"));
    command.args(args);
    for name in [
        "MORAE_ROOTFS",
        "MORAE_HELPER_PATH",
        "MORAE_LIBKRUN_PATH",
        "MORAE_LIB_DIR",
    ] {
        command.env_remove(name);
    }
    command.output().unwrap()
}

fn dry_run(agent: &str) -> Value {
    let output = run_mcp(&[
        "install",
        agent,
        "--rootfs",
        "fixture-rootfs",
        "--server-command",
        "morae-mcp",
        "--dry-run",
    ]);
    assert!(
        output.status.success(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn codex_dry_run_uses_official_stdio_cli_shape() {
    assert_eq!(
        dry_run("codex"),
        json!({
            "program": "codex",
            "args": [
                "mcp", "add", "moraebox",
                "--env", "MORAE_ROOTFS=fixture-rootfs",
                "--", "morae-mcp",
                "--backend", "libkrun",
                "--cpus", "2",
                "--memory-mib", "512"
            ]
        })
    );
}

#[test]
fn claude_dry_run_is_user_scoped_and_uses_stdio() {
    assert_eq!(
        dry_run("claude-code"),
        json!({
            "program": "claude",
            "args": [
                "mcp", "add",
                "--scope", "user",
                "--env", "MORAE_ROOTFS=fixture-rootfs",
                "--transport", "stdio",
                "moraebox",
                "--", "morae-mcp",
                "--backend", "libkrun",
                "--cpus", "2",
                "--memory-mib", "512"
            ]
        })
    );
}

#[test]
fn process_dry_run_warns_and_never_claims_isolation() {
    let output = run_mcp(&[
        "install",
        "codex",
        "--backend",
        "process",
        "--server-command",
        "morae-mcp",
        "--dry-run",
    ]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not provide VM isolation"));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value,
        json!({
            "program": "codex",
            "args": [
                "mcp", "add", "moraebox",
                "--", "morae-mcp", "--backend", "process"
            ]
        })
    );
}

#[test]
fn libkrun_install_requires_rootfs_before_agent_execution() {
    let output = run_mcp(&[
        "install",
        "codex",
        "--server-command",
        "morae-mcp",
        "--dry-run",
    ]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("libkrun registration requires --rootfs or MORAE_ROOTFS")
    );
}

#[test]
fn unconfigured_bare_invocation_prints_help() {
    let output = run_mcp(&[]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Usage: morae-mcp [OPTIONS] [COMMAND]"));
    assert!(stdout.contains("install"));
}
