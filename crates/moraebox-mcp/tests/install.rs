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
        "MORAE_MKE2FS",
        "MORAE_E2FSCK",
        "MORAE_REGISTRY_USERNAME",
        "MORAE_REGISTRY_PASSWORD",
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
    let cache = std::env::current_dir().unwrap().join(".moraebox/cache");
    let state = std::env::current_dir().unwrap().join(".moraebox/state");
    assert_eq!(
        dry_run("codex"),
        json!({
            "program": "codex",
            "args": [
                "mcp", "add", "moraebox",
                "--env", "MORAE_ROOTFS=fixture-rootfs",
                "--", "morae-mcp",
                "--backend", "libkrun",
                "--cache-dir", cache.to_str().unwrap(),
                "--state-dir", state.to_str().unwrap(),
                "--disk-size", "8589934592",
                "--cpus", "2",
                "--memory-mib", "512"
            ]
        })
    );
}

#[test]
fn claude_dry_run_is_user_scoped_and_uses_stdio() {
    let cache = std::env::current_dir().unwrap().join(".moraebox/cache");
    let state = std::env::current_dir().unwrap().join(".moraebox/state");
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
                "--cache-dir", cache.to_str().unwrap(),
                "--state-dir", state.to_str().unwrap(),
                "--disk-size", "8589934592",
                "--cpus", "2",
                "--memory-mib", "512"
            ]
        })
    );
}

#[test]
fn process_dry_run_warns_and_never_claims_isolation() {
    let cache = std::env::current_dir().unwrap().join(".moraebox/cache");
    let state = std::env::current_dir().unwrap().join(".moraebox/state");
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
                "--", "morae-mcp", "--backend", "process",
                "--cache-dir", cache.to_str().unwrap(),
                "--state-dir", state.to_str().unwrap(),
                "--disk-size", "8589934592",
                "--cpus", "2",
                "--memory-mib", "512"
            ]
        })
    );
}

#[test]
fn default_install_registers_python_312_for_both_agents() {
    let cache_dir = std::env::temp_dir().join(format!(
        "moraebox-mcp-install-default-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&cache_dir);

    for agent in ["codex", "claude-code"] {
        let output = run_mcp(&[
            "install",
            agent,
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            "--server-command",
            "morae-mcp",
            "--dry-run",
        ]);
        assert!(
            output.status.success(),
            "unexpected stderr for {agent}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        let args = value["args"].as_array().unwrap();
        assert!(args.contains(&json!("--image")));
        assert!(args.contains(&json!("docker.io/library/python:3.12")));
        assert!(args.contains(&json!("--cache-dir")));
        assert!(args.contains(&json!(cache_dir.to_str().unwrap())));
    }
    std::fs::remove_dir_all(cache_dir).unwrap();
}

#[test]
fn unconfigured_bare_invocation_prints_help() {
    let output = run_mcp(&[]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let expected_usage = format!(
        "Usage: morae-mcp{} [OPTIONS] [COMMAND]",
        std::env::consts::EXE_SUFFIX
    );
    assert!(
        stdout.contains(&expected_usage),
        "unexpected help output:\n{stdout}"
    );
    assert!(stdout.contains("install"));
}
