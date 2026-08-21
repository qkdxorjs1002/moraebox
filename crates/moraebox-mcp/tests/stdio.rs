use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
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

#[cfg(unix)]
#[test]
fn large_waiting_exec_is_bounded_and_continues_with_sandbox_io() {
    const INLINE_BYTES: usize = 1024 * 1024;
    const TOTAL_BYTES: usize = INLINE_BYTES + 3;
    let (mut child, mut stdin, responses, reader) = spawn_server();
    initialize(&mut stdin, &responses);
    write_request(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"sandbox_exec","arguments":{
                "argv":["/bin/sh","-c","yes x | tr -d '\\n' | head -c 1048579"]
            }}
        }),
    );
    let first = read_response(&responses);
    assert_eq!(
        first.pointer("/result/structuredContent/has_more"),
        Some(&json!(true))
    );
    assert_eq!(
        first.pointer("/result/structuredContent/continuation_cursor"),
        Some(&json!(INLINE_BYTES))
    );
    assert_eq!(
        first.pointer("/result/structuredContent/output_next_cursor"),
        Some(&json!(TOTAL_BYTES))
    );
    let inline_bytes = first
        .pointer("/result/structuredContent/output")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|chunk| chunk.get("text").and_then(Value::as_str).unwrap().len())
        .sum::<usize>();
    assert_eq!(inline_bytes, INLINE_BYTES);
    let content_text = first
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap();
    assert!(content_text.len() < 512);
    assert!(!content_text.contains("xxxxxxxx"));
    let session_id = first
        .pointer("/result/structuredContent/status/session_id")
        .and_then(Value::as_str)
        .unwrap()
        .to_owned();

    write_request(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"sandbox_io","arguments":{
                "session_id":session_id,"cursor":INLINE_BYTES,"max_bytes":1024
            }}
        }),
    );
    let continuation = read_response(&responses);
    assert_eq!(
        continuation.pointer("/result/structuredContent/next_cursor"),
        Some(&json!(TOTAL_BYTES))
    );
    let remaining_bytes = continuation
        .pointer("/result/structuredContent/output")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|chunk| chunk.get("text").and_then(Value::as_str).unwrap().len())
        .sum::<usize>();
    assert_eq!(remaining_bytes, 3);

    write_request(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"sandbox_remove","arguments":{"session_id":session_id}}
        }),
    );
    assert_eq!(
        read_response(&responses).pointer("/result/structuredContent/removed"),
        Some(&json!(true))
    );
    drop(stdin);
    assert!(wait_for_child(&mut child).success());
    reader.join().unwrap();
    assert!(responses.try_iter().next().is_none());
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

#[cfg(unix)]
#[test]
fn cancelled_waiting_exec_is_cleaned_before_response() {
    let temporary = tempfile::tempdir().unwrap();
    let pid_path = temporary.path().join("cancelled.pid");
    let (mut child, mut stdin, responses, reader) = spawn_server();

    initialize(&mut stdin, &responses);
    write_request(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"sandbox_exec","arguments":{
                "argv":pid_command(&pid_path),"wait":true
            }}
        }),
    );
    let pid = wait_for_pid(&pid_path);
    assert!(process_is_alive(pid));

    write_request(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0","method":"notifications/cancelled",
            "params":{"requestId":2,"reason":"integration test"}
        }),
    );
    let cancelled = read_response(&responses);
    assert_eq!(cancelled.get("id"), Some(&json!(2)));
    assert_eq!(cancelled.pointer("/error/code"), Some(&json!(-32800)));
    assert!(
        !process_is_alive(pid),
        "request-owned process {pid} outlived its cancellation response"
    );

    write_request(&mut stdin, &json!({"jsonrpc":"2.0","id":3,"method":"ping"}));
    assert_eq!(read_response(&responses).get("id"), Some(&json!(3)));

    drop(stdin);
    assert!(wait_for_child(&mut child).success());
    reader.join().unwrap();
    assert!(responses.try_iter().next().is_none());
}

#[cfg(unix)]
#[test]
fn async_session_survives_request_completion_and_is_cleaned_on_eof() {
    let temporary = tempfile::tempdir().unwrap();
    let pid_path = temporary.path().join("connection-owned.pid");
    let (mut child, mut stdin, responses, reader) = spawn_server();

    initialize(&mut stdin, &responses);
    write_request(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"sandbox_exec","arguments":{
                "argv":pid_command(&pid_path),"wait":false
            }}
        }),
    );
    let started = read_response(&responses);
    assert_eq!(started.get("id"), Some(&json!(2)));
    let pid = wait_for_pid(&pid_path);
    assert!(process_is_alive(pid));

    write_request(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0","method":"notifications/cancelled",
            "params":{"requestId":2,"reason":"request already completed"}
        }),
    );
    thread::sleep(Duration::from_millis(100));
    assert!(
        process_is_alive(pid),
        "completed wait=false request lost its connection-owned session"
    );

    drop(stdin);
    assert!(wait_for_child(&mut child).success());
    assert!(
        !process_is_alive(pid),
        "connection-owned process {pid} outlived client EOF"
    );
    reader.join().unwrap();
    assert!(responses.try_iter().next().is_none());
}

#[test]
fn explicit_remove_stops_and_forgets_a_session_idempotently() {
    let (mut child, mut stdin, responses, reader) = spawn_server();
    initialize(&mut stdin, &responses);
    write_request(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"sandbox_exec","arguments":{
                "argv":long_running_command(),"wait":false
            }}
        }),
    );
    let started = read_response(&responses);
    let session_id = started
        .pointer("/result/structuredContent/status/session_id")
        .and_then(Value::as_str)
        .unwrap()
        .to_owned();

    write_request(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"sandbox_remove","arguments":{"session_id":session_id}}
        }),
    );
    let removed = read_response(&responses);
    assert_eq!(
        removed.pointer("/result/structuredContent/removed"),
        Some(&json!(true))
    );
    assert_eq!(
        removed.pointer("/result/structuredContent/status/state"),
        Some(&json!("dead"))
    );

    write_request(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"sandbox_remove","arguments":{"session_id":session_id}}
        }),
    );
    assert_eq!(
        read_response(&responses).pointer("/result/structuredContent/removed"),
        Some(&json!(false))
    );
    write_request(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0","id":5,"method":"tools/call",
            "params":{"name":"sandbox_io","arguments":{"session_id":session_id}}
        }),
    );
    assert_eq!(
        read_response(&responses).pointer("/result/isError"),
        Some(&json!(true))
    );

    drop(stdin);
    assert!(wait_for_child(&mut child).success());
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

fn spawn_server() -> (
    Child,
    ChildStdin,
    mpsc::Receiver<String>,
    thread::JoinHandle<()>,
) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_morae-mcp"))
        .args(["--backend", "process"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (lines, responses) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            lines.send(line.unwrap()).unwrap();
        }
    });
    (child, stdin, responses, reader)
}

fn initialize(stdin: &mut impl Write, responses: &mpsc::Receiver<String>) {
    write_request(
        stdin,
        &json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-11-25"}
        }),
    );
    assert_eq!(read_response(responses).get("id"), Some(&json!(1)));
}

fn wait_for_child(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("MCP server did not exit after client EOF");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn pid_command(pid_path: &std::path::Path) -> Vec<String> {
    vec![
        "/bin/sh".into(),
        "-c".into(),
        "printf '%s' $$ > \"$1\"; exec /bin/sleep 30".into(),
        "moraebox-pid-writer".into(),
        pid_path.to_string_lossy().into_owned(),
    ]
}

#[cfg(unix)]
fn wait_for_pid(pid_path: &std::path::Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(contents) = std::fs::read_to_string(pid_path)
            && let Ok(pid) = contents.parse()
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "process PID was not published");
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
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
