#![forbid(unsafe_code)]

use std::{path::PathBuf, process::ExitCode, str::FromStr, sync::Arc};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use clap::Parser;
use moraebox_core::{OutputChunk, RunSpec, SessionId, Signal, TimeoutPolicy};
use moraebox_runtime::{Backend, LibkrunBackend, LibkrunConfig, ProcessBackend};
use moraebox_sdk::{ExecutionResult, IoRequest, IoResult, SandboxSdk};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};

const PROTOCOL_VERSION: &str = "2026-07-28";

#[derive(Debug, Parser)]
#[command(name = "morae-mcp", about = "stdio MCP server for moraebox")]
struct Args {
    #[arg(long, default_value = "libkrun", value_parser = ["process", "libkrun"])]
    backend: String,
    #[arg(long, env = "MORAE_HELPER_PATH")]
    helper: Option<PathBuf>,
    #[arg(long, env = "MORAE_LIBKRUN_PATH")]
    libkrun: Option<PathBuf>,
    #[arg(long, env = "MORAE_ROOTFS")]
    rootfs: Option<PathBuf>,
    #[arg(long, env = "MORAE_LIB_DIR")]
    lib_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 2)]
    cpus: u8,
    #[arg(long, default_value_t = 512)]
    memory_mib: u32,
}

#[tokio::main]
async fn main() -> ExitCode {
    match create_sdk(Args::parse()) {
        Ok(sdk) => match serve(sdk).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("morae-mcp: {error}");
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            eprintln!("morae-mcp: {error}");
            ExitCode::FAILURE
        }
    }
}

fn create_sdk(args: Args) -> Result<SandboxSdk, String> {
    let backend: Arc<dyn Backend> = match args.backend.as_str() {
        "process" => Arc::new(ProcessBackend),
        "libkrun" => {
            let helper = args
                .helper
                .ok_or("libkrun backend requires --helper or MORAE_HELPER_PATH")?;
            let library = args
                .libkrun
                .ok_or("libkrun backend requires --libkrun or MORAE_LIBKRUN_PATH")?;
            let root = args
                .rootfs
                .ok_or("libkrun backend requires --rootfs or MORAE_ROOTFS")?;
            let mut config = LibkrunConfig::new(helper, library, root);
            config.library_search_path = args.lib_dir;
            config.vcpus = args.cpus;
            config.memory_mib = args.memory_mib;
            Arc::new(LibkrunBackend::new(config))
        }
        _ => return Err("unsupported backend".into()),
    };
    Ok(SandboxSdk::new(backend))
}

async fn serve(sdk: SandboxSdk) -> Result<(), Box<dyn std::error::Error>> {
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut output = BufWriter::new(tokio::io::stdout());
    while let Some(line) = input.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                write_response(
                    &mut output,
                    &protocol_error(Value::Null, -32700, &error.to_string()),
                )
                .await?;
                continue;
            }
        };
        if request.get("id").is_none() {
            continue;
        }
        let response = handle_request(&sdk, request).await;
        write_response(&mut output, &response).await?;
    }
    Ok(())
}

async fn write_response(
    output: &mut BufWriter<tokio::io::Stdout>,
    response: &Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut encoded = serde_json::to_vec(response)?;
    encoded.push(b'\n');
    output.write_all(&encoded).await?;
    output.flush().await?;
    Ok(())
}

async fn handle_request(sdk: &SandboxSdk, request: Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    match method {
        "initialize" => success(
            id,
            json!({
                "protocolVersion": request.pointer("/params/protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSION),
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "moraebox", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "Use sandbox_exec for one-shot or session execution, sandbox_io for cursor I/O, and sandbox_stop for cleanup."
            }),
        ),
        "ping" => success(id, json!({})),
        "tools/list" => success(id, tools_list()),
        "tools/call" => {
            call_tool(sdk, id, request.get("params").cloned().unwrap_or_default()).await
        }
        _ => protocol_error(id, -32601, "method not found"),
    }
}

async fn call_tool(sdk: &SandboxSdk, id: Value, params: Value) -> Value {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return protocol_error(id, -32602, "tool name is required");
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let result = match name {
        "sandbox_exec" => sandbox_exec(sdk, arguments).await,
        "sandbox_io" => sandbox_io(sdk, arguments).await,
        "sandbox_stop" => sandbox_stop(sdk, arguments).await,
        _ => return protocol_error(id, -32602, "unknown tool"),
    };
    match result {
        Ok(value) => success(id, tool_result(value, false)),
        Err(error) => success(id, tool_result(json!({ "error": error }), true)),
    }
}

async fn sandbox_exec(sdk: &SandboxSdk, arguments: Value) -> Result<Value, String> {
    let args: ExecArgs = serde_json::from_value(arguments).map_err(|error| error.to_string())?;
    if args.argv.is_empty() {
        return Err("argv must contain an executable".into());
    }
    let mut spec = RunSpec::command(args.argv);
    spec.tty = args.tty;
    spec.timeout = match (args.unlimited, args.timeout_ms) {
        (true, _) => TimeoutPolicy::Unlimited,
        (false, Some(milliseconds)) if milliseconds > 0 => TimeoutPolicy::Limited(milliseconds),
        (false, Some(_)) => return Err("timeout_ms must be greater than zero".into()),
        (false, None) => TimeoutPolicy::default(),
    };
    if let Some(input) = args.stdin_base64 {
        spec.stdin = STANDARD.decode(input).map_err(|error| error.to_string())?;
    }
    if args.wait {
        let result = sdk.exec(spec).await.map_err(|error| error.to_string())?;
        Ok(execution_json(&result))
    } else {
        let status = sdk.start(spec).await.map_err(|error| error.to_string())?;
        Ok(json!({ "status": status, "next_cursor": 0 }))
    }
}

async fn sandbox_io(sdk: &SandboxSdk, arguments: Value) -> Result<Value, String> {
    let args: IoArgs = serde_json::from_value(arguments).map_err(|error| error.to_string())?;
    let session_id = SessionId::from_str(&args.session_id).map_err(|error| error.to_string())?;
    let stdin = args
        .stdin_base64
        .map(|input| STANDARD.decode(input).map_err(|error| error.to_string()))
        .transpose()?;
    let resize = args.rows.zip(args.columns);
    let signal = args.signal.as_deref().map(parse_signal).transpose()?;
    let result = sdk
        .io(
            session_id,
            IoRequest {
                cursor: args.cursor,
                max_bytes: args.max_bytes,
                stdin,
                close_stdin: args.close_stdin,
                resize,
                signal,
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(io_json(&result))
}

async fn sandbox_stop(sdk: &SandboxSdk, arguments: Value) -> Result<Value, String> {
    let args: StopArgs = serde_json::from_value(arguments).map_err(|error| error.to_string())?;
    let session_id = SessionId::from_str(&args.session_id).map_err(|error| error.to_string())?;
    let status = sdk
        .stop(session_id)
        .await
        .map_err(|error| error.to_string())?;
    Ok(json!({ "status": status }))
}

fn execution_json(result: &ExecutionResult) -> Value {
    json!({
        "status": result.status,
        "output": chunks_json(&result.output),
        "next_cursor": result.next_cursor,
        "truncated": result.truncated
    })
}

fn io_json(result: &IoResult) -> Value {
    json!({
        "status": result.status,
        "output": chunks_json(&result.output),
        "next_cursor": result.next_cursor,
        "truncated": result.truncated
    })
}

fn chunks_json(chunks: &[OutputChunk]) -> Vec<Value> {
    chunks
        .iter()
        .map(|chunk| {
            json!({
                "cursor": chunk.cursor,
                "channel": chunk.channel,
                "data_base64": STANDARD.encode(&chunk.data)
            })
        })
        .collect()
}

fn parse_signal(value: &str) -> Result<Signal, String> {
    match value.to_ascii_uppercase().as_str() {
        "INT" | "SIGINT" => Ok(Signal::Interrupt),
        "TERM" | "SIGTERM" => Ok(Signal::Terminate),
        "KILL" | "SIGKILL" => Ok(Signal::Kill),
        "HUP" | "SIGHUP" => Ok(Signal::Hangup),
        _ => Err(format!("unsupported signal {value}")),
    }
}

fn tool_result(value: Value, is_error: bool) -> Value {
    let result = json!({
        "content": [{ "type": "text", "text": value.to_string() }],
        "structuredContent": value,
        "isError": is_error
    });
    drop(value);
    result
}

fn success(id: Value, result: Value) -> Value {
    let response = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    drop((id, result));
    response
}

fn protocol_error(id: Value, code: i32, message: &str) -> Value {
    let response =
        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } });
    drop(id);
    response
}

fn tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "sandbox_exec",
                "title": "Execute in sandbox",
                "description": "Start a disposable sandbox command. Set wait=false for an interactive/long-running session.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "argv": { "type": "array", "items": { "type": "string" }, "minItems": 1 },
                        "stdin_base64": { "type": "string" },
                        "timeout_ms": { "type": "integer", "minimum": 1 },
                        "unlimited": { "type": "boolean", "default": false },
                        "tty": { "type": "boolean", "default": false },
                        "wait": { "type": "boolean", "default": true }
                    },
                    "required": ["argv"]
                },
                "annotations": { "destructiveHint": false, "openWorldHint": false }
            },
            {
                "name": "sandbox_io",
                "title": "Sandbox session I/O",
                "description": "Write stdin, close it, signal or resize, and read bounded output from a cursor.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": { "type": "string" },
                        "cursor": { "type": "integer", "minimum": 0, "default": 0 },
                        "max_bytes": { "type": "integer", "minimum": 1, "maximum": 8_388_608, "default": 1_048_576 },
                        "stdin_base64": { "type": "string" },
                        "close_stdin": { "type": "boolean", "default": false },
                        "rows": { "type": "integer", "minimum": 1 },
                        "columns": { "type": "integer", "minimum": 1 },
                        "signal": { "type": "string", "enum": ["INT", "TERM", "KILL", "HUP"] }
                    },
                    "required": ["session_id"]
                },
                "annotations": { "destructiveHint": false, "openWorldHint": false }
            },
            {
                "name": "sandbox_stop",
                "title": "Stop sandbox",
                "description": "Terminate a running sandbox with the configured grace period.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "session_id": { "type": "string" } },
                    "required": ["session_id"]
                },
                "annotations": { "destructiveHint": true, "idempotentHint": true, "openWorldHint": false }
            }
        ]
    })
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct ExecArgs {
    argv: Vec<String>,
    stdin_base64: Option<String>,
    timeout_ms: Option<u64>,
    unlimited: bool,
    tty: bool,
    wait: bool,
}

impl Default for ExecArgs {
    fn default() -> Self {
        Self {
            argv: Vec::new(),
            stdin_base64: None,
            timeout_ms: None,
            unlimited: false,
            tty: false,
            wait: true,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct IoArgs {
    session_id: String,
    cursor: u64,
    max_bytes: usize,
    stdin_base64: Option<String>,
    close_stdin: bool,
    rows: Option<u16>,
    columns: Option<u16>,
    signal: Option<String>,
}

impl Default for IoArgs {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            cursor: 0,
            max_bytes: 1024 * 1024,
            stdin_base64: None,
            close_stdin: false,
            rows: None,
            columns: None,
            signal: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct StopArgs {
    session_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn initialize_list_and_call_work() {
        let sdk = SandboxSdk::new(Arc::new(ProcessBackend));
        let initialized = handle_request(
            &sdk,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}),
        )
        .await;
        assert_eq!(
            initialized.pointer("/result/protocolVersion"),
            Some(&json!("2025-11-25"))
        );
        let list =
            handle_request(&sdk, json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).await;
        assert_eq!(
            list.pointer("/result/tools/0/name"),
            Some(&json!("sandbox_exec"))
        );
        let call = handle_request(
            &sdk,
            json!({
                "jsonrpc":"2.0","id":3,"method":"tools/call",
                "params":{"name":"sandbox_exec","arguments":{"argv":["/usr/bin/printf","mcp"]}}
            }),
        )
        .await;
        assert_eq!(call.pointer("/result/isError"), Some(&json!(false)));
        assert_eq!(
            call.pointer("/result/structuredContent/status/exit_code"),
            Some(&json!(0))
        );
    }
}
