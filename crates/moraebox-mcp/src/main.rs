#![forbid(unsafe_code)]

mod registration;

use std::{ffi::OsString, path::PathBuf, process::ExitCode, str::FromStr, sync::Arc};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use clap::{Parser, Subcommand};
use moraebox_core::{OutputChunk, RunSpec, SessionId, Signal, TimeoutPolicy};
use moraebox_image::{Credentials, ImageCache, Platform};
use moraebox_runtime::{
    Backend, LibkrunBackend, LibkrunConfig, NativeRuntimePaths, ProcessBackend,
};
use moraebox_sdk::{ExecutionResult, IoRequest, IoResult, SandboxSdk};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};

const PROTOCOL_VERSION: &str = "2026-07-28";
const SERVER_INSTRUCTIONS: &str = concat!(
    "Use sandbox_exec when a command benefits from a disposable execution environment, ",
    "including untrusted code, dependency installation, isolated experiments, reproducible ",
    "Linux checks, or long-running sessions. Use wait=true for one-shot commands and ",
    "wait=false to start sessions; use sandbox_io for cursor-based I/O and sandbox_stop to ",
    "terminate and clean up sessions. Only the libkrun backend provides VM isolation; the ",
    "process backend is for deterministic development and is not isolated. Host workspace ",
    "files are not attached automatically, so use this server only when required inputs ",
    "already exist in the guest."
);

#[derive(Debug, Parser)]
#[command(name = "morae-mcp", about = "stdio MCP server for moraebox")]
struct Args {
    #[command(subcommand)]
    command: Option<McpCommand>,
    #[command(flatten)]
    server: ServerArgs,
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    /// Register this stdio server with a supported coding agent.
    Install(registration::InstallArgs),
}

#[derive(Debug, clap::Args)]
struct ServerArgs {
    #[arg(long, default_value = "libkrun", value_parser = ["process", "libkrun"])]
    backend: String,
    #[arg(long, env = "MORAE_HELPER_PATH")]
    helper: Option<PathBuf>,
    #[arg(long, env = "MORAE_LIBKRUN_PATH")]
    libkrun: Option<PathBuf>,
    /// Override the automatically discovered gvproxy network helper.
    #[arg(long, env = "MORAE_GVPROXY_PATH")]
    gvproxy: Option<PathBuf>,
    /// Use an already materialized guest root directory instead of a managed image.
    #[arg(long, env = "MORAE_ROOTFS")]
    rootfs: Option<PathBuf>,
    /// OCI image reference. Uses the configured default image when omitted.
    #[arg(long, conflicts_with = "rootfs")]
    image: Option<String>,
    #[arg(long, default_value = ".moraebox/cache")]
    cache_dir: PathBuf,
    #[arg(long, env = "MORAE_REGISTRY_USERNAME", requires = "registry_password")]
    registry_username: Option<String>,
    #[arg(
        long,
        env = "MORAE_REGISTRY_PASSWORD",
        requires = "registry_username",
        hide_env_values = true
    )]
    registry_password: Option<String>,
    #[arg(long, env = "MORAE_LIB_DIR")]
    lib_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 2)]
    cpus: u8,
    #[arg(long, default_value_t = 512)]
    memory_mib: u32,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = parse_args_from(std::env::args_os()).unwrap_or_else(|error| error.exit());
    let Args { command, server } = args;
    if let Some(McpCommand::Install(args)) = command {
        return match registration::install(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("morae-mcp: {error}");
                ExitCode::FAILURE
            }
        };
    }
    match create_sdk(server).await {
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

fn parse_args_from<I, T>(args: I) -> Result<Args, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let raw_args = args.into_iter().map(Into::into).collect::<Vec<_>>();
    let parsed = Args::try_parse_from(raw_args.clone())?;
    if should_show_bare_help(&raw_args, &parsed) {
        let program = raw_args
            .into_iter()
            .next()
            .unwrap_or_else(|| "morae-mcp".into());
        return Args::try_parse_from([program, "--help".into()]);
    }
    Ok(parsed)
}

fn should_show_bare_help(raw_args: &[OsString], parsed: &Args) -> bool {
    raw_args.len() == 1
        && parsed.command.is_none()
        && parsed.server.rootfs.is_none()
        && parsed.server.image.is_none()
}

async fn create_sdk(args: ServerArgs) -> Result<SandboxSdk, Box<dyn std::error::Error>> {
    let backend: Arc<dyn Backend> = match args.backend.as_str() {
        "process" => {
            if args.image.is_some() {
                return Err("--image requires --backend libkrun".into());
            }
            Arc::new(ProcessBackend)
        }
        "libkrun" => {
            let paths = NativeRuntimePaths::discover_with_gvproxy(
                args.helper,
                args.libkrun,
                args.lib_dir,
                args.gvproxy,
            );
            let helper = paths.helper.ok_or(
                "libkrun backend requires --helper, MORAE_HELPER_PATH, or a sibling morae-vmm-helper",
            )?;
            let library = paths.libkrun.ok_or(
                "libkrun backend requires --libkrun, MORAE_LIBKRUN_PATH, or a supported Homebrew libkrun",
            )?;
            let root = if let Some(rootfs) = args.rootfs {
                rootfs
            } else {
                let cache = ImageCache::new(&args.cache_dir);
                let reference = match args.image {
                    Some(reference) => reference,
                    None => cache.default_reference()?,
                };
                cache
                    .resolve_or_pull(
                        &reference,
                        &Platform::host_linux(),
                        args.registry_username
                            .zip(args.registry_password)
                            .map(|(username, password)| Credentials { username, password }),
                    )
                    .await?
                    .rootfs
            };
            let mut config = LibkrunConfig::new(helper, library, root);
            config.library_search_path = paths.library_search_path;
            config.gvproxy_path = paths.gvproxy;
            config.network_runtime_dir = args.cache_dir.join("network");
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
                "instructions": SERVER_INSTRUCTIONS
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
    spec.network = args.network;
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
                "title": "Execute in configured runtime",
                "description": "Start a command in the configured runtime. With the libkrun backend, prefer this for untrusted code, dependency installation, isolated experiments, reproducible Linux checks, or long-running sessions. Network access is disabled by default; set network=true to opt in for one native VM run. Set wait=false to start a session.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "argv": { "type": "array", "items": { "type": "string" }, "minItems": 1 },
                        "stdin_base64": { "type": "string" },
                        "timeout_ms": { "type": "integer", "minimum": 1 },
                        "unlimited": { "type": "boolean", "default": false },
                        "network": { "type": "boolean", "default": false },
                        "tty": { "type": "boolean", "default": false },
                        "wait": { "type": "boolean", "default": true }
                    },
                    "required": ["argv"]
                },
                "annotations": { "destructiveHint": false, "openWorldHint": true }
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
#[allow(clippy::struct_excessive_bools)]
struct ExecArgs {
    argv: Vec<String>,
    stdin_base64: Option<String>,
    timeout_ms: Option<u64>,
    unlimited: bool,
    network: bool,
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
            network: false,
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
    use moraebox_image::Digest;

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
        let instructions = initialized
            .pointer("/result/instructions")
            .and_then(Value::as_str)
            .unwrap();
        for expected in [
            "untrusted code",
            "dependency installation",
            "long-running sessions",
            "Only the libkrun backend provides VM isolation",
            "process backend is for deterministic development and is not isolated",
            "Host workspace files are not attached automatically",
        ] {
            assert!(instructions.contains(expected), "missing {expected:?}");
        }
        let list =
            handle_request(&sdk, json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).await;
        assert_eq!(
            list.pointer("/result/tools/0/name"),
            Some(&json!("sandbox_exec"))
        );
        assert_eq!(
            list.pointer("/result/tools/0/title"),
            Some(&json!("Execute in configured runtime"))
        );
        let exec_description = list
            .pointer("/result/tools/0/description")
            .and_then(Value::as_str)
            .unwrap();
        assert!(exec_description.contains("With the libkrun backend"));
        assert!(exec_description.contains("reproducible Linux checks"));
        assert!(exec_description.contains("Network access is disabled by default"));
        assert_eq!(
            list.pointer("/result/tools/0/inputSchema/properties/network/default"),
            Some(&json!(false))
        );
        assert_eq!(
            list.pointer("/result/tools/0/annotations/openWorldHint"),
            Some(&json!(true))
        );
        let call = handle_request(
            &sdk,
            json!({
                "jsonrpc":"2.0","id":3,"method":"tools/call",
                "params":{"name":"sandbox_exec","arguments":{"argv":successful_command()}}
            }),
        )
        .await;
        assert_eq!(call.pointer("/result/isError"), Some(&json!(false)));
        assert_eq!(
            call.pointer("/result/structuredContent/status/exit_code"),
            Some(&json!(0))
        );
    }

    #[tokio::test]
    async fn process_backend_rejects_vm_network_opt_in() {
        let sdk = SandboxSdk::new(Arc::new(ProcessBackend));
        let call = handle_request(
            &sdk,
            json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{
                    "name":"sandbox_exec",
                    "arguments":{"argv":successful_command(),"network":true}
                }
            }),
        )
        .await;

        assert_eq!(call.pointer("/result/isError"), Some(&json!(true)));
        assert!(
            call.pointer("/result/structuredContent/error")
                .and_then(Value::as_str)
                .is_some_and(|error| error.contains("without VM isolation"))
        );
    }

    #[test]
    fn bare_invocation_shows_help_unless_runtime_is_configured() {
        let error = parse_args_from(["morae-mcp"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);

        let raw_args = [OsString::from("morae-mcp")];
        let mut configured = Args::try_parse_from(["morae-mcp"]).unwrap();
        assert!(should_show_bare_help(&raw_args, &configured));
        configured.server.rootfs = Some("rootfs".into());
        assert!(!should_show_bare_help(&raw_args, &configured));

        let image = parse_args_from(["morae-mcp", "--image", "python:3.12"]).unwrap();
        assert_eq!(image.server.image.as_deref(), Some("python:3.12"));
        assert!(
            parse_args_from(["morae-mcp", "--rootfs", "rootfs", "--image", "python:3.12"]).is_err()
        );

        let process = parse_args_from(["morae-mcp", "--backend", "process"]).unwrap();
        assert_eq!(process.server.backend, "process");
    }

    #[tokio::test]
    async fn cached_image_configures_the_mcp_backend_without_network_access() {
        let cache_dir =
            std::env::temp_dir().join(format!("moraebox-mcp-cached-image-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache_dir);
        let cache = ImageCache::new(&cache_dir);
        let digest = Digest::from_bytes(b"cached-manifest");
        let rootfs = cache_dir.join("rootfs/sha256").join(digest.hex());
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::write(rootfs.join(".moraebox-rootfs-complete"), digest.to_string()).unwrap();
        let platform = Platform::host_linux();
        let lock = cache.lock_exclusive().unwrap();
        cache
            .record_image(&lock, "python:3.12", &digest, &platform)
            .unwrap();
        drop(lock);

        let runtime_stub = cache_dir.join("runtime-stub");
        std::fs::write(&runtime_stub, b"stub").unwrap();
        let result = create_sdk(ServerArgs {
            backend: "libkrun".into(),
            helper: Some(runtime_stub.clone()),
            libkrun: Some(runtime_stub),
            gvproxy: None,
            rootfs: None,
            image: Some("python:3.12".into()),
            cache_dir: cache_dir.clone(),
            registry_username: None,
            registry_password: None,
            lib_dir: None,
            cpus: 2,
            memory_mib: 512,
        })
        .await;
        assert!(result.is_ok(), "unexpected error: {:?}", result.err());
        std::fs::remove_dir_all(cache_dir).unwrap();
    }

    #[cfg(unix)]
    fn successful_command() -> Vec<String> {
        ["/usr/bin/printf", "mcp"].map(String::from).into()
    }

    #[cfg(windows)]
    fn successful_command() -> Vec<String> {
        vec![
            std::path::PathBuf::from(
                std::env::var_os("SystemRoot").expect("Windows must define SystemRoot"),
            )
            .join("System32")
            .join("cmd.exe")
            .to_string_lossy()
            .into_owned(),
            "/D".into(),
            "/C".into(),
            "exit /b 0".into(),
        ]
    }
}
