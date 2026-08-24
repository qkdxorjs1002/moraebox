use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
#[cfg(unix)]
use std::{io::Write as _, sync::Mutex};

use async_trait::async_trait;
use moraebox_box::{
    BaseDisk, BaseDiskSpec, BaseDiskStore, BoxState, BoxStore, EphemeralDisk, EphemeralDiskStore,
};
#[cfg(unix)]
use moraebox_core::ensure_private_storage_root;
use moraebox_core::{OutputChannel, RunSpec, SessionId, Signal, WorkspaceMode};
use tokio::{
    process::{Child, Command},
    time::Instant,
};
#[cfg(test)]
use tokio::{task::JoinHandle, time::sleep};

use crate::{
    Backend, BackendCapabilities, BackendController, BackendError, CapabilitySupport,
    IsolationLevel, PreparedKey, PreparedPool, RootMode, RunBudget, RunStage, SpawnedSandbox,
    StartupMetrics, doctor::validate_native_runtime_for_spawn, environment::resolve_environment,
};

mod network;
mod root;

use network::NetworkProxy;
#[cfg(test)]
use network::vfkit_socket_uri;
pub(crate) use network::{append_bounded_tail, probe_vfkit_endpoint, stderr_diagnostics};
use root::{ManagedRootLease, PreparedRoot, box_backend_error, repair_dirty_box};

pub type PreparedRootPool = PreparedPool<PreparedKey, EphemeralDisk>;

const NETWORK_PROXY_START_TIMEOUT: Duration = Duration::from_secs(5);
const NETWORK_PROXY_POLL_INTERVAL: Duration = Duration::from_millis(10);
const NETWORK_PROXY_STDERR_FINISH_TIMEOUT: Duration = Duration::from_millis(100);
pub(crate) const NETWORK_PROXY_STDERR_LIMIT: usize = 16 * 1024;

#[derive(Debug, Clone)]
pub struct LibkrunConfig {
    pub helper_path: PathBuf,
    pub library_path: PathBuf,
    pub libkrunfw_path: Option<PathBuf>,
    pub root_path: PathBuf,
    pub root_disk: Option<PathBuf>,
    pub debugfs_path: PathBuf,
    pub library_search_path: Option<PathBuf>,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub workspace_disk: Option<PathBuf>,
    pub gvproxy_path: Option<PathBuf>,
    pub network_runtime_dir: PathBuf,
    pub control_runtime_dir: PathBuf,
    #[cfg(test)]
    enforce_native_preflight: bool,
}

#[derive(Debug, Clone)]
pub struct BoxRootSource {
    pub rootfs_path: PathBuf,
    pub manifest_digest: String,
    pub platform: String,
    pub virtual_size_bytes: u64,
    pub mke2fs_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct BoxRuntimeConfig {
    pub boxes: BoxStore,
    pub base_disks: BaseDiskStore,
    pub ephemeral_disks: EphemeralDiskStore,
    pub source: Option<BoxRootSource>,
    pub e2fsck_path: PathBuf,
}

impl LibkrunConfig {
    pub fn new(
        helper_path: impl Into<PathBuf>,
        library_path: impl Into<PathBuf>,
        root_path: impl Into<PathBuf>,
    ) -> Self {
        let library_path = library_path.into();
        let libkrunfw_path = library_path
            .parent()
            .map(|directory| directory.join("libkrunfw.dylib"))
            .filter(|path| path.is_file());
        Self {
            helper_path: helper_path.into(),
            library_path,
            libkrunfw_path,
            root_path: root_path.into(),
            root_disk: None,
            debugfs_path: PathBuf::from("debugfs"),
            library_search_path: None,
            vcpus: 2,
            memory_mib: 512,
            workspace_disk: None,
            gvproxy_path: None,
            network_runtime_dir: PathBuf::from(".moraebox/network"),
            control_runtime_dir: PathBuf::from(".moraebox/control"),
            #[cfg(test)]
            enforce_native_preflight: false,
        }
    }

    #[must_use]
    pub fn with_root_disk(mut self, root_disk: impl Into<PathBuf>) -> Self {
        self.root_disk = Some(root_disk.into());
        self
    }

    #[must_use]
    pub fn with_libkrunfw(mut self, path: impl Into<PathBuf>) -> Self {
        self.libkrunfw_path = Some(path.into());
        self
    }

    fn validate(&self, managed_root: bool, network: bool) -> Result<(), BackendError> {
        if self.vcpus == 0 {
            return Err(BackendError::InvalidSpec("vCPU count must be non-zero"));
        }
        if self.memory_mib == 0 {
            return Err(BackendError::InvalidSpec("memory must be non-zero"));
        }
        for (name, path) in [
            ("VMM helper", &self.helper_path),
            ("libkrun", &self.library_path),
        ] {
            if !path.exists() {
                return Err(BackendError::Control(format!(
                    "{name} does not exist: {}",
                    path.display()
                )));
            }
        }
        #[cfg(test)]
        let enforce_native_preflight = self.enforce_native_preflight;
        #[cfg(not(test))]
        let enforce_native_preflight = true;
        if enforce_native_preflight {
            validate_native_runtime_for_spawn(
                &self.helper_path,
                &self.library_path,
                self.libkrunfw_path.as_deref(),
                network,
            )
            .map_err(|error| BackendError::Control(error.to_string()))?;
        }
        if managed_root {
            // A managed Box lease supplies the root disk immediately before helper spawn.
        } else if let Some(path) = &self.root_disk {
            if !path.is_file() {
                return Err(BackendError::Control(format!(
                    "root disk does not exist: {}",
                    path.display()
                )));
            }
        } else if !self.root_path.is_dir() {
            return Err(BackendError::Control(format!(
                "root filesystem does not exist: {}",
                self.root_path.display()
            )));
        }
        if (managed_root || self.root_disk.is_some()) && !self.debugfs_path.is_file() {
            return Err(BackendError::Control(format!(
                "debugfs does not exist: {}",
                self.debugfs_path.display()
            )));
        }
        if let Some(path) = self.workspace_disk.as_ref().filter(|path| !path.is_file()) {
            return Err(BackendError::Control(format!(
                "workspace disk does not exist: {}",
                path.display()
            )));
        }
        Ok(())
    }

    fn network_proxy_path(&self) -> Result<&Path, BackendError> {
        let path = self.gvproxy_path.as_deref().ok_or_else(|| {
            BackendError::Control(
                "network access requires --gvproxy, MORAE_GVPROXY_PATH, or gvproxy on PATH".into(),
            )
        })?;
        if !path.is_file() {
            return Err(BackendError::Control(format!(
                "gvproxy does not exist: {}",
                path.display()
            )));
        }
        Ok(path)
    }
}

#[derive(Debug, Clone)]
pub struct LibkrunBackend {
    config: LibkrunConfig,
    box_runtime: Option<BoxRuntimeConfig>,
    prepared_roots: Option<Arc<PreparedRootPool>>,
    workspace_digest: Option<String>,
}

impl LibkrunBackend {
    pub const CAPABILITIES: BackendCapabilities = BackendCapabilities {
        isolation: IsolationLevel::MicroVm,
        tty: CapabilitySupport::Supported,
        network: CapabilitySupport::Supported,
        box_persistence: CapabilitySupport::Supported,
        workspace: CapabilitySupport::Supported,
        file_transfer: CapabilitySupport::Supported,
    };
}

impl LibkrunBackend {
    pub fn new(config: LibkrunConfig) -> Self {
        Self {
            config,
            box_runtime: None,
            prepared_roots: None,
            workspace_digest: None,
        }
    }

    #[must_use]
    pub fn with_box_runtime(mut self, config: BoxRuntimeConfig) -> Self {
        self.box_runtime = Some(config);
        self
    }

    /// Enables single-use prepared ephemeral root artifacts for long-lived backends.
    ///
    /// A leased artifact is consumed by exactly one helper invocation and is never returned to
    /// the pool. Persistent Boxes and unmanaged directory/static-disk roots bypass this pool.
    #[must_use]
    pub fn with_prepared_pool(mut self, pool: Arc<PreparedRootPool>) -> Self {
        self.prepared_roots = Some(pool);
        self
    }

    /// Adds the immutable workspace image identity to prepared artifact pool keys.
    #[must_use]
    pub fn with_workspace_digest(mut self, digest: impl Into<String>) -> Self {
        self.workspace_digest = Some(digest.into());
        self
    }
}

#[async_trait]
impl Backend for LibkrunBackend {
    fn name(&self) -> &'static str {
        "libkrun"
    }

    fn capabilities(&self) -> BackendCapabilities {
        Self::CAPABILITIES
    }

    #[expect(
        clippy::too_many_lines,
        reason = "native spawn preserves the ordered preparation and cleanup boundary"
    )]
    async fn spawn(
        &self,
        spec: &RunSpec,
        budget: &RunBudget,
    ) -> Result<SpawnedSandbox, BackendError> {
        spec.validate().map_err(BackendError::InvalidSpec)?;
        self.config
            .validate(self.box_runtime.is_some(), spec.network)?;
        if spec.box_id.is_some() && self.box_runtime.is_none() {
            return Err(BackendError::Control(
                "run requested a BoxId but no Box store is configured".into(),
            ));
        }
        if (!spec.copy_in.is_empty() || !spec.copy_out.is_empty())
            && self.box_runtime.is_none()
            && self.config.root_disk.is_none()
        {
            return Err(BackendError::Unsupported(
                "copy-in/out with a directory root; use a managed or explicit root disk",
            ));
        }
        if self.config.workspace_disk.is_some()
            && self.box_runtime.is_none()
            && self.config.root_disk.is_none()
        {
            return Err(BackendError::Unsupported(
                "workspace mounting with a directory root; use a managed or explicit root disk",
            ));
        }
        if spec.workspace_mode == WorkspaceMode::Overlay && self.config.workspace_disk.is_none() {
            return Err(BackendError::Unsupported(
                "writable workspace overlay without an immutable workspace disk",
            ));
        }
        let environment = resolve_environment(spec)?;

        let mut startup = StartupMetrics::default();
        let root_started = Instant::now();
        let (prepared_root, root_startup) = match budget
            .run(RunStage::RootPrepare, self.prepare_root(spec, budget))
            .await
        {
            Ok(prepared) => prepared,
            Err(crate::StageError::Timeout(error)) => {
                return Err(BackendError::Timeout {
                    stage: error.stage,
                    limit: error.limit,
                });
            }
            Err(crate::StageError::Failed { source, .. }) => return Err(source),
        };
        startup.root_prepare_micros = Some(elapsed_micros(root_started));
        startup.root_mode = root_startup.root_mode;
        startup.prepared_pool_hit = root_startup.prepared_pool_hit;
        startup.prepared_lease_micros = root_startup.prepared_lease_micros;
        startup.cache_lookup_micros = root_startup.cache_lookup_micros;
        startup.box_lock_micros = root_startup.box_lock_micros;
        startup.base_prepare_micros = root_startup.base_prepare_micros;
        startup.disk_clone_micros = root_startup.disk_clone_micros;
        startup.repair_micros = root_startup.repair_micros;

        let network_proxy = if spec.network {
            let started = Instant::now();
            let proxy = budget
                .run(RunStage::NetworkSetup, NetworkProxy::start(&self.config))
                .await
                .map_err(BackendError::from)?;
            startup.network_setup_micros = Some(elapsed_micros(started));
            Some(proxy)
        } else {
            None
        };

        let mut command = Command::new(&self.config.helper_path);
        command.arg("--libkrun").arg(&self.config.library_path);
        if let Some(root_disk) = prepared_root.disk_path() {
            command
                .arg("--root-disk")
                .arg(root_disk)
                .arg("--debugfs")
                .arg(&self.config.debugfs_path)
                .arg("--session-id")
                .arg(spec.session_id.to_string());
        } else {
            command.arg("--root").arg(&self.config.root_path);
        }
        command
            .arg("--cpus")
            .arg(self.config.vcpus.to_string())
            .arg("--memory-mib")
            .arg(self.config.memory_mib.to_string())
            .arg("--parent-pid")
            .arg(std::process::id().to_string());
        if let Some(cwd) = &spec.cwd {
            command.arg("--cwd").arg(cwd);
        }
        if spec.tty {
            command
                .arg("--tty")
                .arg("--tty-rows")
                .arg(spec.tty_rows.to_string())
                .arg("--tty-cols")
                .arg(spec.tty_columns.to_string());
        }
        if let Some(workspace) = &self.config.workspace_disk {
            command.arg("--workspace-disk").arg(workspace);
            if spec.workspace_mode == WorkspaceMode::Overlay {
                command.arg("--workspace-writable");
            }
        }
        if let Some(proxy) = &network_proxy {
            command.arg("--network-socket").arg(&proxy.socket_path);
        }
        for copy in &spec.copy_in {
            command
                .arg("--copy-in-source")
                .arg(&copy.source)
                .arg("--copy-in-destination")
                .arg(&copy.destination);
        }
        for copy in &spec.copy_out {
            command
                .arg("--copy-out-source")
                .arg(&copy.source)
                .arg("--copy-out-destination")
                .arg(&copy.destination);
        }
        if !spec.copy_in.is_empty() || !spec.copy_out.is_empty() {
            command
                .arg("--copy-limit-bytes")
                .arg(spec.copy_limit_bytes.to_string());
        }
        for (key, value) in environment {
            command.arg("--env").arg(format!("{key}={value}"));
        }
        command.env_clear();
        if let Some(path) = &self.config.library_search_path {
            #[cfg(target_os = "macos")]
            command.env("DYLD_LIBRARY_PATH", path);
            #[cfg(not(target_os = "macos"))]
            command.env("LD_LIBRARY_PATH", path);
        }
        command.kill_on_drop(true);

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }

        let helper_started = Instant::now();
        let mut spawned = Self::spawn_helper(
            command,
            spec,
            prepared_root,
            network_proxy,
            &self.config.control_runtime_dir,
            budget,
        )
        .await?;
        startup.helper_spawn_micros = Some(elapsed_micros(helper_started));
        spawned.startup = startup;
        Ok(spawned)
    }
}

impl LibkrunBackend {
    async fn spawn_helper(
        mut command: Command,
        spec: &RunSpec,
        prepared_root: PreparedRoot,
        mut network_proxy: Option<NetworkProxy>,
        control_runtime_dir: &Path,
        budget: &RunBudget,
    ) -> Result<SpawnedSandbox, BackendError> {
        let spawn_result = budget.run_sync(RunStage::HelperSpawn, || {
            let controlled = prepared_root.disk_path().is_some();
            let root_lease = prepared_root.into_lease();
            if controlled {
                let control = HostControlPipe::new(control_runtime_dir)?;
                command.arg("--control-fifo").arg(control.path());
                command.arg("--").args(&spec.argv);
                spawn_piped(
                    command,
                    if spec.tty {
                        OutputChannel::Tty
                    } else {
                        OutputChannel::Stdout
                    },
                    Some(NativeControl::Protocol(control)),
                    &mut network_proxy,
                    root_lease,
                )
            } else if spec.tty {
                command.arg("--").args(&spec.argv);
                spawn_pty(command, spec, &mut network_proxy, root_lease)
            } else {
                command.arg("--").args(&spec.argv);
                spawn_piped(
                    command,
                    OutputChannel::Stdout,
                    None,
                    &mut network_proxy,
                    root_lease,
                )
            }
        });
        match spawn_result {
            Ok(spawned) => Ok(spawned),
            Err(error) => {
                let mut error = BackendError::from(error);
                let cleanup = if let Some(proxy) = network_proxy.take() {
                    proxy.stop().await.err()
                } else {
                    None
                };
                if let Some(cleanup) = cleanup {
                    error = BackendError::Control(format!(
                        "{error}; failed to reap gvproxy after helper spawn failure: {cleanup}"
                    ));
                }
                Err(error)
            }
        }
    }

    async fn prepare_root(
        &self,
        spec: &RunSpec,
        budget: &RunBudget,
    ) -> Result<(PreparedRoot, StartupMetrics), BackendError> {
        let Some(runtime) = &self.box_runtime else {
            return Ok(self.config.root_disk.as_ref().map_or_else(
                || {
                    (
                        PreparedRoot::Directory,
                        StartupMetrics {
                            root_mode: Some(RootMode::Directory),
                            ..StartupMetrics::default()
                        },
                    )
                },
                |path| {
                    (
                        PreparedRoot::StaticDisk(path.clone()),
                        StartupMetrics {
                            root_mode: Some(RootMode::StaticDisk),
                            ..StartupMetrics::default()
                        },
                    )
                },
            ));
        };

        if let Some(box_id) = spec.box_id {
            return Self::prepare_persistent_root(runtime, box_id, budget).await;
        }
        self.prepare_ephemeral_root(runtime, spec, budget).await
    }

    async fn prepare_persistent_root(
        runtime: &BoxRuntimeConfig,
        box_id: moraebox_core::BoxId,
        budget: &RunBudget,
    ) -> Result<(PreparedRoot, StartupMetrics), BackendError> {
        let lock_started = Instant::now();
        let mut lease = budget
            .run_sync(RunStage::BoxLock, || runtime.boxes.try_acquire(box_id))
            .map_err(BackendError::from)?;
        let mut startup = StartupMetrics {
            root_mode: Some(RootMode::Persistent),
            box_lock_micros: Some(elapsed_micros(lock_started)),
            ..StartupMetrics::default()
        };
        if lease.metadata().state == BoxState::Dirty {
            let repair_started = Instant::now();
            budget
                .run(
                    RunStage::BoxRepair,
                    repair_dirty_box(&runtime.boxes, &mut lease, &runtime.e2fsck_path),
                )
                .await
                .map_err(BackendError::from)?;
            startup.repair_micros = Some(elapsed_micros(repair_started));
        }
        runtime
            .boxes
            .begin_writable_use(&mut lease)
            .map_err(box_backend_error)?;
        Ok((
            PreparedRoot::Managed(ManagedRootLease::Persistent {
                store: runtime.boxes.clone(),
                lease,
            }),
            startup,
        ))
    }

    async fn prepare_ephemeral_root(
        &self,
        runtime: &BoxRuntimeConfig,
        spec: &RunSpec,
        budget: &RunBudget,
    ) -> Result<(PreparedRoot, StartupMetrics), BackendError> {
        let source = runtime.source.clone().ok_or_else(|| {
            BackendError::Control(
                "ephemeral native run requires an image-backed Box root source".into(),
            )
        })?;
        let base_store = runtime.base_disks.clone();
        let base_spec = BaseDiskSpec::new(
            source.manifest_digest,
            source.platform,
            source.virtual_size_bytes,
        );
        let prepared_key = PreparedKey {
            image_digest: base_spec.manifest_digest.clone(),
            workspace_digest: self.workspace_digest.clone(),
            policy_digest: base_spec.key().map_err(box_backend_error)?,
        };
        let lookup_store = base_store.clone();
        let lookup_spec = base_spec.clone();
        let lookup_started = Instant::now();
        let cached = budget
            .run(RunStage::CacheLookup, async move {
                tokio::task::spawn_blocking(move || lookup_store.get(&lookup_spec))
                    .await
                    .map_err(|error| {
                        BackendError::Control(format!("base disk task failed: {error}"))
                    })?
                    .map_err(box_backend_error)
            })
            .await
            .map_err(BackendError::from)?;
        let mut startup = StartupMetrics {
            root_mode: Some(RootMode::Ephemeral),
            cache_lookup_micros: Some(elapsed_micros(lookup_started)),
            ..StartupMetrics::default()
        };
        let base = if let Some(base) = cached {
            base
        } else {
            let rootfs = source.rootfs_path;
            let mke2fs = source.mke2fs_path;
            let prepare_started = Instant::now();
            let base = budget
                .run(RunStage::BaseDiskPrepare, async move {
                    tokio::task::spawn_blocking(move || {
                        base_store.prepare(&base_spec, &rootfs, &mke2fs)
                    })
                    .await
                    .map_err(|error| {
                        BackendError::Control(format!("base disk task failed: {error}"))
                    })?
                    .map_err(box_backend_error)
                })
                .await
                .map_err(BackendError::from)?;
            startup.base_prepare_micros = Some(elapsed_micros(prepare_started));
            base
        };
        let ephemeral_store = runtime.ephemeral_disks.clone();
        let lease_started = Instant::now();
        let prepared = if let Some(pool) = &self.prepared_roots {
            let lease = pool.lease(&prepared_key).await;
            startup.prepared_pool_hit = Some(lease.is_some());
            startup.prepared_lease_micros = Some(elapsed_micros(lease_started));
            lease.map(crate::PreparedLease::into_inner)
        } else {
            None
        };
        let disk = if let Some(disk) = prepared {
            disk
        } else {
            let clone_store = ephemeral_store.clone();
            let clone_base = base.clone();
            let session_id = spec.session_id;
            let clone_started = Instant::now();
            let disk = budget
                .run(RunStage::EphemeralDiskClone, async move {
                    clone_ephemeral_disk(clone_store, clone_base, session_id).await
                })
                .await
                .map_err(BackendError::from)?;
            startup.disk_clone_micros = Some(elapsed_micros(clone_started));
            disk
        };
        if let Some(pool) = &self.prepared_roots {
            replenish_prepared_root(pool, prepared_key, ephemeral_store, base);
        }
        Ok((
            PreparedRoot::Managed(ManagedRootLease::Ephemeral(disk)),
            startup,
        ))
    }
}

async fn clone_ephemeral_disk(
    store: EphemeralDiskStore,
    base: BaseDisk,
    session_id: SessionId,
) -> Result<EphemeralDisk, BackendError> {
    tokio::task::spawn_blocking(move || store.clone_for_session(&base, session_id))
        .await
        .map_err(|error| BackendError::Control(format!("CoW clone task failed: {error}")))?
        .map_err(box_backend_error)
}

fn replenish_prepared_root(
    pool: &Arc<PreparedRootPool>,
    key: PreparedKey,
    store: EphemeralDiskStore,
    base: BaseDisk,
) {
    let pool = Arc::downgrade(pool);
    tokio::spawn(async move {
        let Some(pool) = pool.upgrade() else {
            return;
        };
        let _ = pool
            .replenish(key, 1, || {
                clone_ephemeral_disk(store.clone(), base.clone(), SessionId::new())
            })
            .await;
    });
}

#[cfg(unix)]
const CONTROL_MESSAGE_BYTES: usize = 5;
#[cfg(unix)]
const CONTROL_RESIZE: u8 = 1;
#[cfg(unix)]
const CONTROL_INTERRUPT: u8 = 2;
#[cfg(unix)]
const CONTROL_TERMINATE: u8 = 3;
#[cfg(unix)]
const CONTROL_HANGUP: u8 = 4;

#[cfg(unix)]
#[derive(Debug)]
struct HostControlPipe {
    path: PathBuf,
    writer: Mutex<std::fs::File>,
}

#[cfg(unix)]
impl HostControlPipe {
    fn new(runtime_root: &Path) -> Result<Self, BackendError> {
        use std::os::unix::fs::OpenOptionsExt as _;

        use nix::{fcntl::OFlag, sys::stat::Mode, unistd::mkfifo};

        ensure_private_storage_root(runtime_root)
            .map_err(|error| BackendError::Control(error.to_string()))?;
        let runtime_root = std::fs::canonicalize(runtime_root)?;
        let path = runtime_root.join(format!("run-{}.fifo", SessionId::new()));
        mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).map_err(std::io::Error::from)?;
        let writer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(OFlag::O_NONBLOCK.bits())
            .open(&path)
            .inspect_err(|_| {
                let _ = std::fs::remove_file(&path);
            })?;
        Ok(Self {
            path,
            writer: Mutex::new(writer),
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn resize(&self, rows: u16, cols: u16) -> Result<(), BackendError> {
        if rows == 0 || cols == 0 {
            return Err(BackendError::InvalidSpec(
                "terminal rows and columns must be non-zero",
            ));
        }
        let mut message = [0_u8; CONTROL_MESSAGE_BYTES];
        message[0] = CONTROL_RESIZE;
        message[1..3].copy_from_slice(&rows.to_be_bytes());
        message[3..].copy_from_slice(&cols.to_be_bytes());
        self.write(message)
    }

    fn signal(&self, signal: Signal) -> Result<(), BackendError> {
        let opcode = match signal {
            Signal::Interrupt => CONTROL_INTERRUPT,
            Signal::Terminate => CONTROL_TERMINATE,
            Signal::Hangup => CONTROL_HANGUP,
            Signal::Kill => {
                return Err(BackendError::Control(
                    "kill cannot be sent through the guest control pipe".into(),
                ));
            }
        };
        self.write([opcode, 0, 0, 0, 0])
    }

    fn write(&self, message: [u8; CONTROL_MESSAGE_BYTES]) -> Result<(), BackendError> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|error| BackendError::Control(error.to_string()))?;
        writer.write_all(&message)?;
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for HostControlPipe {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(not(unix))]
#[derive(Debug)]
struct HostControlPipe;

#[cfg(not(unix))]
impl HostControlPipe {
    fn new(_runtime_root: &Path) -> Result<Self, BackendError> {
        Err(BackendError::Unsupported(
            "native guest control on this platform",
        ))
    }

    fn path(&self) -> &Path {
        unreachable!("unsupported native guest control pipe")
    }
}

#[derive(Debug)]
enum NativeControl {
    #[cfg(unix)]
    Pty(Arc<std::fs::File>),
    Protocol(HostControlPipe),
}

fn spawn_piped(
    mut command: Command,
    stdout_channel: OutputChannel,
    control: Option<NativeControl>,
    network_proxy: &mut Option<NetworkProxy>,
    root_lease: Option<ManagedRootLease>,
) -> Result<SpawnedSandbox, BackendError> {
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let pid = child.id().ok_or(BackendError::MissingProcessId)?;
    let stdin = child.stdin.take().map(|writer| Box::pin(writer) as _);
    let stdout = child
        .stdout
        .take()
        .map(|reader| Box::pin(reader) as _)
        .ok_or_else(|| BackendError::Control("helper stdout pipe was not created".into()))?;
    let stderr = child.stderr.take().map(|reader| Box::pin(reader) as _);
    let exit = managed_exit(child, network_proxy.take(), root_lease);
    Ok(SpawnedSandbox {
        stdin,
        stdout,
        stdout_channel,
        stderr,
        exit,
        controller: Box::new(LibkrunController {
            pid: Arc::new(pid),
            control,
        }),
        startup: StartupMetrics::default(),
    })
}

#[cfg(unix)]
#[derive(Debug)]
struct PtyInput {
    file: tokio::fs::File,
    eof: [u8; 2],
    eof_offset: usize,
    shutting_down: bool,
}

#[cfg(unix)]
impl PtyInput {
    fn new(file: std::fs::File, eof: u8) -> Self {
        Self {
            file: tokio::fs::File::from_std(file),
            eof: [eof; 2],
            eof_offset: 0,
            shutting_down: false,
        }
    }
}

#[cfg(unix)]
impl tokio::io::AsyncWrite for PtyInput {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        if self.shutting_down {
            return std::task::Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "terminal input is closed",
            )));
        }
        std::pin::Pin::new(&mut self.file).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.file).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        self.shutting_down = true;
        while self.eof_offset < self.eof.len() {
            let offset = self.eof_offset;
            let eof = self.eof;
            match std::pin::Pin::new(&mut self.file).poll_write(context, &eof[offset..]) {
                std::task::Poll::Ready(Ok(0)) => {
                    return std::task::Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write terminal EOF",
                    )));
                }
                std::task::Poll::Ready(Ok(count)) => self.eof_offset += count,
                std::task::Poll::Ready(Err(error)) => return std::task::Poll::Ready(Err(error)),
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
        std::pin::Pin::new(&mut self.file).poll_shutdown(context)
    }
}

#[cfg(unix)]
fn spawn_pty(
    mut command: Command,
    spec: &RunSpec,
    network_proxy: &mut Option<NetworkProxy>,
    root_lease: Option<ManagedRootLease>,
) -> Result<SpawnedSandbox, BackendError> {
    use std::fs::File;

    use nix::pty::{Winsize, openpty};

    let window = Winsize {
        ws_row: spec.tty_rows,
        ws_col: spec.tty_columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = openpty(&window, None).map_err(std::io::Error::from)?;
    let master = File::from(pty.master);
    let terminal = nix::sys::termios::tcgetattr(&master).map_err(std::io::Error::from)?;
    let eof = terminal.control_chars[nix::sys::termios::SpecialCharacterIndices::VEOF as usize];
    let master_reader = master.try_clone()?;
    let controller_master = master.try_clone()?;
    let slave = File::from(pty.slave);
    command.stdin(Stdio::from(slave.try_clone()?));
    command.stdout(Stdio::from(slave.try_clone()?));
    command.stderr(Stdio::from(slave));

    let child = command.spawn()?;
    let pid = child.id().ok_or(BackendError::MissingProcessId)?;
    let exit = managed_exit(child, network_proxy.take(), root_lease);
    Ok(SpawnedSandbox {
        stdin: Some(Box::pin(PtyInput::new(master, eof))),
        stdout: Box::pin(tokio::fs::File::from_std(master_reader)),
        stdout_channel: OutputChannel::Tty,
        stderr: None,
        exit,
        controller: Box::new(LibkrunController {
            pid: Arc::new(pid),
            control: Some(NativeControl::Pty(Arc::new(controller_master))),
        }),
        startup: StartupMetrics::default(),
    })
}

#[cfg(not(unix))]
fn spawn_pty(
    _command: Command,
    _spec: &RunSpec,
    _network_proxy: &mut Option<NetworkProxy>,
    _root_lease: Option<ManagedRootLease>,
) -> Result<SpawnedSandbox, BackendError> {
    Err(BackendError::Unsupported("PTY on this platform"))
}

fn managed_exit(
    mut child: Child,
    network_proxy: Option<NetworkProxy>,
    mut root_lease: Option<ManagedRootLease>,
) -> crate::backend::ExitFuture {
    let task = tokio::spawn(async move {
        let status = child.wait().await;
        let root_cleanup = match (status.as_ref(), root_lease.as_mut()) {
            (Ok(status), Some(lease)) if helper_exit_is_clean(*status) => lease
                .mark_clean()
                .map_err(|error| io::Error::other(error.to_string())),
            _ => Ok(()),
        };
        let network_cleanup = if let Some(proxy) = network_proxy {
            proxy.stop().await
        } else {
            Ok(())
        };
        root_cleanup?;
        if status.is_ok() {
            network_cleanup?;
        }
        status
    });
    Box::pin(async move {
        task.await
            .map_err(|error| io::Error::other(format!("helper wait task failed: {error}")))?
    })
}

fn helper_exit_is_clean(status: std::process::ExitStatus) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal().is_none() && !matches!(status.code(), Some(125 | 137) | None)
    }
    #[cfg(not(unix))]
    {
        !matches!(status.code(), Some(125 | 137) | None)
    }
}

fn elapsed_micros(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

#[derive(Debug)]
struct LibkrunController {
    #[cfg_attr(
        not(unix),
        expect(
            dead_code,
            reason = "libkrun helper signals are unsupported on this platform"
        )
    )]
    pid: Arc<u32>,
    #[cfg_attr(
        not(unix),
        expect(
            dead_code,
            reason = "terminal resizing is unsupported by the non-Unix native stub"
        )
    )]
    control: Option<NativeControl>,
}

impl Drop for LibkrunController {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use nix::{sys::signal::Signal as NixSignal, unistd::Pid};

            if let Ok(pid) = i32::try_from(*self.pid) {
                let _ = nix::sys::signal::kill(Pid::from_raw(-pid), NixSignal::SIGKILL);
            }
        }
    }
}

#[async_trait]
impl BackendController for LibkrunController {
    async fn signal(&self, signal: Signal) -> Result<(), BackendError> {
        #[cfg(unix)]
        {
            use nix::{
                errno::Errno,
                sys::signal::{Signal as NixSignal, kill},
                unistd::Pid,
            };

            if let Some(NativeControl::Protocol(control)) =
                self.control.as_ref().filter(|_| signal != Signal::Kill)
            {
                return control.signal(signal);
            }
            let raw_pid = i32::try_from(*self.pid)
                .map_err(|error| BackendError::Control(error.to_string()))?;
            let native_signal = match signal {
                Signal::Interrupt => NixSignal::SIGINT,
                Signal::Terminate => NixSignal::SIGTERM,
                Signal::Kill => NixSignal::SIGKILL,
                Signal::Hangup => NixSignal::SIGHUP,
            };
            match kill(Pid::from_raw(-raw_pid), native_signal) {
                Ok(()) | Err(Errno::ESRCH) => Ok(()),
                Err(error) => Err(BackendError::Control(error.to_string())),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = signal;
            Err(BackendError::Unsupported(
                "libkrun helper signals on this platform",
            ))
        }
    }

    async fn resize(&self, rows: u16, cols: u16) -> Result<(), BackendError> {
        #[cfg(unix)]
        {
            use nix::{
                errno::Errno,
                sys::signal::{Signal as NixSignal, kill},
                unistd::Pid,
            };
            let control = self
                .control
                .as_ref()
                .ok_or(BackendError::Unsupported("terminal resize"))?;
            match control {
                NativeControl::Protocol(control) => return control.resize(rows, cols),
                NativeControl::Pty(terminal) => {
                    use rustix::termios::{Winsize, tcsetwinsize};

                    tcsetwinsize(
                        terminal.as_ref(),
                        Winsize {
                            ws_row: rows,
                            ws_col: cols,
                            ws_xpixel: 0,
                            ws_ypixel: 0,
                        },
                    )
                    .map_err(|error| BackendError::Control(error.to_string()))?;
                }
            }
            let raw_pid = i32::try_from(*self.pid)
                .map_err(|error| BackendError::Control(error.to_string()))?;
            match kill(Pid::from_raw(-raw_pid), NixSignal::SIGWINCH) {
                Ok(()) | Err(Errno::ESRCH) => Ok(()),
                Err(error) => Err(BackendError::Control(error.to_string())),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (rows, cols);
            Err(BackendError::Unsupported("terminal resize"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use moraebox_box::CreateBox;

    const MANAGED_TEST_DISK_BYTES: u64 = 8 * 1024 * 1024;

    #[cfg(unix)]
    use std::{fs, os::unix::fs::PermissionsExt};

    fn cow_clone_unavailable(error: &impl std::fmt::Display) -> bool {
        error
            .to_string()
            .contains("copy-on-write cloning is unavailable")
    }

    #[test]
    fn rejects_missing_native_paths() {
        let config = LibkrunConfig::new("missing-helper", "missing-lib", "missing-root");
        assert!(config.validate(false, false).is_err());
    }

    #[test]
    fn gvproxy_socket_uri_requires_an_absolute_path() {
        assert!(vfkit_socket_uri(Path::new("relative/gvproxy.sock")).is_err());
        #[cfg(unix)]
        {
            assert_eq!(
                vfkit_socket_uri(Path::new("/private/tmp/gvproxy.sock")).unwrap(),
                "unixgram:///private/tmp/gvproxy.sock"
            );
            let overlong = PathBuf::from(format!("/private/tmp/{}", "x".repeat(104)));
            assert!(vfkit_socket_uri(&overlong).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn vfkit_probe_requires_a_bound_datagram_endpoint() {
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("gvproxy.sock");
        fs::write(&path, []).unwrap();
        assert!(probe_vfkit_endpoint(&path).is_err());

        fs::remove_file(&path).unwrap();
        let _listener = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
        probe_vfkit_endpoint(&path).unwrap();
    }

    #[test]
    fn stderr_tail_never_exceeds_its_byte_limit() {
        let mut retained = b"old".to_vec();
        append_bounded_tail(&mut retained, b"0123456789", 6);
        assert_eq!(retained, b"456789");

        append_bounded_tail(&mut retained, b"ab", 6);
        assert_eq!(retained, b"6789ab");
    }

    #[cfg(unix)]
    #[test]
    fn host_control_pipe_frames_resize_and_signals() {
        use std::io::Read as _;

        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("control");
        let control = HostControlPipe::new(&runtime).unwrap();
        let path = control.path().to_owned();
        let mut reader = fs::File::open(control.path()).unwrap();

        control.resize(41, 99).unwrap();
        let mut message = [0_u8; CONTROL_MESSAGE_BYTES];
        reader.read_exact(&mut message).unwrap();
        assert_eq!(message, [CONTROL_RESIZE, 0, 41, 0, 99]);

        control.signal(Signal::Terminate).unwrap();
        reader.read_exact(&mut message).unwrap();
        assert_eq!(message, [CONTROL_TERMINATE, 0, 0, 0, 0]);
        assert!(control.resize(0, 99).is_err());
        drop(control);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn controlled_terminal_uses_pipes_and_preserves_eof() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let state = tempfile::tempdir().unwrap();
        let helper = state.path().join("helper");
        let library = state.path().join("libkrun");
        let root_disk = state.path().join("root.ext4");
        let debugfs = state.path().join("debugfs");
        let control_runtime = state.path().join("control");
        write_executable(
            &helper,
            "#!/bin/sh\ncontrol=\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = -- ]; then shift; break; fi\n  if [ \"$1\" = --control-fifo ]; then control=$2; shift 2; continue; fi\n  shift\ndone\n[ -p \"$control\" ] || exit 90\ncat\n",
        );
        write_executable(&debugfs, "#!/bin/sh\nexit 0\n");
        fs::write(&library, []).unwrap();
        fs::write(&root_disk, []).unwrap();
        let mut config = LibkrunConfig::new(&helper, &library, state.path().join("unused"))
            .with_root_disk(&root_disk);
        config.debugfs_path = debugfs;
        config.control_runtime_dir.clone_from(&control_runtime);
        let mut spec = RunSpec::command(["/bin/cat"]);
        spec.tty = true;

        let mut spawned = LibkrunBackend::new(config)
            .spawn(&spec, &RunBudget::new(spec.timeout))
            .await
            .unwrap();
        assert_eq!(spawned.stdout_channel, OutputChannel::Tty);
        let mut input = spawned.stdin.take().unwrap();
        input.write_all(b"pty-eof-probe\n").await.unwrap();
        input.shutdown().await.unwrap();
        drop(input);

        let mut output = Vec::new();
        spawned.stdout.read_to_end(&mut output).await.unwrap();
        assert!(spawned.exit.await.unwrap().success());
        drop(spawned.controller);

        assert_eq!(output, b"pty-eof-probe\n");
        assert_eq!(fs::read_dir(control_runtime).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn gvproxy_early_exit_reports_only_bounded_stderr_tail() {
        let state = tempfile::tempdir().unwrap();
        let helper = state.path().join("helper");
        let gvproxy = state.path().join("gvproxy");
        let library = state.path().join("libkrun");
        let root = state.path().join("root");
        write_executable(&helper, "#!/bin/sh\nexit 0\n");
        write_executable(
            &gvproxy,
            "#!/bin/sh\ndd if=/dev/zero bs=32768 count=1 2>/dev/null >&2\nprintf diagnostic-tail >&2\nexit 7\n",
        );
        fs::write(&library, []).unwrap();
        fs::create_dir(&root).unwrap();
        let mut config = LibkrunConfig::new(helper, library, root);
        config.gvproxy_path = Some(gvproxy.clone());
        config.network_runtime_dir = state.path().join("network");
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.network = true;

        let result = LibkrunBackend::new(config)
            .spawn(&spec, &RunBudget::new(spec.timeout))
            .await;
        let Err(error) = result else {
            panic!("expected gvproxy startup to fail");
        };
        let BackendError::Control(message) = error else {
            panic!("expected a control error");
        };
        assert!(message.contains("diagnostic-tail"));
        assert!(message.contains("exit status: 7"));
        assert!(message.len() <= NETWORK_PROXY_STDERR_LIMIT.saturating_mul(3));
    }

    #[tokio::test]
    async fn network_requires_gvproxy() {
        let state = tempfile::tempdir().unwrap();
        let helper = state.path().join("helper");
        let library = state.path().join("libkrun");
        let root = state.path().join("root");
        std::fs::write(&helper, []).unwrap();
        std::fs::write(&library, []).unwrap();
        std::fs::create_dir(&root).unwrap();
        let config = LibkrunConfig::new(helper, library, root);
        let mut spec = RunSpec::command(["true"]);
        spec.network = true;

        let error = LibkrunBackend::new(config)
            .spawn(&spec, &RunBudget::new(spec.timeout))
            .await;
        assert!(
            matches!(error, Err(BackendError::Control(message)) if message.contains("gvproxy"))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn network_off_does_not_start_or_require_a_proxy() {
        let state = tempfile::tempdir().unwrap();
        let helper = state.path().join("helper");
        let library = state.path().join("libkrun");
        let root = state.path().join("root");
        let network_runtime_dir = state.path().join(".moraebox/network");
        write_executable(&helper, "#!/bin/sh\nprintf 'network-off\\n'\n");
        fs::write(&library, []).unwrap();
        fs::create_dir(&root).unwrap();

        let mut config = LibkrunConfig::new(helper, library, root);
        config.network_runtime_dir = network_runtime_dir.clone();
        let report = crate::Supervisor::new(LibkrunBackend::new(config))
            .run(RunSpec::command(["/usr/bin/true"]))
            .await
            .unwrap();

        assert_eq!(report.exit_code, Some(0));
        assert!(!network_runtime_dir.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn inherited_environment_is_forwarded_to_the_guest_not_the_helper() {
        let state = tempfile::tempdir().unwrap();
        let helper = state.path().join("helper");
        let library = state.path().join("libkrun");
        let root = state.path().join("root");
        write_executable(
            &helper,
            "#!/bin/sh\nfor arg in \"$@\"; do case \"$arg\" in PATH=*) printf path-forwarded;; esac; done\n",
        );
        fs::write(&library, []).unwrap();
        fs::create_dir(&root).unwrap();
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.inherit_env = true;

        let report = crate::Supervisor::new(LibkrunBackend::new(LibkrunConfig::new(
            helper, library, root,
        )))
        .run(spec)
        .await
        .unwrap();
        let output = report
            .output
            .into_iter()
            .flat_map(|chunk| chunk.data)
            .collect::<Vec<_>>();

        assert_eq!(output, b"path-forwarded");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn passes_a_block_root_to_the_helper() {
        let state = tempfile::tempdir().unwrap();
        let helper = state.path().join("helper");
        let library = state.path().join("libkrun");
        let root = state.path().join("unused-root");
        let root_disk = state.path().join("root.ext4");
        let debugfs = state.path().join("debugfs");
        write_executable(&helper, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n");
        write_executable(&debugfs, "#!/bin/sh\nexit 0\n");
        fs::write(&library, []).unwrap();
        fs::write(&root_disk, []).unwrap();

        let mut config = LibkrunConfig::new(helper, library, root).with_root_disk(&root_disk);
        config.debugfs_path = debugfs.clone();
        let report = crate::Supervisor::new(LibkrunBackend::new(config))
            .run(RunSpec::command(["/usr/bin/true"]))
            .await
            .unwrap();
        let output = report
            .output
            .iter()
            .flat_map(|chunk| chunk.data.iter().copied())
            .collect::<Vec<_>>();
        let output = String::from_utf8(output).unwrap();

        assert!(output.contains("--root-disk"));
        assert!(output.contains(root_disk.to_str().unwrap()));
        assert!(output.contains("--debugfs"));
        assert!(output.contains(debugfs.to_str().unwrap()));
        assert!(output.contains("--session-id"));
        assert!(!output.contains("--root\n"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn passes_bounded_copy_requests_to_the_protocol_helper() {
        let state = tempfile::tempdir().unwrap();
        let helper = state.path().join("helper");
        let library = state.path().join("libkrun");
        let root_disk = state.path().join("root.ext4");
        let debugfs = state.path().join("debugfs");
        let workspace = state.path().join("workspace.ext4");
        let source = state.path().join("input");
        let destination = state.path().join("output");
        write_executable(&helper, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n");
        write_executable(&debugfs, "#!/bin/sh\nexit 0\n");
        fs::write(&library, []).unwrap();
        fs::write(&root_disk, []).unwrap();
        fs::write(&workspace, []).unwrap();
        fs::write(&source, b"input").unwrap();
        let mut config = LibkrunConfig::new(&helper, &library, state.path().join("unused"))
            .with_root_disk(&root_disk);
        config.debugfs_path = debugfs;
        config.workspace_disk = Some(workspace.clone());
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.copy_in.push(moraebox_core::CopyInSpec {
            source: source.clone(),
            destination: "/workspace/input".into(),
        });
        spec.copy_out.push(moraebox_core::CopyOutSpec {
            source: "/workspace/output".into(),
            destination: destination.clone(),
        });
        spec.copy_limit_bytes = 4096;
        spec.workspace_mode = WorkspaceMode::Overlay;

        let report = crate::Supervisor::new(LibkrunBackend::new(config))
            .run(spec)
            .await
            .unwrap();
        let output = String::from_utf8(
            report
                .output
                .iter()
                .flat_map(|chunk| chunk.data.iter().copied())
                .collect(),
        )
        .unwrap();
        for expected in [
            "--copy-in-source",
            source.to_str().unwrap(),
            "--copy-in-destination",
            "/workspace/input",
            "--copy-out-source",
            "/workspace/output",
            "--copy-out-destination",
            destination.to_str().unwrap(),
            "--copy-limit-bytes",
            "4096",
            "--workspace-disk",
            workspace.to_str().unwrap(),
            "--workspace-writable",
        ] {
            assert!(
                output.lines().any(|line| line == expected),
                "missing {expected}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persistent_box_lease_is_cleaned_after_helper_exit() {
        let fixture = ManagedFixture::new("#!/bin/sh\nprintf '%s\\n' \"$@\"\n", "exit 0\n");
        let (box_id, disk_path) = fixture.create_box();
        let backend = fixture.backend(None);
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.box_id = Some(box_id);

        let report = crate::Supervisor::new(backend).run(spec).await.unwrap();
        let output = report
            .output
            .iter()
            .flat_map(|chunk| chunk.data.iter().copied())
            .collect::<Vec<_>>();
        let output = String::from_utf8(output).unwrap();

        assert!(output.contains("--root-disk"));
        assert!(output.contains(disk_path.to_str().unwrap()));
        assert_eq!(report.startup.root_mode, Some(RootMode::Persistent));
        assert!(report.startup.box_lock_micros.is_some());
        assert!(report.startup.helper_spawn_micros.is_some());
        assert_eq!(fixture.boxes.get(box_id).unwrap().state, BoxState::Ready);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persistent_box_rejects_a_second_writer_and_stays_dirty_when_dropped() {
        let fixture =
            ManagedFixture::new("#!/bin/sh\nwhile :; do /bin/sleep 1; done\n", "exit 0\n");
        let (box_id, _) = fixture.create_box();
        let backend = fixture.backend(None);
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.box_id = Some(box_id);

        let running = backend
            .spawn(&spec, &RunBudget::new(spec.timeout))
            .await
            .unwrap();
        assert_eq!(fixture.boxes.get(box_id).unwrap().state, BoxState::Dirty);
        assert!(matches!(
            backend
                .spawn(&spec, &RunBudget::new(spec.timeout))
                .await,
            Err(BackendError::Control(message)) if message.contains("already in use")
        ));
        drop(running);

        assert_eq!(fixture.boxes.get(box_id).unwrap().state, BoxState::Dirty);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_leaves_the_persistent_box_dirty_after_reaping_helper() {
        let fixture = ManagedFixture::new(
            "#!/bin/sh\nwhile :; do /bin/sleep 1; done\n",
            "#!/bin/sh\nexit 0\n",
        );
        let (box_id, _) = fixture.create_box();
        let backend = fixture.backend(None);
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.box_id = Some(box_id);
        spec.timeout = moraebox_core::TimeoutPolicy::Limited(200);
        spec.kill_grace = Duration::from_millis(20);

        let report = crate::Supervisor::new(backend).run(spec).await.unwrap();

        assert!(report.timed_out);
        assert_eq!(fixture.boxes.get(box_id).unwrap().state, BoxState::Dirty);
        assert!(fixture.boxes.try_acquire(box_id).is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn helper_backend_failure_leaves_the_persistent_box_dirty() {
        let fixture = ManagedFixture::new("#!/bin/sh\nexit 125\n", "#!/bin/sh\nexit 0\n");
        let (box_id, _) = fixture.create_box();
        let backend = fixture.backend(None);
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.box_id = Some(box_id);

        let report = crate::Supervisor::new(backend).run(spec).await.unwrap();

        assert_eq!(report.exit_code, Some(125));
        assert_eq!(fixture.boxes.get(box_id).unwrap().state, BoxState::Dirty);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn helper_spawn_failure_leaves_a_durable_dirty_state() {
        let fixture = ManagedFixture::new("#!/bin/sh\nexit 0\n", "#!/bin/sh\nexit 0\n");
        let (box_id, _) = fixture.create_box();
        let backend = fixture.backend(None);
        fixture.make_helper_non_executable();
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.box_id = Some(box_id);

        assert!(
            backend
                .spawn(&spec, &RunBudget::new(spec.timeout))
                .await
                .is_err()
        );

        let reopened = BoxStore::new(fixture.boxes.state_root());
        assert_eq!(reopened.get(box_id).unwrap().state, BoxState::Dirty);
        assert!(reopened.try_acquire(box_id).is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dirty_box_runs_e2fsck_before_the_next_helper() {
        let fixture = ManagedFixture::new(
            "#!/bin/sh\nexit 0\n",
            "#!/bin/sh\nprintf repaired > \"$0.called\"\nexit 1\n",
        );
        let (box_id, _) = fixture.create_box();
        fixture.mark_box_dirty(box_id);
        let backend = fixture.backend(None);
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.box_id = Some(box_id);

        crate::Supervisor::new(backend).run(spec).await.unwrap();

        assert!(fixture.e2fsck.with_extension("called").exists());
        assert_eq!(fixture.boxes.get(box_id).unwrap().state, BoxState::Ready);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn repaired_box_is_dirty_again_before_helper_spawn() {
        let fixture = ManagedFixture::new(
            "#!/bin/sh\nexit 0\n",
            "#!/bin/sh\nprintf repaired > \"$0.called\"\nexit 1\n",
        );
        let (box_id, _) = fixture.create_box();
        fixture.mark_box_dirty(box_id);
        let backend = fixture.backend(None);
        fixture.make_helper_non_executable();
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.box_id = Some(box_id);

        assert!(
            backend
                .spawn(&spec, &RunBudget::new(spec.timeout))
                .await
                .is_err()
        );

        assert!(fixture.e2fsck.with_extension("called").exists());
        let reopened = BoxStore::new(fixture.boxes.state_root());
        assert_eq!(reopened.get(box_id).unwrap().state, BoxState::Dirty);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_repair_blocks_the_box() {
        let fixture = ManagedFixture::new("#!/bin/sh\nexit 0\n", "#!/bin/sh\nexit 4\n");
        let (box_id, _) = fixture.create_box();
        fixture.mark_box_dirty(box_id);
        let backend = fixture.backend(None);
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.box_id = Some(box_id);

        assert!(matches!(
            backend
                .spawn(&spec, &RunBudget::new(spec.timeout))
                .await,
            Err(BackendError::Control(message)) if message.contains("could not repair")
        ));
        assert_eq!(
            fixture.boxes.get(box_id).unwrap().state,
            BoxState::NeedsRepair
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn repair_timeout_preserves_the_exact_failure_stage() {
        let fixture = ManagedFixture::new(
            "#!/bin/sh\nexit 0\n",
            "#!/bin/sh\nwhile :; do /bin/sleep 1; done\n",
        );
        let (box_id, _) = fixture.create_box();
        fixture.mark_box_dirty(box_id);
        let backend = fixture.backend(None);
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.box_id = Some(box_id);
        spec.timeout = moraebox_core::TimeoutPolicy::Limited(20);
        let budget = RunBudget::new(spec.timeout);

        assert!(matches!(
            backend.spawn(&spec, &budget).await,
            Err(BackendError::Timeout {
                stage: RunStage::BoxRepair,
                ..
            })
        ));
        assert_eq!(budget.failure_stage(), Some(RunStage::BoxRepair));
        assert_eq!(fixture.boxes.get(box_id).unwrap().state, BoxState::Dirty);
        assert!(fixture.boxes.try_acquire(box_id).is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ephemeral_root_is_deleted_after_the_helper_exits() {
        let fixture = ManagedFixture::new("#!/bin/sh\nexit 0\n", "#!/bin/sh\nexit 0\n");
        let rootfs = fixture.state.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        fs::write(rootfs.join("payload"), b"rootfs").unwrap();
        let mke2fs = fixture.state.path().join("mke2fs");
        write_executable(&mke2fs, "#!/bin/sh\nexit 0\n");
        let source = BoxRootSource {
            rootfs_path: rootfs,
            manifest_digest: "sha256:ephemeral".into(),
            platform: "linux/arm64".into(),
            virtual_size_bytes: 8 * 1024 * 1024,
            mke2fs_path: mke2fs,
        };
        let runtime_root = fixture.state.path().join("runtime");
        let backend = fixture.backend(Some((source, runtime_root.clone())));

        let supervisor = crate::Supervisor::new(backend);
        let cold = match supervisor.run(RunSpec::command(["/usr/bin/true"])).await {
            Ok(report) => report,
            Err(error) if cow_clone_unavailable(&error) => return,
            Err(error) => panic!("ephemeral root run failed: {error}"),
        };
        assert_eq!(cold.startup.root_mode, Some(RootMode::Ephemeral));
        assert!(cold.startup.cache_lookup_micros.is_some());
        assert!(cold.startup.base_prepare_micros.is_some());
        assert!(cold.startup.disk_clone_micros.is_some());

        let cached = supervisor
            .run(RunSpec::command(["/usr/bin/true"]))
            .await
            .unwrap();
        assert_eq!(cached.startup.root_mode, Some(RootMode::Ephemeral));
        assert!(cached.startup.cache_lookup_micros.is_some());
        assert_eq!(cached.startup.base_prepare_micros, None);
        assert!(cached.startup.disk_clone_micros.is_some());

        let ephemeral = runtime_root.join("ephemeral-boxes");
        assert_eq!(fs::read_dir(ephemeral).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prepared_roots_are_leased_once_replenished_and_destroyed() {
        let fixture =
            ManagedFixture::new("#!/bin/sh\nprintf '%s\\n' \"$@\"\n", "#!/bin/sh\nexit 0\n");
        let rootfs = fixture.state.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        fs::write(rootfs.join("payload"), b"rootfs").unwrap();
        let mke2fs = fixture.state.path().join("mke2fs");
        write_executable(&mke2fs, "#!/bin/sh\nexit 0\n");
        let source = BoxRootSource {
            rootfs_path: rootfs,
            manifest_digest: "sha256:prepared-root".into(),
            platform: "linux/arm64".into(),
            virtual_size_bytes: MANAGED_TEST_DISK_BYTES,
            mke2fs_path: mke2fs,
        };
        let runtime_root = fixture.state.path().join("runtime");
        let pool = Arc::new(
            PreparedRootPool::new(crate::PoolConfig {
                max_size: 2,
                idle_ttl: Duration::from_secs(60),
            })
            .unwrap(),
        );
        let backend = fixture
            .backend(Some((source, runtime_root.clone())))
            .with_prepared_pool(Arc::clone(&pool));
        let supervisor = crate::Supervisor::new(backend);

        let cold = match supervisor.run(RunSpec::command(["/usr/bin/true"])).await {
            Ok(report) => report,
            Err(error) if cow_clone_unavailable(&error) => return,
            Err(error) => panic!("prepared root run failed: {error}"),
        };
        assert_eq!(cold.startup.prepared_pool_hit, Some(false));
        assert!(cold.startup.prepared_lease_micros.is_some());
        assert!(cold.startup.disk_clone_micros.is_some());
        wait_for_pool_ready(&pool).await;

        let first_warm = supervisor
            .run(RunSpec::command(["/usr/bin/true"]))
            .await
            .unwrap();
        assert_eq!(first_warm.startup.prepared_pool_hit, Some(true));
        assert!(first_warm.startup.prepared_lease_micros.is_some());
        assert_eq!(first_warm.startup.disk_clone_micros, None);
        let first_path = root_disk_argument(&first_warm);
        wait_for_pool_ready(&pool).await;

        let second_warm = supervisor
            .run(RunSpec::command(["/usr/bin/true"]))
            .await
            .unwrap();
        assert_eq!(second_warm.startup.prepared_pool_hit, Some(true));
        assert_eq!(second_warm.startup.disk_clone_micros, None);
        assert_ne!(first_path, root_disk_argument(&second_warm));
        wait_for_pool_ready(&pool).await;

        drop(supervisor);
        drop(pool);
        let ephemeral = runtime_root.join("ephemeral-boxes");
        tokio::time::timeout(Duration::from_secs(2), async {
            while fs::read_dir(&ephemeral).is_ok_and(|entries| entries.count() != 0) {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("dropping the pool must destroy every unleased prepared root");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_preflight_failure_precedes_root_and_network_side_effects() {
        let fixture = ManagedFixture::new("#!/bin/sh\nexit 0\n", "#!/bin/sh\nexit 0\n");
        let rootfs = fixture.state.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        fs::write(rootfs.join("payload"), b"rootfs").unwrap();
        let mke2fs = fixture.state.path().join("observed-mke2fs");
        write_executable(&mke2fs, "#!/bin/sh\nprintf ran > \"$0.ran\"\nexit 9\n");
        let source = BoxRootSource {
            rootfs_path: rootfs,
            manifest_digest: "sha256:preflight-order".into(),
            platform: "linux/arm64".into(),
            virtual_size_bytes: MANAGED_TEST_DISK_BYTES,
            mke2fs_path: mke2fs.clone(),
        };
        let gvproxy = fixture.state.path().join("gvproxy");
        write_executable(
            &gvproxy,
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$0.pid\"\nwhile :; do /bin/sleep 1; done\n",
        );
        let mut backend = fixture.backend(Some((
            source,
            fixture.state.path().join("ephemeral-runtime"),
        )));
        backend.config.enforce_native_preflight = true;
        backend.config.gvproxy_path = Some(gvproxy.clone());
        backend.config.network_runtime_dir = fixture.state.path().join("network");
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.network = true;

        let error = match backend.spawn(&spec, &RunBudget::new(spec.timeout)).await {
            Ok(_) => panic!("invalid native prerequisites must fail before spawn"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("native runtime preflight failed"));
        assert!(!mke2fs.with_extension("ran").exists());
        assert!(!gvproxy.with_extension("pid").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn root_preparation_failure_never_starts_the_network_proxy() {
        let fixture = ManagedFixture::new("#!/bin/sh\nexit 0\n", "#!/bin/sh\nexit 0\n");
        let rootfs = fixture.state.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        fs::write(rootfs.join("payload"), b"rootfs").unwrap();
        let mke2fs = fixture.state.path().join("failing-mke2fs");
        write_executable(&mke2fs, "#!/bin/sh\nexit 9\n");
        let source = BoxRootSource {
            rootfs_path: rootfs,
            manifest_digest: "sha256:root-failure".into(),
            platform: "linux/arm64".into(),
            virtual_size_bytes: MANAGED_TEST_DISK_BYTES,
            mke2fs_path: mke2fs,
        };
        let gvproxy = fixture.state.path().join("gvproxy");
        write_executable(
            &gvproxy,
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$0.pid\"\nwhile :; do /bin/sleep 1; done\n",
        );
        let mut backend = fixture.backend(Some((
            source,
            fixture.state.path().join("ephemeral-runtime"),
        )));
        backend.config.gvproxy_path = Some(gvproxy.clone());
        backend.config.network_runtime_dir = fixture.state.path().join("network");
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.network = true;

        assert!(
            backend
                .spawn(&spec, &RunBudget::new(spec.timeout))
                .await
                .is_err()
        );
        assert!(!gvproxy.with_extension("pid").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn helper_spawn_failure_awaits_network_proxy_cleanup() {
        use nix::{errno::Errno, sys::signal::kill, unistd::Pid};

        let state = tempfile::tempdir().unwrap();
        let helper = state.path().join("helper");
        let gvproxy = state.path().join("gvproxy");
        let library = state.path().join("libkrun");
        let root = state.path().join("root");
        let network_runtime_dir = state.path().join("network");
        write_executable(&helper, "#!/bin/sh\nexit 0\n");
        let mut permissions = fs::metadata(&helper).unwrap().permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(&helper, permissions).unwrap();
        write_executable(
            &gvproxy,
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$0.pid\"\nprintf '%s' \"${2#unixgram://}\" > \"$0.socket\"\nwhile :; do /bin/sleep 1; done\n",
        );
        fs::write(&library, []).unwrap();
        fs::create_dir(&root).unwrap();
        let mut config = LibkrunConfig::new(helper, library, root);
        config.gvproxy_path = Some(gvproxy.clone());
        config.network_runtime_dir = network_runtime_dir.clone();
        let backend = LibkrunBackend::new(config);
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.network = true;
        let endpoint = spawn_fake_vfkit_endpoint(gvproxy.with_extension("socket"));

        assert!(
            backend
                .spawn(&spec, &RunBudget::new(spec.timeout))
                .await
                .is_err()
        );
        let endpoint = endpoint.await.unwrap();
        let pid = read_pid(&gvproxy.with_extension("pid"));
        assert_eq!(kill(Pid::from_raw(pid), None), Err(Errno::ESRCH));
        assert_eq!(fs::read_dir(&network_runtime_dir).unwrap().count(), 0);
        drop(endpoint);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn network_setup_cancellation_reaps_the_proxy() {
        use nix::{errno::Errno, sys::signal::kill, unistd::Pid};

        let state = tempfile::tempdir().unwrap();
        let helper = state.path().join("helper");
        let gvproxy = state.path().join("gvproxy");
        let library = state.path().join("libkrun");
        let root = state.path().join("root");
        let network_runtime_dir = state.path().join("network");
        write_executable(&helper, "#!/bin/sh\nexit 0\n");
        write_executable(
            &gvproxy,
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$0.pid\"\nwhile :; do /bin/sleep 1; done\n",
        );
        fs::write(&library, []).unwrap();
        fs::create_dir(&root).unwrap();
        let mut config = LibkrunConfig::new(helper, library, root);
        config.gvproxy_path = Some(gvproxy.clone());
        config.network_runtime_dir = network_runtime_dir.clone();
        let (pid_tx, pid_rx) = tokio::sync::oneshot::channel();
        let mut start = Box::pin(NetworkProxy::start_observed(&config, move |pid| {
            let _ = pid_tx.send(pid);
        }));
        let pid = tokio::select! {
            pid = pid_rx => pid.unwrap().expect("spawned gvproxy must have a process id"),
            result = &mut start => panic!("network proxy became ready before cancellation: {result:?}"),
        };
        drop(start);
        let pid = i32::try_from(pid).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while kill(Pid::from_raw(pid), None) != Err(Errno::ESRCH) {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled network setup must reap gvproxy");
        assert_eq!(fs::read_dir(network_runtime_dir).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn network_proxy_is_passed_to_the_helper_and_reaped() {
        use nix::{errno::Errno, sys::signal::kill, unistd::Pid};

        let state = tempfile::tempdir().unwrap();
        let helper = state.path().join("helper");
        let gvproxy = state.path().join("gvproxy");
        let proxy_pid = state.path().join("gvproxy.pid");
        let library = state.path().join("libkrun");
        let root = state.path().join("root");
        let network_runtime_dir = state.path().join(".moraebox/network");
        write_executable(&helper, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n");
        write_executable(
            &gvproxy,
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$0.pid\"\nprintf '%s' \"${2#unixgram://}\" > \"$0.socket\"\nwhile :; do /bin/sleep 1; done\n",
        );
        fs::write(&library, []).unwrap();
        fs::create_dir(&root).unwrap();

        let mut config = LibkrunConfig::new(helper, library, root);
        config.gvproxy_path = Some(gvproxy.clone());
        config.network_runtime_dir = network_runtime_dir.clone();
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.network = true;
        let endpoint = spawn_fake_vfkit_endpoint(gvproxy.with_extension("socket"));

        let report = crate::Supervisor::new(LibkrunBackend::new(config))
            .run(spec)
            .await
            .unwrap();
        let _endpoint = endpoint.await.unwrap();
        let output = report
            .output
            .iter()
            .flat_map(|chunk| chunk.data.iter().copied())
            .collect::<Vec<_>>();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("--network-socket"));

        let pid = fs::read_to_string(proxy_pid)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        assert_eq!(kill(Pid::from_raw(pid), None), Err(Errno::ESRCH));
        assert_eq!(fs::read_dir(network_runtime_dir).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_reaps_the_network_proxy() {
        use nix::{errno::Errno, sys::signal::kill, unistd::Pid};

        let state = tempfile::tempdir().unwrap();
        let helper = state.path().join("helper");
        let gvproxy = state.path().join("gvproxy");
        let proxy_pid = state.path().join("gvproxy.pid");
        let library = state.path().join("libkrun");
        let root = state.path().join("root");
        let network_runtime_dir = state.path().join(".moraebox/network");
        write_executable(&helper, "#!/bin/sh\nwhile :; do /bin/sleep 1; done\n");
        write_executable(
            &gvproxy,
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$0.pid\"\nprintf '%s' \"${2#unixgram://}\" > \"$0.socket\"\nwhile :; do /bin/sleep 1; done\n",
        );
        fs::write(&library, []).unwrap();
        fs::create_dir(&root).unwrap();

        let mut config = LibkrunConfig::new(helper, library, root);
        config.gvproxy_path = Some(gvproxy.clone());
        config.network_runtime_dir = network_runtime_dir.clone();
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.network = true;
        spec.timeout = moraebox_core::TimeoutPolicy::Limited(5_000);
        spec.kill_grace = Duration::from_millis(20);
        let endpoint = spawn_fake_vfkit_endpoint(gvproxy.with_extension("socket"));

        let report = crate::Supervisor::new(LibkrunBackend::new(config))
            .run(spec)
            .await
            .unwrap();
        let _endpoint = endpoint.await.unwrap();

        assert!(report.timed_out);
        let pid = fs::read_to_string(proxy_pid)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        assert_eq!(kill(Pid::from_raw(pid), None), Err(Errno::ESRCH));
        assert_eq!(fs::read_dir(network_runtime_dir).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_spawned_sandbox_reaps_the_network_proxy() {
        use nix::{errno::Errno, sys::signal::kill, unistd::Pid};

        let state = tempfile::tempdir().unwrap();
        let helper = state.path().join("helper");
        let gvproxy = state.path().join("gvproxy");
        let proxy_pid = state.path().join("gvproxy.pid");
        let library = state.path().join("libkrun");
        let root = state.path().join("root");
        let network_runtime_dir = state.path().join(".moraebox/network");
        write_executable(&helper, "#!/bin/sh\nwhile :; do /bin/sleep 1; done\n");
        write_executable(
            &gvproxy,
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$0.pid\"\nprintf '%s' \"${2#unixgram://}\" > \"$0.socket\"\nwhile :; do /bin/sleep 1; done\n",
        );
        fs::write(&library, []).unwrap();
        fs::create_dir(&root).unwrap();

        let mut config = LibkrunConfig::new(helper, library, root);
        config.gvproxy_path = Some(gvproxy.clone());
        config.network_runtime_dir = network_runtime_dir.clone();
        let backend = LibkrunBackend::new(config);
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.network = true;
        let endpoint = spawn_fake_vfkit_endpoint(gvproxy.with_extension("socket"));

        let spawned = backend
            .spawn(&spec, &RunBudget::new(spec.timeout))
            .await
            .unwrap();
        let endpoint = endpoint.await.unwrap();
        let pid = fs::read_to_string(proxy_pid)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        drop(spawned);

        let pid = Pid::from_raw(pid);
        for _ in 0..100 {
            if kill(pid, None) == Err(Errno::ESRCH) {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(kill(pid, None), Err(Errno::ESRCH));
        assert_eq!(fs::read_dir(network_runtime_dir).unwrap().count(), 0);
        drop(endpoint);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_owner_loss_reaps_all_managed_native_resources() {
        use nix::{errno::Errno, sys::signal::kill, unistd::Pid};

        let fixture = ManagedFixture::new(
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$0.pid\"\nprintf '%s' \"${2#unixgram://}\" > \"$0.socket\"\nwhile :; do /bin/sleep 1; done\n",
            "#!/bin/sh\nexit 0\n",
        );
        let rootfs = fixture.state.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        fs::write(rootfs.join("payload"), b"rootfs").unwrap();
        let mke2fs = fixture.state.path().join("mke2fs");
        write_executable(&mke2fs, "#!/bin/sh\nexit 0\n");
        let source = BoxRootSource {
            rootfs_path: rootfs,
            manifest_digest: "sha256:owner-loss".into(),
            platform: "linux/arm64".into(),
            virtual_size_bytes: MANAGED_TEST_DISK_BYTES,
            mke2fs_path: mke2fs,
        };
        let runtime_root = fixture.state.path().join("runtime");
        let network_runtime_dir = fixture.state.path().join("network");
        let gvproxy = fixture.state.path().join("gvproxy");
        write_executable(
            &gvproxy,
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$0.pid\"\nprintf '%s' \"${2#unixgram://}\" > \"$0.socket\"\nwhile :; do /bin/sleep 1; done\n",
        );
        let mut backend = fixture.backend(Some((source, runtime_root.clone())));
        backend.config.gvproxy_path = Some(gvproxy.clone());
        backend.config.network_runtime_dir = network_runtime_dir.clone();
        let manager = crate::SessionManager::new(Arc::new(backend));
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.network = true;
        spec.kill_grace = Duration::from_millis(20);
        let session_id = spec.session_id;
        let endpoint = spawn_fake_vfkit_endpoint(gvproxy.with_extension("socket"));

        let session = match manager.start(spec).await {
            Ok(session) => session,
            Err(error) if cow_clone_unavailable(&error) => {
                endpoint.abort();
                return;
            }
            Err(error) => panic!("managed session start failed: {error}"),
        };
        let endpoint = endpoint.await.unwrap();
        let helper_pid_path = fixture.helper.with_extension("pid");
        let proxy_pid_path = gvproxy.with_extension("pid");
        wait_for_paths([&helper_pid_path, &proxy_pid_path]).await;
        let helper_pid = read_pid(&helper_pid_path);
        let proxy_pid = read_pid(&proxy_pid_path);
        let ephemeral_directory = runtime_root
            .join("ephemeral-boxes")
            .join(session_id.to_string());
        assert!(ephemeral_directory.is_dir());
        assert_eq!(fs::read_dir(&network_runtime_dir).unwrap().count(), 1);

        drop(session);

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let helper_gone = kill(Pid::from_raw(helper_pid), None) == Err(Errno::ESRCH);
                let proxy_gone = kill(Pid::from_raw(proxy_pid), None) == Err(Errno::ESRCH);
                let disk_gone = !ephemeral_directory.exists();
                let network_gone =
                    fs::read_dir(&network_runtime_dir).is_ok_and(|entries| entries.count() == 0);
                if helper_gone && proxy_gone && disk_gone && network_gone {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("owner loss must reap helper, proxy, disk, and socket state");
        drop(endpoint);
    }

    #[cfg(unix)]
    fn spawn_fake_vfkit_endpoint(
        socket_path_file: PathBuf,
    ) -> JoinHandle<std::os::unix::net::UnixDatagram> {
        tokio::task::spawn_blocking(move || {
            for _ in 0..500 {
                let socket = fs::read_to_string(&socket_path_file)
                    .ok()
                    .and_then(|socket_path| {
                        std::os::unix::net::UnixDatagram::bind(socket_path).ok()
                    });
                if let Some(socket) = socket {
                    return socket;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("fake network proxy did not publish a bindable socket path")
        })
    }

    #[cfg(unix)]
    async fn wait_for_paths<const N: usize>(paths: [&Path; N]) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while paths.iter().any(|path| !path.exists()) {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("child process did not publish its pid");
    }

    #[cfg(unix)]
    async fn wait_for_pool_ready(pool: &PreparedRootPool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while pool.stats().await.ready == 0 {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("prepared root replenishment did not finish");
    }

    #[cfg(unix)]
    fn root_disk_argument(report: &crate::RunReport) -> PathBuf {
        let output = report
            .output
            .iter()
            .flat_map(|chunk| chunk.data.iter().copied())
            .collect::<Vec<_>>();
        let output = String::from_utf8(output).unwrap();
        let mut arguments = output.lines();
        while let Some(argument) = arguments.next() {
            if argument == "--root-disk" {
                return PathBuf::from(arguments.next().expect("root disk path follows flag"));
            }
        }
        panic!("helper arguments did not contain --root-disk");
    }

    #[cfg(unix)]
    fn read_pid(path: &Path) -> i32 {
        fs::read_to_string(path).unwrap().parse::<i32>().unwrap()
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(unix)]
    struct ManagedFixture {
        state: tempfile::TempDir,
        helper: PathBuf,
        library: PathBuf,
        e2fsck: PathBuf,
        debugfs: PathBuf,
        boxes: BoxStore,
    }

    #[cfg(unix)]
    impl ManagedFixture {
        fn new(helper_script: &str, e2fsck_script: &str) -> Self {
            let state = tempfile::tempdir().unwrap();
            let helper = state.path().join("helper");
            let library = state.path().join("libkrun");
            let e2fsck = state.path().join("e2fsck");
            let debugfs = state.path().join("debugfs");
            write_executable(&helper, helper_script);
            write_executable(&e2fsck, e2fsck_script);
            write_executable(&debugfs, "#!/bin/sh\nexit 0\n");
            fs::write(&library, []).unwrap();
            let boxes = BoxStore::new(state.path().join("state"));
            Self {
                state,
                helper,
                library,
                e2fsck,
                debugfs,
                boxes,
            }
        }

        fn create_box(&self) -> (moraebox_core::BoxId, PathBuf) {
            let source = self
                .state
                .path()
                .join(format!("{}.ext4", moraebox_core::BoxId::new()));
            let file = fs::File::create(&source).unwrap();
            file.set_len(MANAGED_TEST_DISK_BYTES).unwrap();
            drop(file);
            let metadata = self
                .boxes
                .create(
                    &CreateBox::new("sha256:persistent", "linux/arm64", MANAGED_TEST_DISK_BYTES),
                    &source,
                )
                .unwrap();
            let path = self
                .boxes
                .boxes_directory()
                .join(metadata.box_id.to_string())
                .join("root.ext4");
            (metadata.box_id, path)
        }

        fn mark_box_dirty(&self, box_id: moraebox_core::BoxId) {
            let mut lease = self.boxes.try_acquire(box_id).unwrap();
            self.boxes.begin_writable_use(&mut lease).unwrap();
        }

        fn make_helper_non_executable(&self) {
            let mut permissions = fs::metadata(&self.helper).unwrap().permissions();
            permissions.set_mode(0o600);
            fs::set_permissions(&self.helper, permissions).unwrap();
        }

        fn backend(&self, source: Option<(BoxRootSource, PathBuf)>) -> LibkrunBackend {
            let mut config = LibkrunConfig::new(
                &self.helper,
                &self.library,
                self.state.path().join("unused-root"),
            );
            config.debugfs_path.clone_from(&self.debugfs);
            let runtime = BoxRuntimeConfig {
                boxes: self.boxes.clone(),
                base_disks: BaseDiskStore::new(self.state.path().join("cache")),
                ephemeral_disks: EphemeralDiskStore::new(source.as_ref().map_or_else(
                    || self.state.path().join("runtime"),
                    |(_, path)| path.clone(),
                )),
                source: source.map(|(source, _)| source),
                e2fsck_path: self.e2fsck.clone(),
            };
            LibkrunBackend::new(config).with_box_runtime(runtime)
        }
    }
}
