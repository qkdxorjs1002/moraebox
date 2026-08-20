#![forbid(unsafe_code)]

mod registration;

use std::{ffi::OsString, path::PathBuf, process::ExitCode, str::FromStr, sync::Arc};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use clap::{Parser, Subcommand};
use moraebox_box::{BaseDiskSpec, BaseDiskStore, BoxStore, CreateBox, EphemeralDiskStore};
use moraebox_core::{
    BoxId, OutputChunk, RunSpec, SessionId, Signal, TimeoutPolicy, resolve_cache_dir,
    resolve_state_dir,
};
use moraebox_image::{Credentials, ImageCache, Platform, digest_tree};
use moraebox_runtime::{
    Backend, BoxRootSource, BoxRuntimeConfig, LibkrunBackend, LibkrunConfig, NativeRuntimePaths,
    ProcessBackend,
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
    "terminate and clean up sessions. Pass box_id to continue from a persistent Box while ",
    "still receiving a new microVM and SessionId for every run. Use the sandbox_box_* tools ",
    "to create and manage persistent root filesystems. Only the libkrun backend provides VM isolation; the ",
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
    /// Execution backend. Defaults to the isolated native microVM backend.
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
    /// Cache root; defaults to ~/.moraebox/cache.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Persistent Box metadata root; defaults to ~/.moraebox/state.
    #[arg(long)]
    state_dir: Option<PathBuf>,
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
    /// Path to mke2fs; auto-detected when omitted.
    #[arg(long, env = "MORAE_MKE2FS")]
    mke2fs: Option<PathBuf>,
    /// Path to e2fsck; auto-detected when omitted.
    #[arg(long, env = "MORAE_E2FSCK")]
    e2fsck: Option<PathBuf>,
    /// Virtual root disk size for ephemeral image-backed runs and new Boxes.
    #[arg(long, default_value = "8GiB", value_parser = parse_disk_size)]
    disk_size: u64,
}

#[derive(Clone)]
struct McpServer {
    sdk: SandboxSdk,
    boxes: BoxServices,
}

#[derive(Clone)]
struct BoxServices {
    images: ImageCache,
    base_disks: BaseDiskStore,
    platform: Platform,
    credentials: Option<Credentials>,
    mke2fs_path: PathBuf,
    default_disk_size: u64,
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
    match create_server(server).await {
        Ok(server) => match serve(server).await {
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

#[allow(clippy::too_many_lines)]
async fn create_server(args: ServerArgs) -> Result<McpServer, Box<dyn std::error::Error>> {
    let cache_dir = resolve_cache_dir(args.cache_dir.as_deref())?;
    let state_dir = resolve_state_dir(args.state_dir.as_deref())?;
    let platform = Platform::host_linux();
    let images = ImageCache::new(&cache_dir);
    let credentials = args
        .registry_username
        .zip(args.registry_password)
        .map(|(username, password)| Credentials { username, password });
    let box_store = BoxStore::new(&state_dir);
    let base_disks = BaseDiskStore::new(&cache_dir);
    let mke2fs_path = args.mke2fs.unwrap_or_else(default_mke2fs);
    let backend: Arc<dyn Backend> = match args.backend.as_str() {
        "process" => {
            if args.rootfs.is_some() || args.image.is_some() {
                return Err("--rootfs and --image require --backend libkrun".into());
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
            let root_source = if let Some(rootfs) = args.rootfs {
                BoxRootSource {
                    manifest_digest: digest_tree(&rootfs)?.to_string(),
                    rootfs_path: rootfs,
                    platform: platform_name(&platform),
                    virtual_size_bytes: args.disk_size,
                    mke2fs_path: mke2fs_path.clone(),
                }
            } else {
                let reference = match args.image {
                    Some(reference) => reference,
                    None => images.default_reference()?,
                };
                let prepared = images
                    .resolve_or_pull(&reference, &platform, credentials.clone())
                    .await?;
                BoxRootSource {
                    rootfs_path: prepared.rootfs,
                    manifest_digest: prepared.manifest_digest,
                    platform: platform_name(&platform),
                    virtual_size_bytes: args.disk_size,
                    mke2fs_path: mke2fs_path.clone(),
                }
            };
            let mut config = LibkrunConfig::new(helper, library, &root_source.rootfs_path);
            config.library_search_path = paths.library_search_path;
            config.gvproxy_path = paths.gvproxy;
            config.network_runtime_dir = cache_dir.join("network");
            config.vcpus = args.cpus;
            config.memory_mib = args.memory_mib;
            let ephemeral_disks = EphemeralDiskStore::new(cache_dir.join("runtime"));
            let _ = ephemeral_disks.garbage_collect()?;
            Arc::new(
                LibkrunBackend::new(config).with_box_runtime(BoxRuntimeConfig {
                    boxes: box_store.clone(),
                    base_disks: base_disks.clone(),
                    ephemeral_disks,
                    source: Some(root_source),
                    e2fsck_path: args.e2fsck.unwrap_or_else(default_e2fsck),
                }),
            )
        }
        _ => return Err("unsupported backend".into()),
    };
    Ok(McpServer {
        sdk: SandboxSdk::new(backend).with_box_store(box_store),
        boxes: BoxServices {
            images,
            base_disks,
            platform,
            credentials,
            mke2fs_path,
            default_disk_size: args.disk_size,
        },
    })
}

async fn serve(server: McpServer) -> Result<(), Box<dyn std::error::Error>> {
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
        let response = handle_request(&server, request).await;
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

async fn handle_request(server: &McpServer, request: Value) -> Value {
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
            call_tool(
                server,
                id,
                request.get("params").cloned().unwrap_or_default(),
            )
            .await
        }
        _ => protocol_error(id, -32601, "method not found"),
    }
}

async fn call_tool(server: &McpServer, id: Value, params: Value) -> Value {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return protocol_error(id, -32602, "tool name is required");
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let result = match name {
        "sandbox_exec" => sandbox_exec(&server.sdk, arguments).await,
        "sandbox_io" => sandbox_io(&server.sdk, arguments).await,
        "sandbox_stop" => sandbox_stop(&server.sdk, arguments).await,
        "sandbox_box_create" => sandbox_box_create(server, arguments).await,
        "sandbox_box_list" => sandbox_box_list(server, arguments).await,
        "sandbox_box_get" => sandbox_box_get(server, arguments).await,
        "sandbox_box_delete" => sandbox_box_delete(server, arguments).await,
        "sandbox_box_reset" => sandbox_box_reset(server, arguments).await,
        "sandbox_box_clone" => sandbox_box_clone(server, arguments).await,
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
    spec.box_id = args.box_id;
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

async fn sandbox_box_create(server: &McpServer, arguments: Value) -> Result<Value, String> {
    let args: BoxCreateArgs =
        serde_json::from_value(arguments).map_err(|error| error.to_string())?;
    let reference = args
        .image
        .map_or_else(|| server.boxes.images.default_reference(), Ok)
        .map_err(|error| error.to_string())?;
    let prepared = server
        .boxes
        .images
        .resolve_or_pull(
            &reference,
            &server.boxes.platform,
            server.boxes.credentials.clone(),
        )
        .await
        .map_err(|error| error.to_string())?;
    let disk_size = args
        .disk_size_bytes
        .unwrap_or(server.boxes.default_disk_size);
    if disk_size == 0 {
        return Err("disk_size_bytes must be greater than zero".into());
    }
    let base_spec = BaseDiskSpec::new(
        prepared.manifest_digest.clone(),
        platform_name(&server.boxes.platform),
        disk_size,
    );
    let base_disks = server.boxes.base_disks.clone();
    let rootfs = prepared.rootfs;
    let mke2fs = server.boxes.mke2fs_path.clone();
    let base =
        tokio::task::spawn_blocking(move || base_disks.prepare(&base_spec, &rootfs, &mke2fs))
            .await
            .map_err(|error| format!("base disk task failed: {error}"))?
            .map_err(|error| error.to_string())?;
    let metadata = server
        .sdk
        .create_box(
            CreateBox::new(
                prepared.manifest_digest,
                platform_name(&server.boxes.platform),
                disk_size,
            ),
            base.disk_path().to_path_buf(),
        )
        .await
        .map_err(|error| error.to_string())?;
    serde_json::to_value(metadata).map_err(|error| error.to_string())
}

async fn sandbox_box_list(server: &McpServer, arguments: Value) -> Result<Value, String> {
    let _: EmptyArgs = serde_json::from_value(arguments).map_err(|error| error.to_string())?;
    let boxes = server
        .sdk
        .list_boxes()
        .await
        .map_err(|error| error.to_string())?;
    Ok(json!({ "boxes": boxes }))
}

async fn sandbox_box_get(server: &McpServer, arguments: Value) -> Result<Value, String> {
    let args: BoxIdArgs = serde_json::from_value(arguments).map_err(|error| error.to_string())?;
    let metadata = server
        .sdk
        .get_box(args.box_id)
        .await
        .map_err(|error| error.to_string())?;
    serde_json::to_value(metadata).map_err(|error| error.to_string())
}

async fn sandbox_box_delete(server: &McpServer, arguments: Value) -> Result<Value, String> {
    let args: ConfirmedBoxArgs =
        serde_json::from_value(arguments).map_err(|error| error.to_string())?;
    require_confirmation(args.confirm)?;
    let metadata = server
        .sdk
        .delete_box(args.box_id)
        .await
        .map_err(|error| error.to_string())?;
    Ok(json!({ "deleted": metadata.box_id }))
}

async fn sandbox_box_reset(server: &McpServer, arguments: Value) -> Result<Value, String> {
    let args: ConfirmedBoxArgs =
        serde_json::from_value(arguments).map_err(|error| error.to_string())?;
    require_confirmation(args.confirm)?;
    let current = server
        .sdk
        .get_box(args.box_id)
        .await
        .map_err(|error| error.to_string())?;
    let spec = BaseDiskSpec::new(
        current.manifest_digest,
        current.platform,
        current.virtual_size_bytes,
    );
    let base_disks = server.boxes.base_disks.clone();
    let base = tokio::task::spawn_blocking(move || base_disks.get(&spec))
        .await
        .map_err(|error| format!("base disk task failed: {error}"))?
        .map_err(|error| error.to_string())?
        .ok_or_else(|| {
            format!(
                "the immutable base disk for Box {} is not cached; recreate the image-backed Box instead",
                args.box_id
            )
        })?;
    let metadata = server
        .sdk
        .reset_box(args.box_id, base.disk_path().to_path_buf())
        .await
        .map_err(|error| error.to_string())?;
    serde_json::to_value(metadata).map_err(|error| error.to_string())
}

async fn sandbox_box_clone(server: &McpServer, arguments: Value) -> Result<Value, String> {
    let args: ConfirmedBoxArgs =
        serde_json::from_value(arguments).map_err(|error| error.to_string())?;
    require_confirmation(args.confirm)?;
    let metadata = server
        .sdk
        .clone_box(args.box_id)
        .await
        .map_err(|error| error.to_string())?;
    serde_json::to_value(metadata).map_err(|error| error.to_string())
}

fn require_confirmation(confirmed: bool) -> Result<(), String> {
    if confirmed {
        Ok(())
    } else {
        Err("confirm must be true for this Box operation".into())
    }
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
                "text": String::from_utf8_lossy(&chunk.data)
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

#[allow(clippy::too_many_lines)]
fn tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "sandbox_exec",
                "title": "Execute in configured runtime",
                "description": "Start a command in the configured runtime. Every call receives a new SessionId and, with libkrun, a new microVM. Pass box_id only when the command should reuse that Box's persistent root filesystem. Prefer this for untrusted code, dependency installation, isolated experiments, reproducible Linux checks, or long-running sessions. Network access is disabled by default; set network=true to opt in for one native VM run. Set wait=false to start a session. Output chunks contain UTF-8 text; invalid byte sequences are replaced with U+FFFD.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "argv": { "type": "array", "items": { "type": "string" }, "minItems": 1 },
                        "stdin_base64": { "type": "string" },
                        "box_id": { "type": "string", "format": "uuid", "description": "Persistent Box root filesystem to reuse; the microVM and SessionId remain new." },
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
                "description": "Write stdin, close it, signal or resize, and read bounded UTF-8 text output from a cursor. Invalid byte sequences are replaced with U+FFFD.",
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
            },
            {
                "name": "sandbox_box_create",
                "title": "Create persistent Box",
                "description": "Create an independent persistent root filesystem from an OCI image. Image resolution may access the registry when the immutable base is not cached.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "image": { "type": "string", "description": "OCI image reference; uses the configured default when omitted." },
                        "disk_size_bytes": { "type": "integer", "minimum": 1, "description": "Virtual root disk size; uses the server default when omitted." }
                    },
                    "additionalProperties": false
                },
                "annotations": { "destructiveHint": false, "idempotentHint": false, "openWorldHint": true }
            },
            {
                "name": "sandbox_box_list",
                "title": "List persistent Boxes",
                "description": "List persistent Box metadata without starting a microVM.",
                "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
                "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
            },
            {
                "name": "sandbox_box_get",
                "title": "Get persistent Box",
                "description": "Read metadata for one persistent Box.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "box_id": { "type": "string", "format": "uuid" } },
                    "required": ["box_id"],
                    "additionalProperties": false
                },
                "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
            },
            {
                "name": "sandbox_box_delete",
                "title": "Delete persistent Box",
                "description": "Permanently delete one idle Box and its full root filesystem. confirm must be true.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "box_id": { "type": "string", "format": "uuid" },
                        "confirm": { "type": "boolean", "description": "Explicitly authorize permanent deletion." }
                    },
                    "required": ["box_id", "confirm"],
                    "additionalProperties": false
                },
                "annotations": { "destructiveHint": true, "idempotentHint": false, "openWorldHint": false }
            },
            {
                "name": "sandbox_box_reset",
                "title": "Reset persistent Box",
                "description": "Replace every change in one idle Box with its cached immutable base disk. confirm must be true.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "box_id": { "type": "string", "format": "uuid" },
                        "confirm": { "type": "boolean", "description": "Explicitly authorize discarding all Box changes." }
                    },
                    "required": ["box_id", "confirm"],
                    "additionalProperties": false
                },
                "annotations": { "destructiveHint": true, "idempotentHint": false, "openWorldHint": false }
            },
            {
                "name": "sandbox_box_clone",
                "title": "Clone persistent Box",
                "description": "Create a new independent Box from the current disk of an idle Box. confirm must be true because this allocates durable state.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "box_id": { "type": "string", "format": "uuid" },
                        "confirm": { "type": "boolean", "description": "Explicitly authorize creation of durable cloned state." }
                    },
                    "required": ["box_id", "confirm"],
                    "additionalProperties": false
                },
                "annotations": { "destructiveHint": false, "idempotentHint": false, "openWorldHint": false }
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
    box_id: Option<BoxId>,
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
            box_id: None,
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

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct EmptyArgs {}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BoxCreateArgs {
    image: Option<String>,
    disk_size_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoxIdArgs {
    box_id: BoxId,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfirmedBoxArgs {
    box_id: BoxId,
    confirm: bool,
}

fn default_mke2fs() -> PathBuf {
    for path in [
        "/opt/homebrew/opt/e2fsprogs/sbin/mke2fs",
        "/usr/local/opt/e2fsprogs/sbin/mke2fs",
        "/usr/sbin/mke2fs",
        "/sbin/mke2fs",
    ] {
        let path = PathBuf::from(path);
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from("mke2fs")
}

fn default_e2fsck() -> PathBuf {
    for path in [
        "/opt/homebrew/opt/e2fsprogs/sbin/e2fsck",
        "/usr/local/opt/e2fsprogs/sbin/e2fsck",
        "/usr/sbin/e2fsck",
        "/sbin/e2fsck",
    ] {
        let path = PathBuf::from(path);
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from("e2fsck")
}

fn platform_name(platform: &Platform) -> String {
    match &platform.variant {
        Some(variant) => format!("{}/{}/{}", platform.os, platform.architecture, variant),
        None => format!("{}/{}", platform.os, platform.architecture),
    }
}

fn parse_disk_size(input: &str) -> Result<u64, String> {
    let input = input.trim();
    let split = input
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(input.len());
    let (number, suffix) = input.split_at(split);
    let value = number
        .parse::<u64>()
        .map_err(|_| "disk size must start with a positive integer".to_owned())?;
    if value == 0 {
        return Err("disk size must be greater than zero".into());
    }
    let multiplier = match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kib" => 1024,
        "mib" => 1024 * 1024,
        "gib" => 1024 * 1024 * 1024,
        "kb" => 1000,
        "mb" => 1000 * 1000,
        "gb" => 1000 * 1000 * 1000,
        _ => return Err("disk size suffix must be B, KiB, MiB, GiB, KB, MB, or GB".into()),
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| "disk size is too large".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use moraebox_image::Digest;

    #[tokio::test]
    async fn initialize_list_and_call_work() {
        let server = test_server(SandboxSdk::new(Arc::new(ProcessBackend)));
        let initialized = handle_request(
            &server,
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
            "new microVM and SessionId for every run",
        ] {
            assert!(instructions.contains(expected), "missing {expected:?}");
        }
        let list = handle_request(
            &server,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .await;
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
        assert!(exec_description.contains("with libkrun"));
        assert!(exec_description.contains("reproducible Linux checks"));
        assert!(exec_description.contains("Network access is disabled by default"));
        assert_eq!(
            list.pointer("/result/tools/0/inputSchema/properties/network/default"),
            Some(&json!(false))
        );
        assert_eq!(
            list.pointer("/result/tools/0/inputSchema/properties/box_id/format"),
            Some(&json!("uuid"))
        );
        assert_eq!(
            list.pointer("/result/tools/0/annotations/openWorldHint"),
            Some(&json!(true))
        );
        let call = handle_request(
            &server,
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
        assert_eq!(
            call.pointer("/result/structuredContent/output/0/text"),
            Some(&json!("mcp"))
        );
        assert!(
            call.pointer("/result/structuredContent/output/0/data_base64")
                .is_none()
        );
    }

    #[test]
    fn output_chunks_are_rendered_as_lossy_utf8_text() {
        let output = chunks_json(&[OutputChunk {
            cursor: 7,
            channel: moraebox_core::OutputChannel::Stdout,
            data: vec![b'o', b'k', 0xff],
        }]);

        assert_eq!(output[0].get("cursor"), Some(&json!(7)));
        assert_eq!(output[0].get("channel"), Some(&json!("stdout")));
        assert_eq!(output[0].get("text"), Some(&json!("ok\u{fffd}")));
        assert!(output[0].get("data_base64").is_none());
    }

    #[tokio::test]
    async fn process_backend_rejects_vm_network_opt_in() {
        let server = test_server(SandboxSdk::new(Arc::new(ProcessBackend)));
        let call = handle_request(
            &server,
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

    #[tokio::test]
    async fn process_backend_rejects_box_execution() {
        let server = test_server(SandboxSdk::new(Arc::new(ProcessBackend)));
        let call = handle_request(
            &server,
            json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{
                    "name":"sandbox_exec",
                    "arguments":{"argv":successful_command(),"box_id":BoxId::new()}
                }
            }),
        )
        .await;

        assert_eq!(call.pointer("/result/isError"), Some(&json!(true)));
        assert!(
            call.pointer("/result/structuredContent/error")
                .and_then(Value::as_str)
                .is_some_and(|error| error.contains("Box persistence"))
        );
    }

    #[tokio::test]
    async fn box_list_get_and_confirmed_delete_work() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.ext4");
        let file = std::fs::File::create(&source).unwrap();
        file.set_len(1024 * 1024).unwrap();
        let store = BoxStore::new(temporary.path().join("state"));
        let metadata = store
            .create(
                &CreateBox::new("sha256:test", "linux/arm64", 1024 * 1024),
                &source,
            )
            .unwrap();
        let server = test_server(SandboxSdk::new(Arc::new(ProcessBackend)).with_box_store(store));

        let list = call(&server, "sandbox_box_list", json!({})).await;
        assert_eq!(
            list.pointer("/result/structuredContent/boxes/0/box_id"),
            Some(&json!(metadata.box_id))
        );
        let get = call(
            &server,
            "sandbox_box_get",
            json!({"box_id": metadata.box_id}),
        )
        .await;
        assert_eq!(
            get.pointer("/result/structuredContent/box_id"),
            Some(&json!(metadata.box_id))
        );
        let unconfirmed = call(
            &server,
            "sandbox_box_delete",
            json!({"box_id": metadata.box_id, "confirm": false}),
        )
        .await;
        assert_eq!(unconfirmed.pointer("/result/isError"), Some(&json!(true)));
        let deleted = call(
            &server,
            "sandbox_box_delete",
            json!({"box_id": metadata.box_id, "confirm": true}),
        )
        .await;
        assert_eq!(deleted.pointer("/result/isError"), Some(&json!(false)));
    }

    #[test]
    fn bare_invocation_shows_help_unless_runtime_is_configured() {
        let error = parse_args_from(["morae-mcp"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);

        let raw_args = [OsString::from("morae-mcp")];
        let mut configured = Args::try_parse_from(["morae-mcp"]).unwrap();
        assert_eq!(configured.server.backend, "libkrun");
        assert!(configured.server.cache_dir.is_none());
        assert!(configured.server.state_dir.is_none());
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

        let explicit = parse_args_from([
            "morae-mcp",
            "--cache-dir",
            "custom-cache",
            "--state-dir",
            "custom-state",
            "--image",
            "python:3.12",
        ])
        .unwrap();
        assert_eq!(explicit.server.cache_dir, Some("custom-cache".into()));
        assert_eq!(explicit.server.state_dir, Some("custom-state".into()));
    }

    #[tokio::test]
    async fn process_server_rejects_a_guest_rootfs() {
        let result = create_server(ServerArgs {
            backend: "process".into(),
            helper: None,
            libkrun: None,
            gvproxy: None,
            rootfs: Some("ignored-rootfs".into()),
            image: None,
            cache_dir: Some(".moraebox/cache".into()),
            state_dir: Some(".moraebox/state".into()),
            registry_username: None,
            registry_password: None,
            lib_dir: None,
            cpus: 2,
            memory_mib: 512,
            mke2fs: None,
            e2fsck: None,
            disk_size: 8 * 1024 * 1024 * 1024,
        })
        .await;

        let Err(error) = result else {
            panic!("process server unexpectedly accepted a guest rootfs");
        };
        assert_eq!(
            error.to_string(),
            "--rootfs and --image require --backend libkrun"
        );
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
        let result = create_server(ServerArgs {
            backend: "libkrun".into(),
            helper: Some(runtime_stub.clone()),
            libkrun: Some(runtime_stub),
            gvproxy: None,
            rootfs: None,
            image: Some("python:3.12".into()),
            cache_dir: Some(cache_dir.clone()),
            state_dir: Some(cache_dir.join("state")),
            registry_username: None,
            registry_password: None,
            lib_dir: None,
            cpus: 2,
            memory_mib: 512,
            mke2fs: None,
            e2fsck: None,
            disk_size: 8 * 1024 * 1024 * 1024,
        })
        .await;
        assert!(result.is_ok(), "unexpected error: {:?}", result.err());
        std::fs::remove_dir_all(cache_dir).unwrap();
    }

    fn test_server(sdk: SandboxSdk) -> McpServer {
        let root =
            std::env::temp_dir().join(format!("moraebox-mcp-test-services-{}", std::process::id()));
        McpServer {
            sdk,
            boxes: BoxServices {
                images: ImageCache::new(&root),
                base_disks: BaseDiskStore::new(&root),
                platform: Platform::host_linux(),
                credentials: None,
                mke2fs_path: default_mke2fs(),
                default_disk_size: 8 * 1024 * 1024 * 1024,
            },
        }
    }

    async fn call(server: &McpServer, name: &str, arguments: Value) -> Value {
        handle_request(
            server,
            json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":name,"arguments":arguments}
            }),
        )
        .await
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
