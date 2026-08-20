use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

use serde_json::{Value, json};

#[test]
fn stdio_has_one_json_response_per_request() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_morae-mcp"))
        .args(["--backend", "process"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"sandbox_exec","arguments":{"argv":["/usr/bin/printf","stdio"]}}
        }),
    ] {
        writeln!(stdin, "{request}").unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response.get("id"), request.get("id"));
        if request.get("id") == Some(&json!(3)) {
            assert_eq!(
                response.pointer("/result/structuredContent/output/0/text"),
                Some(&json!("stdio"))
            );
            assert!(
                response
                    .pointer("/result/structuredContent/output/0/data_base64")
                    .is_none()
            );
        }
    }
    drop(stdin);
    assert!(child.wait().unwrap().success());
    let mut trailing = String::new();
    stdout.read_to_string(&mut trailing).unwrap();
    assert!(trailing.is_empty(), "unexpected stdout: {trailing}");
}

#[test]
fn controls_remain_responsive_during_a_waiting_exec() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_morae-mcp"))
        .args(["--backend", "process"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (lines, responses) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            lines.send(line.unwrap()).unwrap();
        }
    });

    write_request(
        &mut stdin,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}),
    );
    assert_eq!(read_response(&responses).get("id"), Some(&json!(1)));

    write_request(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"sandbox_exec","arguments":{"argv":long_running_command(),"wait":false}}
        }),
    );
    let started = read_response(&responses);
    let session_id = started
        .pointer("/result/structuredContent/status/session_id")
        .and_then(Value::as_str)
        .unwrap()
        .to_owned();

    for request in [
        json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"sandbox_exec","arguments":{"argv":delayed_command(),"wait":true}}
        }),
        json!({
            "jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"sandbox_io","arguments":{"session_id":session_id,"cursor":0,"max_bytes":1024}}
        }),
        json!({
            "jsonrpc":"2.0","id":5,"method":"tools/call",
            "params":{"name":"sandbox_stop","arguments":{"session_id":session_id}}
        }),
        json!({"jsonrpc":"2.0","id":6,"method":"ping"}),
    ] {
        write_request(&mut stdin, &request);
    }

    let mut response_ids = Vec::new();
    for _ in 0..4 {
        let response = read_response(&responses);
        response_ids.push(response.get("id").and_then(Value::as_u64).unwrap());
        assert!(
            response.get("result").is_some(),
            "unexpected response: {response}"
        );
    }
    let wait_position = response_ids.iter().position(|id| *id == 3).unwrap();
    for control_id in [4, 5, 6] {
        assert!(
            response_ids
                .iter()
                .position(|id| *id == control_id)
                .unwrap()
                < wait_position,
            "response order was {response_ids:?}"
        );
    }

    drop(stdin);
    assert!(child.wait().unwrap().success());
    reader.join().unwrap();
    assert!(responses.try_iter().next().is_none());
}

fn write_request(stdin: &mut impl Write, request: &Value) {
    writeln!(stdin, "{request}").unwrap();
    stdin.flush().unwrap();
}

fn read_response(responses: &mpsc::Receiver<String>) -> Value {
    let line = responses
        .recv_timeout(Duration::from_secs(5))
        .expect("MCP response timed out");
    serde_json::from_str(&line).expect("stdout line must be one JSON response")
}

#[cfg(unix)]
fn long_running_command() -> Vec<String> {
    ["/bin/sh", "-c", "sleep 30"].map(String::from).into()
}

#[cfg(unix)]
fn delayed_command() -> Vec<String> {
    ["/bin/sh", "-c", "sleep 2"].map(String::from).into()
}

#[cfg(windows)]
fn long_running_command() -> Vec<String> {
    vec![
        windows_system_executable("ping.exe"),
        "-n".into(),
        "31".into(),
        "127.0.0.1".into(),
    ]
}

#[cfg(windows)]
fn delayed_command() -> Vec<String> {
    vec![
        windows_system_executable("ping.exe"),
        "-n".into(),
        "3".into(),
        "127.0.0.1".into(),
    ]
}

#[cfg(windows)]
fn windows_system_executable(name: &str) -> String {
    std::path::PathBuf::from(
        std::env::var_os("SystemRoot").expect("Windows must define SystemRoot"),
    )
    .join("System32")
    .join(name)
    .to_string_lossy()
    .into_owned()
}
