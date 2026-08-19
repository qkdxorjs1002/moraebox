use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

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
fn libkrun_install_reports_how_to_prepare_a_missing_rootfs() {
    let cache_dir = std::env::temp_dir().join(format!(
        "moraebox-mcp-install-missing-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&cache_dir);
    let output = run_mcp(&[
        "install",
        "codex",
        "--server-command",
        "morae-mcp",
        "--cache-dir",
        cache_dir.to_str().unwrap(),
        "--dry-run",
    ]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no prepared rootfs found"));
    assert!(stderr.contains("morae image pull alpine@latest"));
}

#[test]
fn both_agents_discover_a_single_completed_cached_rootfs() {
    let cache = TestCache::new();
    let expected_rootfs = cache.add_rootfs();

    for agent in ["codex", "claude-code"] {
        let output = run_mcp(&[
            "install",
            agent,
            "--cache-dir",
            cache.path.to_str().unwrap(),
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
        let assignment = format!("MORAE_ROOTFS={}", expected_rootfs.display());
        assert!(
            value["args"]
                .as_array()
                .unwrap()
                .contains(&json!(assignment))
        );
    }
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

struct TestCache {
    path: PathBuf,
}

impl TestCache {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("moraebox-mcp-install-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        Self { path }
    }

    fn add_rootfs(&self) -> PathBuf {
        let rootfs = self.path.join("rootfs/sha256/digest");
        fs::create_dir_all(&rootfs).unwrap();
        fs::write(rootfs.join(".moraebox-rootfs-complete"), "digest").unwrap();
        rootfs.canonicalize().unwrap()
    }
}

impl Drop for TestCache {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
