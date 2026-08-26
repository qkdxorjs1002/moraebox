#![forbid(unsafe_code)]

use std::{fs, process::Command};

fn morae() -> Command {
    Command::new(env!("CARGO_BIN_EXE_morae"))
}

fn write_profile(directory: &std::path::Path, body: &str) -> std::path::PathBuf {
    let path = directory.join("morae.toml");
    fs::write(&path, body).unwrap();
    path
}

#[test]
fn list_and_validate_use_only_the_exact_selected_file() {
    let temporary = tempfile::tempdir().unwrap();
    let path = write_profile(
        temporary.path(),
        "version = 1\n[profiles.dev]\ncommand = [\"true\"]\n[profiles.ci]\ncommand = [\"false\"]\n",
    );

    let list = morae()
        .args(["profile", "list", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(list.status.success());
    assert_eq!(list.stdout, b"ci\ndev\n");
    assert!(list.stderr.is_empty());

    let validate = morae()
        .args(["profile", "validate", "--config"])
        .arg(&path)
        .arg("--json")
        .output()
        .unwrap();
    assert!(validate.status.success());
    assert!(validate.stderr.is_empty());
    let document: serde_json::Value = serde_json::from_slice(&validate.stdout).unwrap();
    assert_eq!(document["valid"], true);
    assert_eq!(document["profile_count"], 2);
    assert_eq!(
        document["path"],
        fs::canonicalize(path).unwrap().to_string_lossy().as_ref()
    );
}

#[test]
fn normal_run_ignores_a_malformed_project_file() {
    let temporary = tempfile::tempdir().unwrap();
    write_profile(temporary.path(), "this is not TOML");
    let output = morae()
        .current_dir(temporary.path())
        .args(["run", "--backend", "process", "--", successful_command()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn profile_loading_does_not_search_parent_directories() {
    let temporary = tempfile::tempdir().unwrap();
    write_profile(
        temporary.path(),
        "version = 1\n[profiles.dev]\nbackend = \"process\"\ncommand = [\"true\"]\n",
    );
    let child = temporary.path().join("child");
    fs::create_dir(&child).unwrap();

    let output = morae()
        .current_dir(child)
        .args(["run", "--profile", "dev"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("child/morae.toml"));
}

#[test]
fn malformed_profiles_use_the_stable_json_error_contract() {
    let temporary = tempfile::tempdir().unwrap();
    let path = write_profile(
        temporary.path(),
        "version = 1\n[profiles.dev]\ninherit_env = true\n",
    );
    let output = morae()
        .args(["profile", "validate", "--config"])
        .arg(path)
        .arg("--json")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["error"]["code"], "profile_invalid");
    assert_eq!(document["error"]["stage"], "profile_load");
    assert_eq!(document["error"]["retryable"], false);
    assert!(
        document["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("unknown field"))
    );
}

#[cfg(unix)]
#[test]
fn process_profile_executes_argv_and_literal_environment() {
    let temporary = tempfile::tempdir().unwrap();
    write_profile(
        temporary.path(),
        r#"
version = 1
[profiles.dev]
backend = "process"
command = ["/bin/sh", "-c", "printf '%s' \"$PROFILE_VALUE\""]
[profiles.dev.env]
PROFILE_VALUE = "literal"
"#,
    );
    let output = morae()
        .current_dir(temporary.path())
        .args(["run", "--profile", "dev"])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(output.stdout, b"literal");
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
fn successful_command() -> &'static str {
    "/usr/bin/true"
}

#[cfg(windows)]
fn successful_command() -> &'static str {
    "cmd.exe"
}
