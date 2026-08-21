#![forbid(unsafe_code)]

use std::{fs, path::Path, process::Command};

#[test]
fn workspace_rejects_nested_cache_before_image_preparation() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let cache = source.join(".moraebox/cache");
    let state = root.path().join("state");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file"), b"content").unwrap();

    let output = run(&source, &cache, &state);

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("workspace source and cache must not overlap")
    );
    assert!(!cache.exists());
    assert!(!state.exists());
}

#[test]
fn workspace_rejects_nested_state_before_image_preparation() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let cache = root.path().join("cache");
    let state = source.join(".moraebox/state");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file"), b"content").unwrap();

    let output = run(&source, &cache, &state);

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("workspace source and managed path must not overlap")
    );
    assert!(!cache.exists());
    assert!(!state.exists());
}

fn run(source: &Path, cache: &Path, state: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_morae"))
        .args(["run", "--backend", "libkrun", "--workspace"])
        .arg(source)
        .arg("--cache-dir")
        .arg(cache)
        .arg("--state-dir")
        .arg(state)
        .args(["--", "/bin/true"])
        .output()
        .unwrap()
}
