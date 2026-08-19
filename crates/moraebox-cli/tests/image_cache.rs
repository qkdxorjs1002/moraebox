#![forbid(unsafe_code)]

use std::{fs, path::Path, process::Command};

use serde_json::Value;
use tempfile::TempDir;

const PYTHON_DEFAULT: &str = "docker.io/library/python:3.12";

#[test]
fn default_image_can_be_shown_set_and_reset() {
    let cache = TempDir::new().unwrap();

    let shown = run_json(cache.path(), ["image", "default"]);
    assert_eq!(shown["reference"], PYTHON_DEFAULT);

    let set = run_json(cache.path(), ["image", "default", "debian:bookworm"]);
    assert_eq!(set["reference"], "docker.io/library/debian:bookworm");

    let reset = run_json(cache.path(), ["image", "default", "--unset"]);
    assert_eq!(reset["reference"], PYTHON_DEFAULT);
}

#[test]
fn list_and_remove_aliases_manage_a_legacy_rootfs() {
    let cache = TempDir::new().unwrap();
    let digest_hex = "1".repeat(64);
    let rootfs = cache.path().join("rootfs").join("sha256").join(&digest_hex);
    fs::create_dir_all(&rootfs).unwrap();
    fs::write(rootfs.join(".fastmvm-rootfs-complete"), b"ready").unwrap();
    fs::write(rootfs.join("payload"), b"cached").unwrap();

    let images = run_json(cache.path(), ["image", "ls"]);
    let entries = images.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0]["manifest_digest"],
        format!("sha256:{digest_hex}")
    );
    assert_eq!(entries[0]["reference"], Value::Null);

    let target = format!("sha256:{digest_hex}");
    let preview = run_json_with(
        cache.path(),
        ["image", "rm"],
        [target.as_str(), "--dry-run"],
    );
    assert_eq!(preview["applied"], false);
    assert!(rootfs.exists());

    let removed = run_json_with(cache.path(), ["image", "rm"], [target.as_str()]);
    assert_eq!(removed["applied"], true);
    assert!(!rootfs.exists());
}

#[test]
fn cache_commands_report_usage_and_require_destructive_confirmation() {
    let cache = TempDir::new().unwrap();

    let usage = run_json(cache.path(), ["cache", "info"]);
    assert_eq!(usage["total_bytes"], 0);

    let mut prune = base_command(cache.path(), ["cache", "prune"]);
    let output = prune.output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires --dry-run"));

    let preview = run_json(cache.path(), ["cache", "prune", "--dry-run"]);
    assert_eq!(preview["applied"], false);

    let mut clean_without_all = base_command(cache.path(), ["cache", "clean", "--dry-run"]);
    let output = clean_without_all.output().unwrap();
    assert_eq!(output.status.code(), Some(2));

    let clean_preview = run_json(cache.path(), ["cache", "clean", "--all", "--dry-run"]);
    assert_eq!(clean_preview["applied"], false);
}

fn run_json<const N: usize>(cache: &Path, args: [&str; N]) -> Value {
    run_json_with(cache, args, std::iter::empty::<&str>())
}

fn run_json_with<I, S, J, T>(cache: &Path, args: I, extra: J) -> Value
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
    J: IntoIterator<Item = T>,
    T: AsRef<std::ffi::OsStr>,
{
    let mut command = base_command(cache, args);
    command.args(extra).arg("--json");
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn base_command<I, S>(cache: &Path, args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new(env!("CARGO_BIN_EXE_morae"));
    command.args(args).arg("--cache-dir").arg(cache);
    command
}
