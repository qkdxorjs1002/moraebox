use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=guest-agent/go.mod");
    println!("cargo:rerun-if-changed=guest-agent/main_linux.go");
    println!("cargo:rerun-if-changed=guest-agent/copy_linux.go");
    println!("cargo:rerun-if-changed=guest-agent/process_linux.go");
    println!("cargo:rerun-if-changed=guest-agent/protocol.go");
    println!("cargo:rerun-if-changed=guest-agent/workspace_linux.go");
    println!("cargo:rerun-if-env-changed=MORAE_GO");

    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo supplies OUT_DIR"))
        .join("morae-guest-agent");
    let go_cache = output.with_extension("go-cache");
    let go = env::var_os("MORAE_GO").unwrap_or_else(|| "go".into());
    let version = env::var("CARGO_PKG_VERSION").expect("Cargo supplies package version");
    let status = Command::new(go)
        .current_dir("guest-agent")
        .env("CGO_ENABLED", "0")
        .env("GOOS", "linux")
        .env("GOARCH", "arm64")
        .env("GOCACHE", go_cache)
        .env("GOTOOLCHAIN", "local")
        .args(["build", "-mod=readonly", "-trimpath", "-buildvcs=false"])
        .arg(format!(
            "-ldflags=-s -w -buildid= -X main.agentVersion={version}"
        ))
        .arg("-o")
        .arg(&output)
        .arg(".")
        .status()
        .expect("failed to launch Go for the Linux guest agent");
    assert!(status.success(), "failed to build the Linux guest agent");
    println!(
        "cargo:rustc-env=MORAE_GUEST_AGENT_PATH={}",
        output.display()
    );
}
