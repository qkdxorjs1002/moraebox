use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use moraebox_box::{
    BaseDiskSpec, BaseDiskStore, BoxLease, BoxState, BoxStore, BoxStoreError, EphemeralDisk,
    EphemeralDiskStore,
};
use moraebox_core::{OutputChannel, RunSpec, Signal};
use tempfile::TempDir;
use tokio::{
    process::{Child, Command},
    time::{Instant, sleep},
};

use crate::{
    Backend, BackendCapabilities, BackendController, BackendError, CapabilitySupport,
    IsolationLevel, RootMode, RunBudget, RunStage, SpawnedSandbox, StartupMetrics,
    environment::resolve_environment,
};

const NETWORK_PROXY_START_TIMEOUT: Duration = Duration::from_secs(5);
const NETWORK_PROXY_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone)]
pub struct LibkrunConfig {
    pub helper_path: PathBuf,
    pub library_path: PathBuf,
    pub root_path: PathBuf,
    pub root_disk: Option<PathBuf>,
    pub library_search_path: Option<PathBuf>,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub workspace_disk: Option<PathBuf>,
    pub gvproxy_path: Option<PathBuf>,
    pub network_runtime_dir: PathBuf,
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
        Self {
            helper_path: helper_path.into(),
            library_path: library_path.into(),
            root_path: root_path.into(),
            root_disk: None,
            library_search_path: None,
            vcpus: 2,
            memory_mib: 512,
            workspace_disk: None,
            gvproxy_path: None,
            network_runtime_dir: PathBuf::from(".moraebox/network"),
        }
    }

    #[must_use]
    pub fn with_root_disk(mut self, root_disk: impl Into<PathBuf>) -> Self {
        self.root_disk = Some(root_disk.into());
        self
    }

    fn validate(&self, managed_root: bool) -> Result<(), BackendError> {
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
        if let Some(path) = &self.workspace_disk
            && !path.is_file()
        {
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
}

impl LibkrunBackend {
    pub const CAPABILITIES: BackendCapabilities = BackendCapabilities {
        isolation: IsolationLevel::MicroVm,
        tty: CapabilitySupport::Supported,
        network: CapabilitySupport::Supported,
        box_persistence: CapabilitySupport::Supported,
        workspace: CapabilitySupport::Supported,
    };
}

impl LibkrunBackend {
    pub fn new(config: LibkrunConfig) -> Self {
        Self {
            config,
            box_runtime: None,
        }
    }

    #[must_use]
    pub fn with_box_runtime(mut self, config: BoxRuntimeConfig) -> Self {
        self.box_runtime = Some(config);
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

    async fn spawn(
        &self,
        spec: &RunSpec,
        budget: &RunBudget,
    ) -> Result<SpawnedSandbox, BackendError> {
        spec.validate().map_err(BackendError::InvalidSpec)?;
        self.config.validate(self.box_runtime.is_some())?;
        if spec.box_id.is_some() && self.box_runtime.is_none() {
            return Err(BackendError::Control(
                "run requested a BoxId but no Box store is configured".into(),
            ));
        }
        let environment = resolve_environment(spec)?;

        let mut startup = StartupMetrics::default();
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
        startup.cache_lookup_micros = root_startup.cache_lookup_micros;
        startup.box_lock_micros = root_startup.box_lock_micros;
        startup.base_prepare_micros = root_startup.base_prepare_micros;
        startup.disk_clone_micros = root_startup.disk_clone_micros;
        startup.repair_micros = root_startup.repair_micros;

        let mut command = Command::new(&self.config.helper_path);
        command.arg("--libkrun").arg(&self.config.library_path);
        if let Some(root_disk) = prepared_root.disk_path() {
            command.arg("--root-disk").arg(root_disk);
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
        if let Some(workspace) = &self.config.workspace_disk {
            command.arg("--workspace-disk").arg(workspace);
        }
        if let Some(proxy) = &network_proxy {
            command.arg("--network-socket").arg(&proxy.socket_path);
        }
        for (key, value) in environment {
            command.arg("--env").arg(format!("{key}={value}"));
        }
        command.arg("--").args(&spec.argv);
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
        let mut spawned = budget
            .run_sync(RunStage::HelperSpawn, || {
                if spec.tty {
                    spawn_pty(command, spec, network_proxy, prepared_root.into_lease())
                } else {
                    spawn_piped(command, network_proxy, prepared_root.into_lease())
                }
            })
            .map_err(BackendError::from)?;
        startup.helper_spawn_micros = Some(elapsed_micros(helper_started));
        spawned.startup = startup;
        Ok(spawned)
    }
}

impl LibkrunBackend {
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
        Self::prepare_ephemeral_root(runtime, spec, budget).await
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
        let session_id = spec.session_id;
        let clone_started = Instant::now();
        let disk = budget
            .run(RunStage::EphemeralDiskClone, async move {
                tokio::task::spawn_blocking(move || {
                    ephemeral_store.clone_for_session(&base, session_id)
                })
                .await
                .map_err(|error| BackendError::Control(format!("CoW clone task failed: {error}")))?
                .map_err(box_backend_error)
            })
            .await
            .map_err(BackendError::from)?;
        startup.disk_clone_micros = Some(elapsed_micros(clone_started));
        Ok((
            PreparedRoot::Managed(ManagedRootLease::Ephemeral(disk)),
            startup,
        ))
    }
}

enum PreparedRoot {
    Directory,
    StaticDisk(PathBuf),
    Managed(ManagedRootLease),
}

impl PreparedRoot {
    fn disk_path(&self) -> Option<&Path> {
        match self {
            Self::Directory => None,
            Self::StaticDisk(path) => Some(path),
            Self::Managed(lease) => Some(lease.disk_path()),
        }
    }

    fn into_lease(self) -> Option<ManagedRootLease> {
        match self {
            Self::Managed(lease) => Some(lease),
            Self::Directory | Self::StaticDisk(_) => None,
        }
    }
}

enum ManagedRootLease {
    Persistent { store: BoxStore, lease: BoxLease },
    Ephemeral(EphemeralDisk),
}

impl ManagedRootLease {
    fn disk_path(&self) -> &Path {
        match self {
            Self::Persistent { lease, .. } => lease.disk_path(),
            Self::Ephemeral(disk) => disk.disk_path(),
        }
    }

    fn mark_clean(&mut self) -> Result<(), BoxStoreError> {
        if let Self::Persistent { store, lease } = self {
            store.finish_clean_use(lease)?;
        }
        Ok(())
    }
}

async fn repair_dirty_box(
    store: &BoxStore,
    lease: &mut BoxLease,
    e2fsck: &Path,
) -> Result<(), BackendError> {
    if !e2fsck.is_file() {
        return Err(BackendError::Control(format!(
            "dirty Box {} requires e2fsck, but it was not found at {}",
            lease.id(),
            e2fsck.display()
        )));
    }
    let mut command = Command::new(e2fsck);
    command
        .arg("-p")
        .arg(lease.disk_path())
        .env_clear()
        .kill_on_drop(true);
    let output = command.output().await?;
    if matches!(output.status.code(), Some(0 | 1)) {
        store.finish_repair(lease).map_err(box_backend_error)?;
        return Ok(());
    }
    store.mark_needs_repair(lease).map_err(box_backend_error)?;
    Err(BackendError::Control(format!(
        "e2fsck could not repair Box {} (status {:?}): {}",
        lease.id(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "this function is a map_err adapter and owns the source error"
)]
fn box_backend_error(error: BoxStoreError) -> BackendError {
    BackendError::Control(error.to_string())
}

fn spawn_piped(
    mut command: Command,
    network_proxy: Option<NetworkProxy>,
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
    let exit = managed_exit(child, network_proxy, root_lease);
    Ok(SpawnedSandbox {
        stdin,
        stdout,
        stdout_channel: OutputChannel::Stdout,
        stderr,
        exit,
        controller: Box::new(LibkrunController { pid: Arc::new(pid) }),
        startup: StartupMetrics::default(),
    })
}

#[cfg(unix)]
fn spawn_pty(
    mut command: Command,
    spec: &RunSpec,
    network_proxy: Option<NetworkProxy>,
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
    let master_reader = master.try_clone()?;
    let slave = File::from(pty.slave);
    command.stdin(Stdio::from(slave.try_clone()?));
    command.stdout(Stdio::from(slave.try_clone()?));
    command.stderr(Stdio::from(slave));

    let child = command.spawn()?;
    let pid = child.id().ok_or(BackendError::MissingProcessId)?;
    let exit = managed_exit(child, network_proxy, root_lease);
    Ok(SpawnedSandbox {
        stdin: Some(Box::pin(tokio::fs::File::from_std(master))),
        stdout: Box::pin(tokio::fs::File::from_std(master_reader)),
        stdout_channel: OutputChannel::Tty,
        stderr: None,
        exit,
        controller: Box::new(LibkrunController { pid: Arc::new(pid) }),
        startup: StartupMetrics::default(),
    })
}

#[cfg(not(unix))]
fn spawn_pty(
    _command: Command,
    _spec: &RunSpec,
    _network_proxy: Option<NetworkProxy>,
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
        let root_cleanup = if let Ok(status) = &status
            && helper_exit_is_clean(*status)
            && let Some(lease) = root_lease.as_mut()
        {
            lease
                .mark_clean()
                .map_err(|error| io::Error::other(error.to_string()))
        } else {
            Ok(())
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
struct NetworkProxy {
    child: Child,
    state: TempDir,
    socket_path: PathBuf,
}

impl NetworkProxy {
    async fn start(config: &LibkrunConfig) -> Result<Self, BackendError> {
        let executable = config.network_proxy_path()?;
        let state = create_network_state(config)?;
        let socket_path = state.path().join("gvproxy.sock");
        let socket = vfkit_socket_uri(&socket_path)?;

        let mut command = Command::new(executable);
        command
            .arg("--listen-vfkit")
            .arg(socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_clear()
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }

        let mut child = command
            .spawn()
            .map_err(|error| BackendError::Control(format!("failed to start gvproxy: {error}")))?;
        let started = Instant::now();
        loop {
            if socket_path.exists() {
                break;
            }
            if let Some(status) = child.try_wait()? {
                return Err(BackendError::Control(format!(
                    "gvproxy exited before creating its vfkit socket: {status}"
                )));
            }
            if started.elapsed() >= NETWORK_PROXY_START_TIMEOUT {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(BackendError::Control(
                    "gvproxy did not create its vfkit socket within 5 seconds".into(),
                ));
            }
            sleep(NETWORK_PROXY_POLL_INTERVAL).await;
        }

        Ok(Self {
            child,
            state,
            socket_path,
        })
    }

    async fn stop(mut self) -> io::Result<()> {
        if self.child.try_wait()?.is_none()
            && let Err(error) = self.child.start_kill()
            && self.child.try_wait()?.is_none()
        {
            return Err(error);
        }
        let _ = self.child.wait().await?;
        self.state.close()
    }
}

fn create_network_state(config: &LibkrunConfig) -> Result<TempDir, BackendError> {
    std::fs::create_dir_all(&config.network_runtime_dir)?;
    let runtime_root = std::fs::canonicalize(&config.network_runtime_dir)?;
    let configured = tempfile::Builder::new()
        .prefix("run-")
        .tempdir_in(runtime_root)?;
    if vfkit_socket_uri(&configured.path().join("gvproxy.sock")).is_ok() {
        return Ok(configured);
    }
    drop(configured);

    let temporary_root = std::fs::canonicalize(std::env::temp_dir())?;
    let fallback = tempfile::Builder::new()
        .prefix("morae-net-")
        .tempdir_in(temporary_root)?;
    vfkit_socket_uri(&fallback.path().join("gvproxy.sock"))?;
    Ok(fallback)
}

fn vfkit_socket_uri(path: &Path) -> Result<String, BackendError> {
    if !path.is_absolute() {
        return Err(BackendError::Control(
            "gvproxy socket path must be absolute".into(),
        ));
    }
    let path = path
        .to_str()
        .ok_or_else(|| BackendError::Control("gvproxy socket path must be valid UTF-8".into()))?;
    #[cfg(unix)]
    if path.len() >= 104 {
        return Err(BackendError::Control(format!(
            "gvproxy socket path exceeds the Unix limit: {} bytes",
            path.len()
        )));
    }
    Ok(format!("unixgram://{path}"))
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

            let raw_pid = i32::try_from(*self.pid)
                .map_err(|error| BackendError::Control(error.to_string()))?;
            let signal = match signal {
                Signal::Interrupt => NixSignal::SIGINT,
                Signal::Terminate => NixSignal::SIGTERM,
                Signal::Kill => NixSignal::SIGKILL,
                Signal::Hangup => NixSignal::SIGHUP,
            };
            match kill(Pid::from_raw(-raw_pid), signal) {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    use moraebox_box::CreateBox;

    const MANAGED_TEST_DISK_BYTES: u64 = 8 * 1024 * 1024;

    #[cfg(unix)]
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn rejects_missing_native_paths() {
        let config = LibkrunConfig::new("missing-helper", "missing-lib", "missing-root");
        assert!(config.validate(false).is_err());
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
        write_executable(&helper, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n");
        fs::write(&library, []).unwrap();
        fs::write(&root_disk, []).unwrap();

        let config = LibkrunConfig::new(helper, library, root).with_root_disk(&root_disk);
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
        assert!(!output.contains("--root\n"));
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
        let cold = supervisor
            .run(RunSpec::command(["/usr/bin/true"]))
            .await
            .unwrap();
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
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$0.pid\"\nsocket=${2#unixgram://}\n: > \"$socket\"\nwhile :; do /bin/sleep 1; done\n",
        );
        fs::write(&library, []).unwrap();
        fs::create_dir(&root).unwrap();

        let mut config = LibkrunConfig::new(helper, library, root);
        config.gvproxy_path = Some(gvproxy);
        config.network_runtime_dir = network_runtime_dir.clone();
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.network = true;

        let report = crate::Supervisor::new(LibkrunBackend::new(config))
            .run(spec)
            .await
            .unwrap();
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
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$0.pid\"\nsocket=${2#unixgram://}\n: > \"$socket\"\nwhile :; do /bin/sleep 1; done\n",
        );
        fs::write(&library, []).unwrap();
        fs::create_dir(&root).unwrap();

        let mut config = LibkrunConfig::new(helper, library, root);
        config.gvproxy_path = Some(gvproxy);
        config.network_runtime_dir = network_runtime_dir.clone();
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.network = true;
        spec.timeout = moraebox_core::TimeoutPolicy::Limited(5_000);
        spec.kill_grace = Duration::from_millis(20);

        let report = crate::Supervisor::new(LibkrunBackend::new(config))
            .run(spec)
            .await
            .unwrap();

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
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$0.pid\"\nsocket=${2#unixgram://}\n: > \"$socket\"\nwhile :; do /bin/sleep 1; done\n",
        );
        fs::write(&library, []).unwrap();
        fs::create_dir(&root).unwrap();

        let mut config = LibkrunConfig::new(helper, library, root);
        config.gvproxy_path = Some(gvproxy);
        config.network_runtime_dir = network_runtime_dir.clone();
        let backend = LibkrunBackend::new(config);
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.network = true;

        let spawned = backend
            .spawn(&spec, &RunBudget::new(spec.timeout))
            .await
            .unwrap();
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
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_owner_loss_reaps_all_managed_native_resources() {
        use nix::{errno::Errno, sys::signal::kill, unistd::Pid};

        let fixture = ManagedFixture::new(
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$0.pid\"\nwhile :; do /bin/sleep 1; done\n",
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
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$0.pid\"\nsocket=${2#unixgram://}\n: > \"$socket\"\nwhile :; do /bin/sleep 1; done\n",
        );
        let mut backend = fixture.backend(Some((source, runtime_root.clone())));
        backend.config.gvproxy_path = Some(gvproxy.clone());
        backend.config.network_runtime_dir = network_runtime_dir.clone();
        let manager = crate::SessionManager::new(Arc::new(backend));
        let mut spec = RunSpec::command(["/usr/bin/true"]);
        spec.network = true;
        spec.kill_grace = Duration::from_millis(20);
        let session_id = spec.session_id;

        let session = manager.start(spec).await.unwrap();
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
        boxes: BoxStore,
    }

    #[cfg(unix)]
    impl ManagedFixture {
        fn new(helper_script: &str, e2fsck_script: &str) -> Self {
            let state = tempfile::tempdir().unwrap();
            let helper = state.path().join("helper");
            let library = state.path().join("libkrun");
            let e2fsck = state.path().join("e2fsck");
            write_executable(&helper, helper_script);
            write_executable(&e2fsck, e2fsck_script);
            fs::write(&library, []).unwrap();
            let boxes = BoxStore::new(state.path().join("state"));
            Self {
                state,
                helper,
                library,
                e2fsck,
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
            let disk = self.boxes.try_acquire(metadata.box_id).unwrap();
            let path = disk.disk_path().to_path_buf();
            drop(disk);
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
            let config = LibkrunConfig::new(
                &self.helper,
                &self.library,
                self.state.path().join("unused-root"),
            );
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
