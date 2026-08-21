#![forbid(unsafe_code)]

mod errors;
mod registration;
mod tools;
mod transport;

use errors::McpServerError;
use tools::parse_disk_size;
use transport::serve;

use std::{
    collections::HashMap,
    ffi::OsString,
    io,
    path::PathBuf,
    process::ExitCode,
    str::FromStr,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use clap::{Parser, Subcommand};
use moraebox_box::{BaseDiskSpec, BaseDiskStore, BoxStore, BoxStoreError, CreateBox};
use moraebox_core::{
    BoxId, DEFAULT_KILL_GRACE, DEFAULT_OUTPUT_LIMIT, ImagePullPolicy, MAX_KILL_GRACE,
    MAX_OUTPUT_LIMIT, OutputChunk, OutputReadError, RunSpec, SessionId, Signal, TimeoutPolicy,
    resolve_cache_dir, resolve_state_dir,
};
use moraebox_image::{Credentials, ImageCache, Platform};
use moraebox_runtime::{
    Backend, BackendCapabilities, BackendError, BoxRootSource, BoxRuntimeConfig, DiskToolPaths,
    LibkrunBackend, LibkrunConfig, ProcessBackend, RunBudget, RunStage, SessionError,
    SpawnedSandbox, StageError,
};
use moraebox_sdk::{
    ExecutionPageResult, IoRequest, IoResult, MAX_IO_OUTPUT_READ_BYTES, ManagedStorage,
    NativeRuntimeOverrides, NativeSandboxConfig, SandboxSdk, SdkError,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter},
    sync::{Semaphore, mpsc, oneshot},
    task::JoinSet,
};

const PROTOCOL_VERSION: &str = "2026-07-28";
const SUPPORTED_PROTOCOL_VERSIONS: [&str; 3] = [PROTOCOL_VERSION, "2025-11-25", "2025-06-18"];
const MAX_CONCURRENT_REQUESTS: usize = 32;
const RESPONSE_QUEUE_CAPACITY: usize = 128;
const SANDBOX_EXEC_INLINE_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_MCP_STDIN_BYTES: usize = 1024 * 1024;
const MAX_MCP_STDIN_BASE64_CHARS: usize = MAX_MCP_STDIN_BYTES.div_ceil(3) * 4;
const MAX_MCP_WAIT_MS: u64 = 30_000;
type InflightRequests = Arc<StdMutex<HashMap<String, Option<oneshot::Sender<()>>>>>;
const SERVER_INSTRUCTIONS: &str = concat!(
    "Use sandbox_exec when a command benefits from a disposable execution environment, ",
    "including untrusted code, dependency installation, isolated experiments, reproducible ",
    "Linux checks, or long-running sessions. Use wait=true for one-shot commands; its inline ",
    "output is limited to 1 MiB, and has_more output can be read with sandbox_io using the ",
    "returned SessionId and continuation_cursor within five minutes. Use ",
    "wait=false to start sessions; use sandbox_io for cursor-based I/O and a bounded wait_ms ",
    "long-poll, sandbox_session_list and sandbox_session_status to inspect sessions, and ",
    "sandbox_stop to terminate and clean up sessions. wait=true sessions belong to their request and are ",
    "cleaned when that request is cancelled. wait=false sessions belong to this stdio ",
    "connection and remain available until sandbox_remove or client disconnect. Up to 32 ",
    "sessions may run at once; completed async sessions retain status and output for five ",
    "minutes unless sandbox_remove releases them sooner. sandbox_stop preserves the completed ",
    "record for output reads. The first image-backed run prepares its image lazily within the ",
    "run timeout; preparation failures report image_prepare_failed at the image_pull stage. ",
    "Pass box_id to continue from a persistent Box while ",
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

struct LazyImageBackend {
    config: LibkrunConfig,
    runtime: BoxRuntimeConfig,
    native: NativeSandboxConfig,
    images: ImageCache,
    reference: Option<String>,
    platform: Platform,
    credentials: Option<Credentials>,
}

impl LazyImageBackend {
    fn backend(&self, source: Option<BoxRootSource>) -> LibkrunBackend {
        let mut runtime = self.runtime.clone();
        runtime.source = source;
        LibkrunBackend::new(self.config.clone()).with_box_runtime(runtime)
    }

    async fn prepare(
        &self,
        policy: ImagePullPolicy,
    ) -> Result<(LibkrunBackend, String), BackendError> {
        let reference = self
            .reference
            .clone()
            .map_or_else(|| self.images.default_reference(), Ok)
            .map_err(image_prepare_error)?;
        let prepared = self
            .images
            .prepare(&reference, &self.platform, self.credentials.clone(), policy)
            .await
            .map_err(image_prepare_error)?;
        let digest = prepared.manifest_digest.clone();
        Ok((
            self.backend(Some(
                self.native.prepared_image_source(prepared, &self.platform),
            )),
            digest,
        ))
    }

    async fn prepared_backend(
        &self,
        policy: ImagePullPolicy,
        budget: &RunBudget,
    ) -> Result<(LibkrunBackend, String), BackendError> {
        match budget.run(RunStage::ImagePull, self.prepare(policy)).await {
            Ok(prepared) => Ok(prepared),
            Err(StageError::Timeout(error)) => Err(BackendError::Timeout {
                stage: error.stage,
                limit: error.limit,
            }),
            Err(StageError::Failed { source, .. }) => Err(source),
        }
    }
}

fn image_prepare_error(error: impl std::fmt::Display) -> BackendError {
    BackendError::ImagePreparation(error.to_string())
}

#[async_trait]
impl Backend for LazyImageBackend {
    fn name(&self) -> &'static str {
        "libkrun"
    }

    fn capabilities(&self) -> BackendCapabilities {
        LibkrunBackend::CAPABILITIES
    }

    async fn spawn(
        &self,
        spec: &RunSpec,
        budget: &RunBudget,
    ) -> Result<SpawnedSandbox, BackendError> {
        if spec.box_id.is_some() {
            self.backend(None).spawn(spec, budget).await
        } else {
            let (backend, digest) = self
                .prepared_backend(spec.image_pull_policy, budget)
                .await?;
            let mut spawned = backend.spawn(spec, budget).await?;
            spawned.startup.resolved_image_digest = Some(digest);
            Ok(spawned)
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = parse_args_from(std::env::args_os()).unwrap_or_else(|error| error.exit());
    let Args { command, server } = args;
    if let Some(McpCommand::Install(args)) = command {
        return match registration::install(&args).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!(
                    "morae-mcp: {error} (stage={}, retryable={})",
                    error.stage(),
                    error.retryable()
                );
                ExitCode::FAILURE
            }
        };
    }
    match create_server(server) {
        Ok(server) => match serve(server).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!(
                    "morae-mcp: {error} (stage={}, retryable={})",
                    error.stage(),
                    error.retryable()
                );
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            eprintln!(
                "morae-mcp: {error} (stage={}, retryable={})",
                error.stage(),
                error.retryable()
            );
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
fn create_server(args: ServerArgs) -> Result<McpServer, McpServerError> {
    let cache_dir = resolve_cache_dir(args.cache_dir.as_deref())?;
    let state_dir = resolve_state_dir(args.state_dir.as_deref())?;
    let platform = Platform::host_linux();
    let managed_storage = (args.backend == "libkrun")
        .then(|| ManagedStorage::open(&cache_dir, &state_dir))
        .transpose()?;
    let images = managed_storage.as_ref().map_or_else(
        || ImageCache::new(&cache_dir),
        |storage| storage.images().clone(),
    );
    let credentials = args
        .registry_username
        .zip(args.registry_password)
        .map(|(username, password)| Credentials { username, password });
    let box_store = managed_storage.as_ref().map_or_else(
        || BoxStore::new(&state_dir),
        |storage| storage.boxes().clone(),
    );
    let base_disks = managed_storage.as_ref().map_or_else(
        || BaseDiskStore::new(&cache_dir),
        |storage| storage.base_disks().clone(),
    );
    let disk_tools = DiskToolPaths::discover(args.mke2fs.clone(), args.e2fsck.clone());
    let mke2fs_path = disk_tools.mke2fs_command();
    let backend: Arc<dyn Backend> = match args.backend.as_str() {
        "process" => {
            if args.rootfs.is_some() || args.image.is_some() {
                return Err("--rootfs and --image require --backend libkrun".into());
            }
            Arc::new(ProcessBackend)
        }
        "libkrun" => {
            let storage = managed_storage
                .as_ref()
                .expect("libkrun storage is initialized above");
            let native = NativeSandboxConfig::discover(
                NativeRuntimeOverrides {
                    helper: args.helper,
                    libkrun: args.libkrun,
                    library_search_path: args.lib_dir,
                    gvproxy: args.gvproxy,
                },
                disk_tools,
                args.disk_size,
                args.cpus,
                args.memory_mib,
            );
            let root_path = args
                .rootfs
                .clone()
                .unwrap_or_else(|| cache_dir.join("rootfs"));
            let config = native.libkrun_config(Some(root_path), storage, None)?;
            let runtime = native.box_runtime(storage, None);
            if let Some(rootfs) = args.rootfs {
                let source = native.rootfs_source(rootfs, &platform)?;
                let mut runtime = runtime;
                runtime.source = Some(source);
                Arc::new(LibkrunBackend::new(config).with_box_runtime(runtime))
            } else {
                Arc::new(LazyImageBackend {
                    config,
                    runtime,
                    native,
                    images: images.clone(),
                    reference: args.image,
                    platform: platform.clone(),
                    credentials: credentials.clone(),
                })
            }
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
