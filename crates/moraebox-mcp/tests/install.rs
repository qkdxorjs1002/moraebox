use std::process::{Command, Output};

use moraebox_core::{resolve_cache_dir, resolve_state_dir};
use serde_json::{Value, json};

fn mcp_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_morae-mcp"));
    for name in [
        "MORAE_ROOTFS",
        "MORAE_HELPER_PATH",
        "MORAE_LIBKRUN_PATH",
        "MORAE_LIBKRUNFW_PATH",
        "MORAE_GVPROXY_PATH",
        "MORAE_LIB_DIR",
        "MORAE_MKE2FS",
        "MORAE_E2FSCK",
        "MORAE_REGISTRY_USERNAME",
        "MORAE_REGISTRY_PASSWORD",
    ] {
        command.env_remove(name);
    }
    command
}

fn run_mcp(args: &[&str]) -> Output {
    let mut command = mcp_command();
    command.args(args);
    command.output().unwrap()
}

#[cfg(unix)]
struct FakeCodex {
    _temporary: tempfile::TempDir,
    directory: std::path::PathBuf,
    marker: std::path::PathBuf,
}

#[cfg(unix)]
impl FakeCodex {
    fn new(exit_code: i32) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().to_path_buf();
        let marker = directory.join("agent-invoked");
        let codex = directory.join("codex");
        std::fs::write(
            &codex,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$MORAE_TEST_MARKER\"\nexit {exit_code}\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            _temporary: temporary,
            directory,
            marker,
        }
    }

    fn configure(&self, command: &mut Command) {
        let mut paths = vec![self.directory.clone()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command.env("PATH", std::env::join_paths(paths).unwrap());
        command.env("MORAE_TEST_MARKER", &self.marker);
    }
}

fn dry_run(agent: &str) -> Value {
    let server = env!("CARGO_BIN_EXE_morae-mcp");
    let output = run_mcp(&[
        "install",
        agent,
        "--rootfs",
        "fixture-rootfs",
        "--server-command",
        server,
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

fn string_args(value: &Value) -> Vec<&str> {
    value["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect()
}

fn assert_launch_paths_are_absolute(args: &[&str]) {
    let separator = args.iter().position(|arg| *arg == "--").unwrap();
    assert!(std::path::Path::new(args[separator + 1]).is_absolute());
    for argument in &args[..separator] {
        let Some((name, value)) = argument.split_once('=') else {
            continue;
        };
        if name == "MORAE_LIB_DIR" {
            assert!(std::env::split_paths(value).all(|path| path.is_absolute()));
        } else if name.starts_with("MORAE_") {
            assert!(
                std::path::Path::new(value).is_absolute(),
                "{name} was not absolute: {value}"
            );
        }
    }
    for option in ["--cache-dir", "--state-dir"] {
        let index = args.iter().position(|arg| *arg == option).unwrap();
        assert!(std::path::Path::new(args[index + 1]).is_absolute());
    }
}

#[test]
fn codex_dry_run_uses_official_stdio_cli_shape() {
    let value = dry_run("codex");
    assert_eq!(value["program"], "codex");
    let args = string_args(&value);
    assert_eq!(args[..3], ["mcp", "add", "moraebox"]);
    assert!(args.iter().any(|arg| arg.starts_with("MORAE_ROOTFS=")));
    assert_launch_paths_are_absolute(&args);
}

#[test]
fn claude_dry_run_is_user_scoped_and_uses_stdio() {
    let value = dry_run("claude-code");
    assert_eq!(value["program"], "claude");
    let args = string_args(&value);
    assert_eq!(args[..4], ["mcp", "add", "--scope", "user"]);
    let transport = args.iter().position(|arg| *arg == "--transport").unwrap();
    assert_eq!(
        args[transport..transport + 3],
        ["--transport", "stdio", "moraebox"]
    );
    assert_launch_paths_are_absolute(&args);
}

#[test]
fn process_dry_run_warns_and_never_claims_isolation() {
    let cache = resolve_cache_dir(None).unwrap();
    let state = resolve_state_dir(None).unwrap();
    let server = env!("CARGO_BIN_EXE_morae-mcp");
    let output = run_mcp(&[
        "install",
        "codex",
        "--backend",
        "process",
        "--server-command",
        server,
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
                "--", server, "--backend", "process",
                "--cache-dir", cache.to_str().unwrap(),
                "--state-dir", state.to_str().unwrap(),
                "--disk-size", "8589934592"
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
            env!("CARGO_BIN_EXE_morae-mcp"),
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

#[cfg(unix)]
#[test]
fn successful_initialize_preflight_precedes_agent_registration() {
    let fake = FakeCodex::new(0);
    let mut command = mcp_command();
    command.args([
        "install",
        "codex",
        "--backend",
        "process",
        "--server-command",
        env!("CARGO_BIN_EXE_morae-mcp"),
    ]);
    fake.configure(&mut command);

    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fake.marker.is_file(), "agent CLI was not invoked");
}

#[cfg(unix)]
#[test]
fn failed_initialize_preflight_leaves_agent_configuration_untouched() {
    use std::os::unix::fs::PermissionsExt;

    let fake = FakeCodex::new(0);
    let bad_server = fake.directory.join("bad-server");
    std::fs::write(&bad_server, "#!/bin/sh\necho not-json\n").unwrap();
    std::fs::set_permissions(&bad_server, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut command = mcp_command();
    command
        .arg("install")
        .arg("codex")
        .arg("--backend")
        .arg("process")
        .arg("--server-command")
        .arg(&bad_server);
    fake.configure(&mut command);

    let output = command.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(stderr.contains("before agent configuration was changed"));
    assert!(stderr.contains("not valid JSON"));
    assert!(
        !fake.marker.exists(),
        "agent CLI must not run after failed preflight"
    );
}

#[cfg(unix)]
#[test]
fn agent_failure_includes_non_destructive_rollback_guidance() {
    let fake = FakeCodex::new(7);
    let mut command = mcp_command();
    command.args([
        "install",
        "codex",
        "--name",
        "sandbox_test",
        "--backend",
        "process",
        "--server-command",
        env!("CARGO_BIN_EXE_morae-mcp"),
    ]);
    fake.configure(&mut command);

    let output = command.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(
        fake.marker.is_file(),
        "agent CLI should have been attempted"
    );
    assert!(stderr.contains("may have been partially updated"));
    assert!(stderr.contains("codex mcp remove sandbox_test"));
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
