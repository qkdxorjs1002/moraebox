#![forbid(unsafe_code)]

mod registration;

use std::{
    collections::HashMap,
    ffi::OsString,
    io,
    path::PathBuf,
    process::ExitCode,
    str::FromStr,
    sync::{Arc, Mutex as StdMutex},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use clap::{Parser, Subcommand};
use moraebox_box::{
    BaseDiskSpec, BaseDiskStore, BoxStore, BoxStoreError, CreateBox, EphemeralDiskStore,
};
use moraebox_core::{
    BoxId, OutputChunk, OutputReadError, RunSpec, SessionId, Signal, TimeoutPolicy,
    resolve_cache_dir, resolve_state_dir,
};
use moraebox_image::{Credentials, ImageCache, Platform, digest_tree};
use moraebox_runtime::{
    Backend, BackendError, BoxRootSource, BoxRuntimeConfig, LibkrunBackend, LibkrunConfig,
    NativeRuntimePaths, ProcessBackend, SessionError,
};
use moraebox_sdk::{
    ExecutionPageResult, IoRequest, IoResult, MAX_IO_OUTPUT_READ_BYTES, SandboxSdk, SdkError,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter},
    sync::{Semaphore, mpsc, oneshot},
    task::JoinSet,
};

const PROTOCOL_VERSION: &str = "2026-07-28";
const MAX_CONCURRENT_REQUESTS: usize = 32;
const RESPONSE_QUEUE_CAPACITY: usize = 128;
const SANDBOX_EXEC_INLINE_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_MCP_STDIN_BYTES: usize = 1024 * 1024;
const MAX_MCP_STDIN_BASE64_CHARS: usize = MAX_MCP_STDIN_BYTES.div_ceil(3) * 4;
type InflightRequests = Arc<StdMutex<HashMap<String, Option<oneshot::Sender<()>>>>>;
const SERVER_INSTRUCTIONS: &str = concat!(
    "Use sandbox_exec when a command benefits from a disposable execution environment, ",
    "including untrusted code, dependency installation, isolated experiments, reproducible ",
    "Linux checks, or long-running sessions. Use wait=true for one-shot commands; its inline ",
    "output is limited to 1 MiB, and has_more output can be read with sandbox_io using the ",
    "returned SessionId and continuation_cursor within five minutes. Use ",
    "wait=false to start sessions; use sandbox_io for cursor-based I/O and sandbox_stop to ",
    "terminate and clean up sessions. wait=true sessions belong to their request and are ",
    "cleaned when that request is cancelled. wait=false sessions belong to this stdio ",
    "connection and remain available until sandbox_remove or client disconnect. Up to 32 ",
    "sessions may run at once; completed async sessions retain status and output for five ",
    "minutes unless sandbox_remove releases them sooner. sandbox_stop preserves the completed ",
    "record for output reads. Pass box_id to continue from a persistent Box while ",
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
    let (responses, response_receiver) = mpsc::channel(RESPONSE_QUEUE_CAPACITY);
    let writer = tokio::spawn(write_responses(response_receiver));
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS));
    let inflight: InflightRequests = Arc::new(StdMutex::new(HashMap::new()));
    let mut requests = JoinSet::new();
    let input_error = loop {
        let line = match input.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break None,
            Err(error) => break Some(error),
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                if responses
                    .send(protocol_error(Value::Null, -32700, &error.to_string()))
                    .await
                    .is_err()
                {
                    break Some(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "MCP response writer stopped",
                    ));
                }
                continue;
            }
        };
        if request.get("method").and_then(Value::as_str) == Some("notifications/cancelled") {
            if let Some(request_id) = request.pointer("/params/requestId") {
                cancel_request(&inflight, request_id);
            }
            continue;
        }
        if request.get("id").is_none() {
            continue;
        }
        if let Err(error) = dispatch_request(
            &server,
            request,
            &responses,
            &permits,
            &inflight,
            &mut requests,
        )
        .await
        {
            break Some(error);
        }
    };

    cancel_all_requests(&inflight);
    drop(responses);

    let mut request_error = None;
    while let Some(result) = requests.join_next().await {
        if let Err(error) = result
            && request_error.is_none()
        {
            request_error = Some(io::Error::other(format!(
                "MCP request task failed: {error}"
            )));
        }
    }
    let cleanup_error = server
        .sdk
        .shutdown()
        .await
        .err()
        .map(|error| io::Error::other(format!("MCP session cleanup failed: {error}")));
    let writer_result = writer
        .await
        .map_err(|error| io::Error::other(format!("MCP writer task failed: {error}")))?;
    if let Some(error) = input_error {
        return Err(error.into());
    }
    if let Some(error) = request_error {
        return Err(error.into());
    }
    if let Some(error) = cleanup_error {
        return Err(error.into());
    }
    writer_result?;
    Ok(())
}

async fn dispatch_request(
    server: &McpServer,
    request: Value,
    responses: &mpsc::Sender<Value>,
    permits: &Arc<Semaphore>,
    inflight: &InflightRequests,
    requests: &mut JoinSet<()>,
) -> io::Result<()> {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let key = request_key(&id);
    let (cancel, cancellation) = oneshot::channel();
    if !register_request(inflight, &key, cancel) {
        responses
            .send(protocol_error(id, -32600, "duplicate in-flight request id"))
            .await
            .map_err(|_| response_writer_stopped())?;
        return Ok(());
    }
    let server = server.clone();
    let responses = responses.clone();
    let inflight = Arc::clone(inflight);
    let permits = Arc::clone(permits);
    requests.spawn(async move {
        let mut cancellation = cancellation;
        let permit = tokio::select! {
            permit = permits.acquire_owned() => {
                permit.expect("request semaphore remains open")
            }
            _ = &mut cancellation => {
                inflight
                    .lock()
                    .expect("in-flight request lock is not poisoned")
                    .remove(&key);
                let _ = responses
                    .send(protocol_error(id, -32800, "request cancelled"))
                    .await;
                return;
            }
        };
        let _permit = permit;
        let response = handle_cancellable_request(&server, request, cancellation).await;
        inflight
            .lock()
            .expect("in-flight request lock is not poisoned")
            .remove(&key);
        let _ = responses.send(response).await;
    });
    Ok(())
}

fn register_request(
    inflight: &InflightRequests,
    key: &str,
    cancellation: oneshot::Sender<()>,
) -> bool {
    let mut inflight = inflight
        .lock()
        .expect("in-flight request lock is not poisoned");
    if inflight.contains_key(key) {
        false
    } else {
        inflight.insert(key.to_owned(), Some(cancellation));
        true
    }
}

fn cancel_request(inflight: &InflightRequests, request_id: &Value) {
    let cancellation = inflight
        .lock()
        .expect("in-flight request lock is not poisoned")
        .get_mut(&request_key(request_id))
        .and_then(Option::take);
    if let Some(cancellation) = cancellation {
        let _ = cancellation.send(());
    }
}

fn cancel_all_requests(inflight: &InflightRequests) {
    let cancellations = std::mem::take(
        &mut *inflight
            .lock()
            .expect("in-flight request lock is not poisoned"),
    );
    for cancellation in cancellations.into_values().flatten() {
        let _ = cancellation.send(());
    }
}

fn response_writer_stopped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "MCP response writer stopped")
}

fn request_key(id: &Value) -> String {
    serde_json::to_string(id).expect("JSON-RPC request id is serializable")
}

async fn write_responses(mut responses: mpsc::Receiver<Value>) -> io::Result<()> {
    let mut output = BufWriter::new(tokio::io::stdout());
    while let Some(response) = responses.recv().await {
        write_response(&mut output, &response).await?;
    }
    Ok(())
}

async fn write_response<W>(output: &mut W, response: &Value) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut encoded = serde_json::to_vec(response).map_err(io::Error::other)?;
    encoded.push(b'\n');
    output.write_all(&encoded).await?;
    output.flush().await?;
    Ok(())
}

#[cfg(test)]
async fn handle_request(server: &McpServer, request: Value) -> Value {
    handle_request_inner(server, request, None).await
}

async fn handle_cancellable_request(
    server: &McpServer,
    request: Value,
    cancellation: oneshot::Receiver<()>,
) -> Value {
    handle_request_inner(server, request, Some(cancellation)).await
}

async fn handle_request_inner(
    server: &McpServer,
    request: Value,
    cancellation: Option<oneshot::Receiver<()>>,
) -> Value {
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
                cancellation,
            )
            .await
        }
        _ => protocol_error(id, -32601, "method not found"),
    }
}

async fn call_tool(
    server: &McpServer,
    id: Value,
    params: Value,
    cancellation: Option<oneshot::Receiver<()>>,
) -> Value {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return protocol_error(id, -32602, "tool name is required");
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let result: Result<ToolOutput, ToolError> = match name {
        "sandbox_exec" => match sandbox_exec(&server.sdk, arguments, cancellation).await {
            Ok(value) => Ok(value),
            Err(ExecError::Cancelled) => {
                return protocol_error(id, -32800, "request cancelled");
            }
            Err(ExecError::Failed(error)) => Err(error),
        },
        "sandbox_io" => sandbox_io(&server.sdk, arguments)
            .await
            .map(ToolOutput::mirrored),
        "sandbox_stop" => sandbox_stop(&server.sdk, arguments)
            .await
            .map(ToolOutput::mirrored),
        "sandbox_remove" => sandbox_remove(&server.sdk, arguments)
            .await
            .map(ToolOutput::mirrored),
        "sandbox_box_create" => sandbox_box_create(server, arguments)
            .await
            .map(ToolOutput::mirrored),
        "sandbox_box_list" => sandbox_box_list(server, arguments)
            .await
            .map(ToolOutput::mirrored),
        "sandbox_box_get" => sandbox_box_get(server, arguments)
            .await
            .map(ToolOutput::mirrored),
        "sandbox_box_delete" => sandbox_box_delete(server, arguments)
            .await
            .map(ToolOutput::mirrored),
        "sandbox_box_reset" => sandbox_box_reset(server, arguments)
            .await
            .map(ToolOutput::mirrored),
        "sandbox_box_clone" => sandbox_box_clone(server, arguments)
            .await
            .map(ToolOutput::mirrored),
        _ => return protocol_error(id, -32602, "unknown tool"),
    };
    match result {
        Ok(value) => success(id, tool_result(value, false)),
        Err(error) => success(id, tool_result(error.output(), true)),
    }
}

struct ToolOutput {
    structured_content: Value,
    content_text: String,
}

impl ToolOutput {
    fn mirrored(structured_content: Value) -> Self {
        let content_text = structured_content.to_string();
        Self {
            structured_content,
            content_text,
        }
    }

    fn summarized(structured_content: Value, content_text: String) -> Self {
        Self {
            structured_content,
            content_text,
        }
    }
}

#[derive(Debug)]
enum ExecError {
    Cancelled,
    Failed(ToolError),
}

impl From<SdkError> for ExecError {
    fn from(error: SdkError) -> Self {
        match error {
            SdkError::RequestCancelled => Self::Cancelled,
            error => Self::Failed(error.into()),
        }
    }
}

#[derive(Debug, Serialize)]
struct ToolError {
    code: &'static str,
    stage: String,
    retryable: bool,
    message: String,
    remediation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    earliest_cursor: Option<u64>,
}

impl ToolError {
    fn new(
        code: &'static str,
        stage: impl Into<String>,
        retryable: bool,
        message: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            code,
            stage: stage.into(),
            retryable,
            message: message.into(),
            remediation: remediation.into(),
            earliest_cursor: None,
        }
    }

    fn invalid_arguments(message: impl Into<String>) -> Self {
        Self::new(
            "invalid_arguments",
            "request_validation",
            false,
            message,
            "Correct the tool arguments and call the tool again.",
        )
    }

    fn internal(stage: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(
            "internal_error",
            stage,
            false,
            message,
            "Inspect the MCP server diagnostics and retry after correcting the server configuration.",
        )
    }

    fn output(self) -> ToolOutput {
        let content_text = format!(
            "{} during {}: {} Remediation: {}",
            self.code, self.stage, self.message, self.remediation
        );
        ToolOutput::summarized(json!({ "error": self }), content_text)
    }
}

impl From<OutputReadError> for ToolError {
    fn from(error: OutputReadError) -> Self {
        match error {
            OutputReadError::CursorExpired {
                requested,
                earliest,
            } => {
                let mut envelope = Self::new(
                    "cursor_expired",
                    "output_read",
                    true,
                    format!(
                        "output cursor {requested} expired; earliest retained cursor is {earliest}"
                    ),
                    format!("Retry sandbox_io with cursor={earliest}."),
                );
                envelope.earliest_cursor = Some(earliest);
                envelope
            }
            OutputReadError::CursorAhead { requested, next } => Self::new(
                "cursor_ahead",
                "output_read",
                true,
                format!("output cursor {requested} is ahead of next cursor {next}"),
                format!("Retry sandbox_io with cursor={next}, or wait for more output."),
            ),
        }
    }
}

impl From<BackendError> for ToolError {
    fn from(error: BackendError) -> Self {
        match error {
            BackendError::InvalidSpec(message) => Self::invalid_arguments(message),
            BackendError::Unsupported(capability) => Self::new(
                "unsupported_capability",
                "backend_capability",
                false,
                format!("backend does not support {capability}"),
                "Change the arguments or select a backend that supports this capability.",
            ),
            BackendError::Timeout { stage, limit } => Self::new(
                "timeout",
                stage.to_string(),
                true,
                format!("run timed out after {limit:?} during {stage}"),
                "Retry with a larger timeout, or reduce the work performed by the command.",
            ),
            error => Self::new(
                "backend_failure",
                "backend",
                true,
                error.to_string(),
                "Check backend readiness and configuration, then retry the operation.",
            ),
        }
    }
}

impl From<SessionError> for ToolError {
    fn from(error: SessionError) -> Self {
        match error {
            SessionError::Backend(error) => error.into(),
            SessionError::Output(error) => error.into(),
            SessionError::Io(error) => Self::new(
                "session_io_failure",
                format!("{}_io", error.stream),
                false,
                error.to_string(),
                "Remove the failed session and start a new sandbox session.",
            ),
            SessionError::SessionClosed => Self::new(
                "session_closed",
                "session_io",
                false,
                "session is no longer available",
                "Start a new sandbox session and use its SessionId.",
            ),
            SessionError::StdinWriteTooLarge { requested, maximum } => Self::new(
                "stdin_too_large",
                "stdin_write",
                false,
                format!(
                    "stdin write is {requested} bytes, exceeding the {maximum}-byte queue limit"
                ),
                format!("Send stdin in chunks no larger than {maximum} bytes."),
            ),
            SessionError::OutputReadTooLarge { requested, maximum } => Self::new(
                "output_read_too_large",
                "output_read",
                false,
                format!(
                    "output read is {requested} bytes, exceeding the {maximum}-byte request limit"
                ),
                format!("Set max_bytes to at most {maximum}."),
            ),
            error => Self::new(
                "session_failure",
                "session_control",
                false,
                error.to_string(),
                "Remove the failed session and start a new sandbox session.",
            ),
        }
    }
}

impl From<BoxStoreError> for ToolError {
    fn from(error: BoxStoreError) -> Self {
        let message = error.to_string();
        match error {
            BoxStoreError::NotFound(_) => Self::new(
                "box_not_found",
                "box_lookup",
                false,
                message,
                "Use sandbox_box_list to select an existing BoxId.",
            ),
            BoxStoreError::Busy { .. } | BoxStoreError::BaseDiskBusy { .. } => Self::new(
                "box_busy",
                "box_lock",
                true,
                message,
                "Wait for the current Box operation to finish, then retry.",
            ),
            BoxStoreError::NeedsRepair(_) => Self::new(
                "box_needs_repair",
                "box_repair",
                false,
                message,
                "Reset or recreate the Box before running it again.",
            ),
            BoxStoreError::Io(_) => Self::new(
                "box_io_failure",
                "box_storage",
                true,
                message,
                "Check storage availability and permissions, then retry.",
            ),
            error => Self::new(
                "box_failure",
                "box_storage",
                false,
                error.to_string(),
                "Inspect the Box metadata and storage, then reset or recreate the Box.",
            ),
        }
    }
}

impl From<SdkError> for ToolError {
    fn from(error: SdkError) -> Self {
        match error {
            SdkError::Session(error) => error.into(),
            SdkError::UnknownSession(session_id) => Self::new(
                "session_not_found",
                "session_lookup",
                false,
                format!("unknown sandbox session {session_id}"),
                "Start a new sandbox session and use its SessionId.",
            ),
            SdkError::SessionLimitExceeded { maximum } => Self::new(
                "session_limit_exceeded",
                "session_start",
                true,
                format!("active sandbox session limit reached (maximum {maximum})"),
                "Stop or remove an existing session, then retry.",
            ),
            SdkError::OutputReadTooLarge { requested, maximum } => Self::new(
                "output_read_too_large",
                "output_read",
                false,
                format!("output read is {requested} bytes, exceeding the {maximum}-byte SDK limit"),
                format!("Set max_bytes to at most {maximum}."),
            ),
            SdkError::OutputReadEmpty => {
                Self::invalid_arguments("output read must request at least one byte")
            }
            SdkError::RequestCancelled => Self::new(
                "request_cancelled",
                "request",
                true,
                "sandbox request was cancelled",
                "Call the tool again if the operation is still required.",
            ),
            SdkError::BoxStore(error) => error.into(),
            SdkError::BoxStoreNotConfigured => Self::new(
                "box_store_unavailable",
                "box_storage",
                false,
                "Box store is not configured for this SDK instance",
                "Configure the MCP server state directory before using Box tools.",
            ),
            error => Self::internal("sdk", error.to_string()),
        }
    }
}

async fn sandbox_exec(
    sdk: &SandboxSdk,
    arguments: Value,
    cancellation: Option<oneshot::Receiver<()>>,
) -> Result<ToolOutput, ExecError> {
    sandbox_exec_with_inline_limit(
        sdk,
        arguments,
        cancellation,
        SANDBOX_EXEC_INLINE_OUTPUT_BYTES,
    )
    .await
}

async fn sandbox_exec_with_inline_limit(
    sdk: &SandboxSdk,
    arguments: Value,
    cancellation: Option<oneshot::Receiver<()>>,
    inline_output_bytes: usize,
) -> Result<ToolOutput, ExecError> {
    let args: ExecArgs = serde_json::from_value(arguments)
        .map_err(|error| ExecError::Failed(ToolError::invalid_arguments(error.to_string())))?;
    if args.argv.is_empty() {
        return Err(ExecError::Failed(ToolError::invalid_arguments(
            "argv must contain an executable",
        )));
    }
    let mut spec = RunSpec::command(args.argv);
    spec.box_id = args.box_id;
    spec.tty = args.tty;
    spec.network = args.network;
    spec.timeout = match (args.unlimited, args.timeout_ms) {
        (true, Some(_)) => {
            return Err(ExecError::Failed(ToolError::invalid_arguments(
                "unlimited=true cannot be combined with timeout_ms",
            )));
        }
        (true, None) => TimeoutPolicy::Unlimited,
        (false, Some(milliseconds)) if milliseconds > 0 => TimeoutPolicy::Limited(milliseconds),
        (false, Some(_)) => {
            return Err(ExecError::Failed(ToolError::invalid_arguments(
                "timeout_ms must be greater than zero",
            )));
        }
        (false, None) => TimeoutPolicy::default(),
    };
    spec.stdin = decode_bounded_stdin(args.stdin_base64)
        .map_err(ExecError::Failed)?
        .unwrap_or_default();
    if args.wait {
        let result = match cancellation {
            Some(cancellation) => {
                sdk.exec_retained_cancellable(spec, inline_output_bytes, cancellation)
                    .await
            }
            None => sdk.exec_retained(spec, inline_output_bytes).await,
        }?;
        let has_more = result.next_cursor < result.output_next_cursor;
        let structured_content = execution_page_json(&result);
        let content_text = execution_content_summary(&result, has_more);
        if !has_more {
            sdk.remove(result.status.session_id).await?;
        }
        Ok(ToolOutput::summarized(structured_content, content_text))
    } else {
        let status = match cancellation {
            Some(cancellation) => sdk.start_cancellable(spec, cancellation).await,
            None => sdk.start(spec).await,
        }?;
        Ok(ToolOutput::mirrored(
            json!({ "status": status, "next_cursor": 0 }),
        ))
    }
}

async fn sandbox_io(sdk: &SandboxSdk, arguments: Value) -> Result<Value, ToolError> {
    let args: IoArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    validate_io_args(&args)?;
    let session_id = SessionId::from_str(&args.session_id)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    let stdin = decode_bounded_stdin(args.stdin_base64)?;
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
        .await?;
    Ok(io_json(&result))
}

fn validate_io_args(args: &IoArgs) -> Result<(), ToolError> {
    if !(1..=MAX_IO_OUTPUT_READ_BYTES).contains(&args.max_bytes) {
        return Err(ToolError::invalid_arguments(format!(
            "max_bytes must be between 1 and {MAX_IO_OUTPUT_READ_BYTES}"
        )));
    }
    match (args.rows, args.columns) {
        (None, None) => Ok(()),
        (Some(rows), Some(columns)) if rows > 0 && columns > 0 => Ok(()),
        (Some(_), Some(_)) => Err(ToolError::invalid_arguments(
            "rows and columns must be greater than zero",
        )),
        _ => Err(ToolError::invalid_arguments(
            "rows and columns must be provided together",
        )),
    }
}

fn decode_bounded_stdin(input: Option<String>) -> Result<Option<Vec<u8>>, ToolError> {
    let Some(input) = input else {
        return Ok(None);
    };
    if input.len() > MAX_MCP_STDIN_BASE64_CHARS {
        return Err(ToolError::invalid_arguments(format!(
            "stdin_base64 exceeds the {MAX_MCP_STDIN_BYTES}-byte decoded input limit"
        )));
    }
    let decoded = STANDARD
        .decode(input)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    if decoded.len() > MAX_MCP_STDIN_BYTES {
        return Err(ToolError::invalid_arguments(format!(
            "stdin_base64 decodes to {} bytes, exceeding the {MAX_MCP_STDIN_BYTES}-byte limit",
            decoded.len()
        )));
    }
    Ok(Some(decoded))
}

async fn sandbox_stop(sdk: &SandboxSdk, arguments: Value) -> Result<Value, ToolError> {
    let args: StopArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    let session_id = SessionId::from_str(&args.session_id)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    let status = sdk.stop(session_id).await?;
    Ok(json!({ "status": status }))
}

async fn sandbox_remove(sdk: &SandboxSdk, arguments: Value) -> Result<Value, ToolError> {
    let args: StopArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    let session_id = SessionId::from_str(&args.session_id)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    let status = sdk.remove(session_id).await?;
    match status {
        Some(status) => Ok(json!({ "removed": true, "status": status })),
        None => Ok(json!({ "removed": false })),
    }
}

async fn sandbox_box_create(server: &McpServer, arguments: Value) -> Result<Value, ToolError> {
    let args: BoxCreateArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    let reference = args
        .image
        .map_or_else(|| server.boxes.images.default_reference(), Ok)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    let prepared = server
        .boxes
        .images
        .resolve_or_pull(
            &reference,
            &server.boxes.platform,
            server.boxes.credentials.clone(),
        )
        .await
        .map_err(|error| {
            ToolError::new(
                "image_prepare_failed",
                "image_pull",
                true,
                error.to_string(),
                "Check the image reference, registry credentials, and network, then retry.",
            )
        })?;
    let disk_size = args
        .disk_size_bytes
        .unwrap_or(server.boxes.default_disk_size);
    if disk_size == 0 {
        return Err(ToolError::invalid_arguments(
            "disk_size_bytes must be greater than zero",
        ));
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
            .map_err(|error| {
                ToolError::internal(
                    "base_disk_prepare",
                    format!("base disk task failed: {error}"),
                )
            })??;
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
        .await?;
    serde_json::to_value(metadata)
        .map_err(|error| ToolError::internal("response_serialization", error.to_string()))
}

async fn sandbox_box_list(server: &McpServer, arguments: Value) -> Result<Value, ToolError> {
    let _: EmptyArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    let boxes = server.sdk.list_boxes().await?;
    Ok(json!({ "boxes": boxes }))
}

async fn sandbox_box_get(server: &McpServer, arguments: Value) -> Result<Value, ToolError> {
    let args: BoxIdArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    let metadata = server.sdk.get_box(args.box_id).await?;
    serde_json::to_value(metadata)
        .map_err(|error| ToolError::internal("response_serialization", error.to_string()))
}

async fn sandbox_box_delete(server: &McpServer, arguments: Value) -> Result<Value, ToolError> {
    let args: ConfirmedBoxArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    require_confirmation(args.confirm)?;
    let metadata = server.sdk.delete_box(args.box_id).await?;
    Ok(json!({ "deleted": metadata.box_id }))
}

async fn sandbox_box_reset(server: &McpServer, arguments: Value) -> Result<Value, ToolError> {
    let args: ConfirmedBoxArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    require_confirmation(args.confirm)?;
    let current = server.sdk.get_box(args.box_id).await?;
    let spec = BaseDiskSpec::new(
        current.manifest_digest,
        current.platform,
        current.virtual_size_bytes,
    );
    let base_disks = server.boxes.base_disks.clone();
    let base = tokio::task::spawn_blocking(move || base_disks.get(&spec))
        .await
        .map_err(|error| {
            ToolError::internal("base_disk_lookup", format!("base disk task failed: {error}"))
        })??
        .ok_or_else(|| {
            ToolError::new(
                "base_disk_not_found",
                "base_disk_lookup",
                false,
                format!(
                "the immutable base disk for Box {} is not cached; recreate the image-backed Box instead",
                args.box_id
                ),
                "Recreate the image-backed Box to restore its immutable base disk.",
            )
        })?;
    let metadata = server
        .sdk
        .reset_box(args.box_id, base.disk_path().to_path_buf())
        .await?;
    serde_json::to_value(metadata)
        .map_err(|error| ToolError::internal("response_serialization", error.to_string()))
}

async fn sandbox_box_clone(server: &McpServer, arguments: Value) -> Result<Value, ToolError> {
    let args: ConfirmedBoxArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    require_confirmation(args.confirm)?;
    let metadata = server.sdk.clone_box(args.box_id).await?;
    serde_json::to_value(metadata)
        .map_err(|error| ToolError::internal("response_serialization", error.to_string()))
}

fn require_confirmation(confirmed: bool) -> Result<(), ToolError> {
    if confirmed {
        Ok(())
    } else {
        Err(ToolError::invalid_arguments(
            "confirm must be true for this Box operation",
        ))
    }
}

fn execution_page_json(result: &ExecutionPageResult) -> Value {
    let has_more = result.next_cursor < result.output_next_cursor;
    json!({
        "status": result.status,
        "output": chunks_json(&result.output),
        "next_cursor": result.next_cursor,
        "output_next_cursor": result.output_next_cursor,
        "has_more": has_more,
        "continuation_cursor": has_more.then_some(result.next_cursor),
        "truncated": result.truncated
    })
}

fn execution_content_summary(result: &ExecutionPageResult, has_more: bool) -> String {
    let inline_bytes = result
        .output
        .iter()
        .map(|chunk| chunk.data.len())
        .sum::<usize>();
    if has_more {
        format!(
            "sandbox_exec completed; {inline_bytes} output bytes are in structuredContent; continue session {} from cursor {} with sandbox_io",
            result.status.session_id, result.next_cursor
        )
    } else {
        format!(
            "sandbox_exec completed; {inline_bytes} output bytes are in structuredContent; no continuation is required"
        )
    }
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

fn parse_signal(value: &str) -> Result<Signal, ToolError> {
    match value.to_ascii_uppercase().as_str() {
        "INT" | "SIGINT" => Ok(Signal::Interrupt),
        "TERM" | "SIGTERM" => Ok(Signal::Terminate),
        "KILL" | "SIGKILL" => Ok(Signal::Kill),
        "HUP" | "SIGHUP" => Ok(Signal::Hangup),
        _ => Err(ToolError::invalid_arguments(format!(
            "unsupported signal {value}"
        ))),
    }
}

fn tool_result(output: ToolOutput, is_error: bool) -> Value {
    let mut content_item = serde_json::Map::with_capacity(2);
    content_item.insert("type".into(), Value::String("text".into()));
    content_item.insert("text".into(), Value::String(output.content_text));
    let mut result = serde_json::Map::with_capacity(3);
    result.insert(
        "content".into(),
        Value::Array(vec![Value::Object(content_item)]),
    );
    result.insert("structuredContent".into(), output.structured_content);
    result.insert("isError".into(), Value::Bool(is_error));
    Value::Object(result)
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
                "description": "Start a command in the configured runtime. Every call receives a new SessionId and, with libkrun, a new microVM. Pass box_id only when the command should reuse that Box's persistent root filesystem. Prefer this for untrusted code, dependency installation, isolated experiments, reproducible Linux checks, or long-running sessions. Network access is disabled by default; set network=true to opt in for one native VM run. At most 32 sessions run concurrently. wait=true returns at most 1 MiB inline; when has_more is true, call sandbox_io with the returned SessionId and continuation_cursor within five minutes. Cancelled wait=true runs are cleaned. wait=false starts a session owned by this stdio connection until sandbox_remove or disconnect; completed status and output expire after five minutes. Output chunks contain UTF-8 text; invalid byte sequences are replaced with U+FFFD.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "argv": { "type": "array", "items": { "type": "string" }, "minItems": 1 },
                        "stdin_base64": { "type": "string", "maxLength": MAX_MCP_STDIN_BASE64_CHARS },
                        "box_id": { "type": "string", "format": "uuid", "description": "Persistent Box root filesystem to reuse; the microVM and SessionId remain new." },
                        "timeout_ms": { "type": "integer", "minimum": 1 },
                        "unlimited": { "type": "boolean", "default": false },
                        "network": { "type": "boolean", "default": false },
                        "tty": { "type": "boolean", "default": false },
                        "wait": { "type": "boolean", "default": true }
                    },
                    "required": ["argv"],
                    "additionalProperties": false
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
                        "max_bytes": { "type": "integer", "minimum": 1, "maximum": MAX_IO_OUTPUT_READ_BYTES, "default": 1_048_576 },
                        "stdin_base64": { "type": "string", "maxLength": MAX_MCP_STDIN_BASE64_CHARS },
                        "close_stdin": { "type": "boolean", "default": false },
                        "rows": { "type": "integer", "minimum": 1, "maximum": 65_535 },
                        "columns": { "type": "integer", "minimum": 1, "maximum": 65_535 },
                        "signal": { "type": "string", "enum": ["INT", "TERM", "KILL", "HUP"] }
                    },
                    "required": ["session_id"],
                    "additionalProperties": false
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
                    "required": ["session_id"],
                    "additionalProperties": false
                },
                "annotations": { "destructiveHint": true, "idempotentHint": true, "openWorldHint": false }
            },
            {
                "name": "sandbox_remove",
                "title": "Remove sandbox session",
                "description": "Stop a running session if needed, wait for cleanup, and immediately remove its retained status and output. Repeating the call returns removed=false.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "session_id": { "type": "string" } },
                    "required": ["session_id"],
                    "additionalProperties": false
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
        assert!(exec_description.contains("continuation_cursor"));
        assert_eq!(
            list.pointer("/result/tools/0/inputSchema/properties/network/default"),
            Some(&json!(false))
        );
        assert_eq!(
            list.pointer("/result/tools/0/inputSchema/properties/box_id/format"),
            Some(&json!("uuid"))
        );
        assert_eq!(
            list.pointer("/result/tools/0/inputSchema/additionalProperties"),
            Some(&json!(false))
        );
        assert_eq!(
            list.pointer("/result/tools/1/inputSchema/properties/rows/maximum"),
            Some(&json!(u16::MAX))
        );
        assert_eq!(
            list.pointer("/result/tools/0/annotations/openWorldHint"),
            Some(&json!(true))
        );
        assert!(
            list.pointer("/result/tools")
                .and_then(Value::as_array)
                .is_some_and(|tools| tools
                    .iter()
                    .any(|tool| tool.get("name") == Some(&json!("sandbox_remove"))))
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
        let content_text = call
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .unwrap();
        assert!(content_text.starts_with("sandbox_exec completed"));
        assert!(!content_text.contains("mcp"));
    }

    #[tokio::test]
    async fn waiting_exec_exposes_a_real_continuation_without_duplicating_output() {
        let sdk = SandboxSdk::new(Arc::new(ProcessBackend));
        let result =
            sandbox_exec_with_inline_limit(&sdk, json!({ "argv": successful_command() }), None, 2)
                .await
                .unwrap();

        assert_eq!(
            result.structured_content.get("has_more"),
            Some(&json!(true))
        );
        assert_eq!(
            result.structured_content.get("continuation_cursor"),
            Some(&json!(2))
        );
        assert_eq!(
            result.structured_content.get("output_next_cursor"),
            Some(&json!(3))
        );
        assert!(!result.content_text.contains("mcp"));
        let session_id = result
            .structured_content
            .pointer("/status/session_id")
            .and_then(Value::as_str)
            .unwrap()
            .parse::<SessionId>()
            .unwrap();
        let continuation = sdk
            .io(
                session_id,
                IoRequest {
                    cursor: 2,
                    ..IoRequest::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(continuation.next_cursor, 3);
        assert_eq!(continuation.output[0].data, b"p");
        sdk.remove(session_id).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn waiting_exec_removes_a_session_when_inline_output_is_complete() {
        let sdk = SandboxSdk::new(Arc::new(ProcessBackend));
        let result =
            sandbox_exec_with_inline_limit(&sdk, json!({ "argv": successful_command() }), None, 3)
                .await
                .unwrap();
        let session_id = result
            .structured_content
            .pointer("/status/session_id")
            .and_then(Value::as_str)
            .unwrap()
            .parse::<SessionId>()
            .unwrap();

        assert_eq!(
            result.structured_content.get("has_more"),
            Some(&json!(false))
        );
        assert_eq!(
            result.structured_content.get("continuation_cursor"),
            Some(&Value::Null)
        );
        assert!(matches!(
            sdk.wait(session_id).await,
            Err(SdkError::UnknownSession(id)) if id == session_id
        ));
    }

    #[tokio::test]
    async fn tool_handlers_reject_schema_bypasses_before_execution() {
        let server = test_server(SandboxSdk::new(Arc::new(ProcessBackend)));
        let cases = [
            (
                "sandbox_exec",
                json!({ "argv": successful_command(), "unknown": true }),
                "unknown field",
            ),
            (
                "sandbox_exec",
                json!({
                    "argv": successful_command(),
                    "unlimited": true,
                    "timeout_ms": 10
                }),
                "cannot be combined",
            ),
            (
                "sandbox_io",
                json!({ "session_id": SessionId::new(), "max_bytes": 0 }),
                "max_bytes must be between",
            ),
            (
                "sandbox_io",
                json!({ "session_id": SessionId::new(), "rows": 24 }),
                "provided together",
            ),
            (
                "sandbox_io",
                json!({ "session_id": SessionId::new(), "rows": 0, "columns": 80 }),
                "greater than zero",
            ),
            (
                "sandbox_io",
                json!({ "session_id": SessionId::new(), "rows": 65_536, "columns": 80 }),
                "invalid value",
            ),
        ];
        for (index, (name, arguments, expected)) in cases.into_iter().enumerate() {
            let response = handle_request(
                &server,
                json!({
                    "jsonrpc": "2.0",
                    "id": index,
                    "method": "tools/call",
                    "params": { "name": name, "arguments": arguments }
                }),
            )
            .await;
            assert_eq!(response.pointer("/result/isError"), Some(&json!(true)));
            let envelope = response
                .pointer("/result/structuredContent/error")
                .and_then(Value::as_object)
                .unwrap();
            assert_eq!(envelope.get("code"), Some(&json!("invalid_arguments")));
            assert_eq!(envelope.get("stage"), Some(&json!("request_validation")));
            assert_eq!(envelope.get("retryable"), Some(&json!(false)));
            assert!(
                envelope
                    .get("remediation")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty())
            );
            let message = envelope.get("message").and_then(Value::as_str).unwrap();
            assert!(
                message.contains(expected),
                "expected {expected:?} in {message:?}"
            );
        }
    }

    #[test]
    fn stdin_decoder_rejects_encoded_and_decoded_oversize_inputs() {
        let oversized = STANDARD.encode(vec![0_u8; MAX_MCP_STDIN_BYTES + 1]);
        let error = decode_bounded_stdin(Some(oversized)).unwrap_err();
        assert!(
            error.message.contains("decoded input limit") || error.message.contains("decodes to")
        );
        assert_eq!(error.code, "invalid_arguments");

        let encoded_oversize = "A".repeat(MAX_MCP_STDIN_BASE64_CHARS + 1);
        assert!(
            decode_bounded_stdin(Some(encoded_oversize))
                .unwrap_err()
                .message
                .contains("decoded input limit")
        );
    }

    #[test]
    fn cursor_expiry_error_includes_recovery_cursor() {
        let output = ToolError::from(SdkError::Session(SessionError::Output(
            OutputReadError::CursorExpired {
                requested: 4,
                earliest: 12,
            },
        )))
        .output();

        assert_eq!(
            output.structured_content.pointer("/error/code"),
            Some(&json!("cursor_expired"))
        );
        assert_eq!(
            output.structured_content.pointer("/error/stage"),
            Some(&json!("output_read"))
        );
        assert_eq!(
            output.structured_content.pointer("/error/retryable"),
            Some(&json!(true))
        );
        assert_eq!(
            output.structured_content.pointer("/error/earliest_cursor"),
            Some(&json!(12))
        );
        assert!(
            output
                .structured_content
                .pointer("/error/remediation")
                .and_then(Value::as_str)
                .is_some_and(|value| value.contains("cursor=12"))
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
            call.pointer("/result/structuredContent/error/message")
                .and_then(Value::as_str)
                .is_some_and(|error| error.contains("without VM isolation"))
        );
        assert_eq!(
            call.pointer("/result/structuredContent/error/code"),
            Some(&json!("unsupported_capability"))
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
            call.pointer("/result/structuredContent/error/message")
                .and_then(Value::as_str)
                .is_some_and(|error| error.contains("Box persistence"))
        );
        assert_eq!(
            call.pointer("/result/structuredContent/error/code"),
            Some(&json!("unsupported_capability"))
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
