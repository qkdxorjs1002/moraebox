#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use std::{
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use nix::{
    errno::Errno,
    fcntl::{FcntlArg, OFlag, fcntl},
    pty::{Winsize, openpty},
    sys::signal::{self, Signal},
    unistd::Pid,
};
use serde::Deserialize;
use serde_json::Value;

const E2E_ENABLE_ENV: &str = "MORAE_NATIVE_E2E";
const E2E_CACHE_ENV: &str = "MORAE_NATIVE_E2E_CACHE_DIR";
const E2E_IMAGE_ENV: &str = "MORAE_NATIVE_E2E_IMAGE";
const E2E_HELPER_ENV: &str = "MORAE_NATIVE_E2E_HELPER";
const CHILD_START_TIMEOUT: Duration = Duration::from_secs(30);
const CHILD_EXIT_TIMEOUT: Duration = Duration::from_secs(10);
const RUN_EXIT_TIMEOUT: Duration = Duration::from_secs(60);

const NETWORK_OFF_PROBE: &str = r#"
import os, socket, time
host = os.environ["MORAE_EGRESS_HOST"]
udp_dns = os.environ["MORAE_EGRESS_UDP_DNS"]
socket.setdefaulttimeout(3)

def dns_query(name):
    labels = b"".join(bytes([len(part)]) + part.encode() for part in name.split(".")) + b"\0"
    return b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00" + labels + b"\x00\x01\x00\x01"

def dns():
    socket.getaddrinfo(host, 443, socket.AF_INET, socket.SOCK_STREAM)

def tcp():
    connection = socket.create_connection(("1.1.1.1", 443), 3)
    connection.close()

def udp():
    connection = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    connection.settimeout(3)
    connection.sendto(dns_query(host), (udp_dns, 53))
    connection.recvfrom(512)

def must_be_blocked(name, operation):
    try:
        operation()
    except OSError:
        return
    raise RuntimeError(name + " unexpectedly reached the network")

must_be_blocked("DNS", dns)
must_be_blocked("TCP", tcp)
must_be_blocked("UDP", udp)
print("network-off-blocked")
time.sleep(1)
"#;

const NETWORK_ON_PROBE: &str = r#"
import os, socket, time
host = os.environ["MORAE_EGRESS_HOST"]
udp_dns = os.environ["MORAE_EGRESS_UDP_DNS"]
socket.setdefaulttimeout(5)

def dns_query(name):
    labels = b"".join(bytes([len(part)]) + part.encode() for part in name.split(".")) + b"\0"
    return b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00" + labels + b"\x00\x01\x00\x01"

addresses = socket.getaddrinfo(host, 443, socket.AF_INET, socket.SOCK_STREAM)
if not addresses:
    raise RuntimeError("DNS returned no IPv4 address")
tcp = socket.create_connection((host, 443), 5)
tcp.close()
udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
udp.settimeout(5)
udp.sendto(dns_query(host), (udp_dns, 53))
response, _ = udp.recvfrom(512)
if response[:2] != b"\x12\x34":
    raise RuntimeError("UDP DNS response ID mismatch")
print("network-on-allowed")
time.sleep(1)
"#;

const SLEEP_PROBE: &str = "import time; time.sleep(60)";
const PROTOCOL_IO_PROBE: &str = r#"
import sys
data = sys.stdin.buffer.read()
sys.stdout.write("protocol-stdout:" + data.decode())
sys.stderr.write("protocol-stderr\n")
sys.exit(23)
"#;
const TTY_RESIZE_PROBE: &str = r#"
import os, signal, sys

def report_size(*_):
    size = os.get_terminal_size(sys.stdin.fileno())
    print(f"tty-size:{size.lines}x{size.columns}", flush=True)
    if size.lines == 41 and size.columns == 99:
        raise SystemExit(0)

signal.signal(signal.SIGWINCH, report_size)
report_size()
signal.pause()
"#;
const TTY_EOF_PROBE: &str = r#"
import sys
data = sys.stdin.buffer.read().decode()
print("tty-eof-result:" + data, end="")
"#;

#[derive(Debug, Deserialize)]
struct CachedImage {
    reference: Option<String>,
    rootfs: PathBuf,
    ready: bool,
    default: bool,
}

#[derive(Debug)]
struct NativeHarness {
    morae: PathBuf,
    helper: PathBuf,
    image: String,
    cache_dir: PathBuf,
    state: tempfile::TempDir,
    egress_host: String,
    udp_dns: String,
}

impl NativeHarness {
    fn new() -> Self {
        assert_eq!(
            env::var(E2E_ENABLE_ENV).as_deref(),
            Ok("1"),
            "native E2E is opt-in; run scripts/native-egress-e2e.sh"
        );
        let morae = PathBuf::from(env!("CARGO_BIN_EXE_morae"));
        let helper = env::var_os(E2E_HELPER_ENV).map_or_else(
            || morae.parent().unwrap().join("morae-vmm-helper"),
            PathBuf::from,
        );
        assert!(
            helper.is_file(),
            "signed helper is missing at {}; run cargo build --workspace and codesign it",
            helper.display()
        );

        let configured_cache = env::var_os(E2E_CACHE_ENV).map(PathBuf::from);
        let configured_image = env::var(E2E_IMAGE_ENV).ok();
        verify_doctor(&morae, configured_cache.as_deref());
        let cached = ready_image(
            &morae,
            configured_cache.as_deref(),
            configured_image.as_deref(),
        );
        assert!(
            cached.rootfs.is_dir(),
            "native E2E rootfs is missing at {}",
            cached.rootfs.display()
        );
        let cache_dir = configured_cache.unwrap_or_else(|| cache_root_for(&cached.rootfs));
        let image = configured_image
            .or(cached.reference)
            .expect("ready native E2E image has no registry reference");
        let state = tempfile::Builder::new()
            .prefix("morae-native-e2e-")
            .tempdir_in("/private/tmp")
            .expect("create short native E2E state directory");
        let harness = Self {
            morae,
            helper,
            image,
            cache_dir,
            state,
            egress_host: env::var("MORAE_EGRESS_HOST").unwrap_or_else(|_| "example.com".into()),
            udp_dns: env::var("MORAE_EGRESS_UDP_DNS").unwrap_or_else(|_| "1.1.1.1".into()),
        };
        harness.assert_network_state_empty();
        harness
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.morae);
        command
            .arg("run")
            .arg("--helper")
            .arg(&self.helper)
            .arg("--image")
            .arg(&self.image)
            .arg("--cache-dir")
            .arg(&self.cache_dir)
            .arg("--state-dir")
            .arg(self.state.path().join("state"))
            .arg("--env")
            .arg(format!("MORAE_EGRESS_HOST={}", self.egress_host))
            .arg("--env")
            .arg(format!("MORAE_EGRESS_UDP_DNS={}", self.udp_dns))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn assert_network_state_empty(&self) {
        assert_directory_empty(&self.cache_dir.join("network"));
    }

    fn assert_control_state_empty(&self) {
        assert_directory_empty(&self.cache_dir.join("control"));
    }

    fn spawn_json(&self, network: bool, timeout: &str, script: &str) -> Child {
        let mut command = self.command();
        command.arg("--timeout").arg(timeout).arg("--json");
        if network {
            command.arg("--network");
        }
        command
            .arg("--")
            .arg("python3")
            .arg("-c")
            .arg(python_bootstrap(script))
            .spawn()
            .expect("spawn morae native run")
    }

    fn spawn_interactive(&self, script: &str) -> Child {
        let mut command = self.command();
        command
            .arg("--timeout")
            .arg("30s")
            .arg("--network")
            .arg("--interactive")
            .arg("--")
            .arg("python3")
            .arg("-c")
            .arg(python_bootstrap(script))
            .stdin(Stdio::piped());
        command.spawn().expect("spawn interactive morae native run")
    }
}

#[derive(Debug, Clone)]
struct NativeChildren {
    helper: i32,
    gvproxy: Option<i32>,
}

fn verify_doctor(morae: &Path, cache_dir: Option<&Path>) {
    let mut command = Command::new(morae);
    if let Some(cache_dir) = cache_dir {
        command.arg("--cache-dir").arg(cache_dir);
    }
    let output = command
        .args(["doctor", "--json", "--strict"])
        .output()
        .expect("run morae doctor");
    assert!(
        output.status.success(),
        "native doctor failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("parse doctor JSON");
    assert_eq!(report["native_backend_ready"], true);
    assert_eq!(report["native_network_ready"], true);
    assert_eq!(report["hypervisor_entitlement"], true);
    assert_eq!(report["cache_volume"]["reflink_supported"], true);
    assert_eq!(report["cache_volume"]["free_space_sufficient"], true);
    assert_eq!(report["network"]["socket_created"], true);
    assert_eq!(report["helper"]["code_signature_valid"], true);
    assert_eq!(report["libkrun"]["version_matches"], true);
    assert_eq!(report["libkrunfw"]["version_matches"], true);
    let checks = report["checks"].as_array().expect("doctor checks");
    for id in [
        "cache_reflink",
        "cache_free_space",
        "network_helper",
        "network_socket",
        "helper_signing",
        "libkrun_abi",
        "libkrunfw_abi",
        "network_abi",
        "disk_tools",
    ] {
        let check = checks
            .iter()
            .find(|check| check["id"] == id)
            .unwrap_or_else(|| panic!("missing doctor check {id}"));
        assert_eq!(check["status"], "pass", "doctor check {id} failed");
        assert_eq!(check["remediation"], Value::Null);
    }
}

fn ready_image(morae: &Path, cache_dir: Option<&Path>, image: Option<&str>) -> CachedImage {
    let mut command = Command::new(morae);
    command.args(["image", "list", "--json"]);
    if let Some(cache_dir) = cache_dir {
        command.arg("--cache-dir").arg(cache_dir);
    }
    let output = command.output().expect("list cached images");
    assert!(
        output.status.success(),
        "native E2E requires a ready cached image: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let images: Vec<CachedImage> =
        serde_json::from_slice(&output.stdout).expect("parse image list JSON");
    images
        .into_iter()
        .find(|candidate| {
            candidate.ready
                && image.map_or(candidate.default, |expected| {
                    candidate.reference.as_deref() == Some(expected)
                })
        })
        .expect("native E2E requires a ready default image or MORAE_NATIVE_E2E_IMAGE")
}

fn python_bootstrap(script: &str) -> String {
    format!(
        "exec(__import__(\"base64\").b64decode(\"{}\"))",
        BASE64.encode(script)
    )
}

fn cache_root_for(rootfs: &Path) -> PathBuf {
    rootfs
        .ancestors()
        .nth(3)
        .filter(|cache| cache.join("rootfs").is_dir())
        .expect("cached rootfs must be under <cache>/rootfs/sha256/<digest>")
        .to_path_buf()
}

fn wait_for_native_children(child: &mut Child, expect_gvproxy: bool) -> NativeChildren {
    let deadline = Instant::now() + CHILD_START_TIMEOUT;
    loop {
        assert!(
            child.try_wait().expect("poll morae process").is_none(),
            "morae exited before native children became ready"
        );
        let children = direct_children(child.id());
        let helper = children
            .iter()
            .find(|(_, command)| command.contains("morae-vmm-helper"))
            .map(|(pid, _)| *pid);
        let gvproxy = children
            .iter()
            .find(|(_, command)| command.contains("gvproxy") && command.contains("--listen-vfkit"))
            .map(|(pid, _)| *pid);
        let helper = helper.filter(|_| !expect_gvproxy || gvproxy.is_some());
        if let Some(helper) = helper {
            return NativeChildren { helper, gvproxy };
        }
        assert!(
            Instant::now() < deadline,
            "native helper/gvproxy did not start within {CHILD_START_TIMEOUT:?}: {children:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn direct_children(parent: u32) -> Vec<(i32, String)> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()
        .expect("inspect native child processes");
    assert!(output.status.success(), "ps failed");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<i32>().ok()?;
            let parent_pid = fields.next()?.parse::<u32>().ok()?;
            (parent_pid == parent).then(|| (pid, fields.collect::<Vec<_>>().join(" ")))
        })
        .collect()
}

fn wait_for_output(mut child: Child) -> Output {
    let deadline = Instant::now() + RUN_EXIT_TIMEOUT;
    loop {
        if child.try_wait().expect("poll morae process").is_some() {
            return child.wait_with_output().expect("collect morae output");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("reap timed out morae process");
            panic!(
                "morae did not exit within {RUN_EXIT_TIMEOUT:?}: stderr={}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn parse_report(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "parse morae JSON report: {error}; stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn assert_dead_report(report: &Value) {
    assert_eq!(report["backend"], "libkrun");
    assert_eq!(report["state"], "dead");
    assert!(
        report["trace"].as_array().is_some_and(|trace| trace
            .iter()
            .any(|event| event["kind"] == "cleanup_complete")),
        "native report has no cleanup_complete event: {report}"
    );
}

fn report_output_text(report: &Value) -> String {
    report_channel_text(report, None)
}

fn report_channel_text(report: &Value, channel: Option<&str>) -> String {
    let bytes = report["output"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|chunk| channel.is_none_or(|expected| chunk["channel"] == expected))
        .filter_map(|chunk| chunk["data"].as_array())
        .flatten()
        .filter_map(Value::as_u64)
        .filter_map(|byte| u8::try_from(byte).ok())
        .collect::<Vec<_>>();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn assert_protocol_io(harness: &NativeHarness) {
    let mut command = harness.command();
    command
        .args(["--timeout", "20s", "--json", "--", "python3", "-c"])
        .arg(python_bootstrap(PROTOCOL_IO_PROBE))
        .stdin(Stdio::piped());
    let mut child = command.spawn().expect("spawn protocol I/O probe");
    child
        .stdin
        .take()
        .expect("protocol I/O stdin")
        .write_all(b"stdin-through-vsock\n")
        .expect("write protocol I/O stdin");
    let children = wait_for_native_children(&mut child, false);
    let output = wait_for_output(child);
    let report = parse_report(&output);
    assert_eq!(
        output.status.code(),
        Some(23),
        "protocol I/O probe failed: report={report}; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_dead_report(&report);
    assert_eq!(report["exit_code"], 23);
    assert_eq!(report["timed_out"], false);
    assert_eq!(
        report_channel_text(&report, Some("stdout")),
        "protocol-stdout:stdin-through-vsock\n"
    );
    assert_eq!(
        report_channel_text(&report, Some("stderr")),
        "protocol-stderr\n"
    );
    assert_children_gone(&children);
    harness.assert_network_state_empty();
}

fn read_pty_until(reader: &mut fs::File, output: &mut Vec<u8>, marker: &str) {
    let deadline = Instant::now() + RUN_EXIT_TIMEOUT;
    let mut buffer = [0_u8; 4096];
    while !String::from_utf8_lossy(output).contains(marker) {
        match reader.read(&mut buffer) {
            Ok(0) => panic!("PTY closed before output contained {marker:?}"),
            Ok(count) => output.extend_from_slice(&buffer[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "PTY output did not contain {marker:?}: {}",
                    String::from_utf8_lossy(output)
                );
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("read PTY output while waiting for {marker:?}: {error}"),
        }
    }
}

fn assert_tty_resize(harness: &NativeHarness) {
    let initial = Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = openpty(Some(&initial), None).expect("open host PTY");
    fcntl(&pty.master, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).expect("make PTY nonblocking");
    let mut master = fs::File::from(pty.master);
    let slave = fs::File::from(pty.slave);
    let mut command = harness.command();
    command
        .args([
            "--timeout",
            "30s",
            "--interactive",
            "--tty",
            "--",
            "python3",
            "-c",
        ])
        .arg(python_bootstrap(TTY_RESIZE_PROBE))
        .stdin(Stdio::from(
            slave.try_clone().expect("clone PTY slave stdin"),
        ))
        .stdout(Stdio::from(
            slave.try_clone().expect("clone PTY slave stdout"),
        ))
        .stderr(Stdio::from(slave));
    let mut child = command.spawn().expect("spawn TTY resize probe");
    let children = wait_for_native_children(&mut child, false);

    let mut pty_output = Vec::new();
    read_pty_until(&mut master, &mut pty_output, "tty-size:24x80");

    rustix::termios::tcsetwinsize(
        &master,
        rustix::termios::Winsize {
            ws_row: 41,
            ws_col: 99,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .expect("resize host PTY");
    signal::kill(
        Pid::from_raw(i32::try_from(child.id()).expect("morae PID fits i32")),
        Signal::SIGWINCH,
    )
    .expect("signal host terminal resize");
    read_pty_until(&mut master, &mut pty_output, "tty-size:41x99");

    let output = wait_for_output(child);
    let mut final_buffer = [0_u8; 4096];
    while let Ok(count) = master.read(&mut final_buffer) {
        if count == 0 {
            break;
        }
        pty_output.extend_from_slice(&final_buffer[..count]);
    }
    drop(master);
    assert!(
        output.status.success(),
        "TTY resize probe failed with status {}: PTY={} stderr={}",
        output.status,
        String::from_utf8_lossy(&pty_output),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_children_gone(&children);
    harness.assert_network_state_empty();
    harness.assert_control_state_empty();
}

fn assert_tty_eof(harness: &NativeHarness) {
    let mut command = harness.command();
    command
        .args([
            "--timeout",
            "10s",
            "--interactive",
            "--tty",
            "--",
            "python3",
            "-c",
        ])
        .arg(python_bootstrap(TTY_EOF_PROBE))
        .stdin(Stdio::piped());
    let mut child = command.spawn().expect("spawn TTY EOF probe");
    let children = wait_for_native_children(&mut child, false);
    let mut stdin = child.stdin.take().expect("TTY EOF stdin pipe");
    stdin
        .write_all(b"tty-eof-input\n")
        .expect("write TTY EOF input");
    drop(stdin);

    let output = wait_for_output(child);
    assert!(
        output.status.success(),
        "TTY EOF probe failed with status {}: stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("tty-eof-result:tty-eof-input"));
    assert_eq!(
        text.matches("tty-eof-input").count(),
        2,
        "TTY input should have one guest echo and one workload result: {text:?}"
    );
    assert_children_gone(&children);
    harness.assert_network_state_empty();
    harness.assert_control_state_empty();
}

fn assert_children_gone(children: &NativeChildren) {
    let mut pids = vec![children.helper];
    pids.extend(children.gvproxy);
    for pid in pids {
        let deadline = Instant::now() + CHILD_EXIT_TIMEOUT;
        while process_exists(pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!process_exists(pid), "native child PID {pid} remains alive");
    }
}

fn process_exists(pid: i32) -> bool {
    match signal::kill(Pid::from_raw(pid), None) {
        Ok(()) | Err(Errno::EPERM) => true,
        Err(Errno::ESRCH) => false,
        Err(error) => panic!("inspect PID {pid}: {error}"),
    }
}

fn assert_directory_empty(path: &Path) {
    if !path.exists() {
        return;
    }
    let entries = fs::read_dir(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|error| panic!("read {} entries: {error}", path.display()));
    assert!(entries.is_empty(), "{} is not empty", path.display());
}

fn assert_successful_probe(harness: &NativeHarness, network: bool, marker: &str, script: &str) {
    let mut child = harness.spawn_json(network, "20s", script);
    let children = wait_for_native_children(&mut child, network);
    if network {
        assert!(
            children.gvproxy.is_some(),
            "network-on did not start gvproxy"
        );
    } else {
        assert!(children.gvproxy.is_none(), "network-off started gvproxy");
    }
    let output = wait_for_output(child);
    assert!(
        output.status.success(),
        "native probe failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report = parse_report(&output);
    assert_dead_report(&report);
    assert_eq!(report["exit_code"], 0);
    assert_eq!(report["timed_out"], false);
    let text = report_output_text(&report);
    assert!(
        text.contains(marker),
        "native report output does not contain {marker:?}: {text:?}"
    );
    assert_children_gone(&children);
    harness.assert_network_state_empty();
}

fn assert_timeout_cleanup(harness: &NativeHarness) {
    let mut child = harness.spawn_json(true, "1s", SLEEP_PROBE);
    let children = wait_for_native_children(&mut child, true);
    let output = wait_for_output(child);
    assert_eq!(output.status.code(), Some(124));
    let report = parse_report(&output);
    assert_dead_report(&report);
    assert_eq!(report["timed_out"], true);
    assert_children_gone(&children);
    harness.assert_network_state_empty();
}

fn assert_cancellation_cleanup(harness: &NativeHarness) {
    let mut child = harness.spawn_interactive(SLEEP_PROBE);
    let children = wait_for_native_children(&mut child, true);
    signal::kill(
        Pid::from_raw(i32::try_from(child.id()).expect("morae PID fits i32")),
        Signal::SIGTERM,
    )
    .expect("signal morae cancellation");
    let output = wait_for_output(child);
    assert!(
        !output.status.success(),
        "cancelled native run unexpectedly succeeded"
    );
    assert_children_gone(&children);
    harness.assert_network_state_empty();
}

fn assert_helper_crash_cleanup(harness: &NativeHarness) {
    let mut child = harness.spawn_json(true, "30s", SLEEP_PROBE);
    let children = wait_for_native_children(&mut child, true);
    signal::kill(Pid::from_raw(children.helper), Signal::SIGKILL).expect("kill native helper");
    let output = wait_for_output(child);
    assert!(
        !output.status.success(),
        "helper crash unexpectedly produced a successful native run"
    );
    assert_children_gone(&children);
    harness.assert_network_state_empty();
}

#[test]
#[ignore = "requires signed Apple Silicon libkrun, gvproxy, and a ready cached image"]
fn signed_native_egress_gate() {
    let harness = NativeHarness::new();
    assert_protocol_io(&harness);
    assert_tty_eof(&harness);
    assert_tty_resize(&harness);
    assert_successful_probe(&harness, false, "network-off-blocked", NETWORK_OFF_PROBE);
    assert_successful_probe(&harness, true, "network-on-allowed", NETWORK_ON_PROBE);
    assert_timeout_cleanup(&harness);
    assert_cancellation_cleanup(&harness);
    assert_helper_crash_cleanup(&harness);
}
