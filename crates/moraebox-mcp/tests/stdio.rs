use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Command, Stdio},
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
