use super::{
    Arc, Backend, BackendCapabilities, BaseDiskSpec, BaseDiskStore, BenchmarkArgs, BoxCloneArgs,
    BoxCommand, BoxCreateArgs, BoxDeleteArgs, BoxId, BoxListArgs, BoxMetadata, BoxRepairArgs,
    BoxRepairReport, BoxResetArgs, BoxShowArgs, BoxStore, CacheCleanArgs, CacheCommand,
    CacheInfoArgs, CachePruneArgs, CacheReconcileArgs, CacheReconcileReport, CacheUsage,
    CachedImage, CleanReport, Cli, CliError, CliErrorSource, Command, CommandFactory,
    CompletionArgs, CreateBox, Credentials, DiskToolPaths, DoctorArgs, DoctorReport, Duration,
    ExitCode, GlobalOptions, ImageCache, ImageCommand, ImageDefaultArgs, ImageListArgs,
    ImageProgressStage, ImagePullArgs, ImagePullPolicy, ImageRemoveArgs, IsTerminal,
    IsolationLevel, LibkrunBackend, MAX_KILL_GRACE, MAX_OUTPUT_LIMIT, ManagedStorage,
    NativeRuntimeOverrides, NativeRuntimePaths, NativeSandboxConfig, OutputChannel, Path, Platform,
    PoolConfig, PreparedImage, PreparedRootPool, ProcessBackend, PruneReport, Read, RemoveReport,
    RootfsMetadataIssueKind, RunArgs, RunBudget, RunSpec, RunStage, Serialize, StoragePaths,
    Supervisor, TimeoutPolicy, WorkspaceSnapshot, WorkspaceStage, Write, command_stage, fs, io,
    resolve_cache_dir, resolve_state_dir, run_interactive,
};
use std::collections::BTreeMap;

pub(super) async fn execute(cli: Cli) -> Result<i32, CliError> {
    let Cli { global, command } = cli;
    let stage = command_stage(&command);
    warn_project_local_storage(&global, storage_use(&command));
    let result = match command {
        Command::Doctor(args) => doctor(&args, &global),
        Command::Run(args) => run(*args, &global).await,
        Command::Image {
            command: ImageCommand::Pull(args),
        } => image_pull(args, &global).await,
        Command::Image {
            command: ImageCommand::List(args),
        } => image_list(&args, &global),
        Command::Image {
            command: ImageCommand::Remove(args),
        } => image_remove(&args, &global),
        Command::Image {
            command: ImageCommand::Default(args),
        } => image_default(&args, &global),
        Command::Cache {
            command: CacheCommand::Info(args),
        } => cache_info(&args, &global),
        Command::Cache {
            command: CacheCommand::Reconcile(args),
        } => cache_reconcile(&args, &global),
        Command::Cache {
            command: CacheCommand::Prune(args),
        } => cache_prune(&args, &global),
        Command::Cache {
            command: CacheCommand::Clean(args),
        } => cache_clean(&args, &global),
        Command::Box {
            command: BoxCommand::Create(args),
        } => box_create(args, &global).await,
        Command::Box {
            command: BoxCommand::List(args),
        } => box_list(&args, &global),
        Command::Box {
            command: BoxCommand::Show(args),
        } => box_show(&args, &global),
        Command::Box {
            command: BoxCommand::Delete(args),
        } => box_delete(&args, &global),
        Command::Box {
            command: BoxCommand::Reset(args),
        } => box_reset(&args, &global),
        Command::Box {
            command: BoxCommand::Clone(args),
        } => box_clone(&args, &global),
        Command::Box {
            command: BoxCommand::Repair(args),
        } => box_repair(&args, &global),
        Command::Benchmark(args) => benchmark(*args, &global).await,
        Command::Completion(args) => {
            completion(&args);
            Ok(0)
        }
    };
    result.map_err(|source| CliError::for_command(stage, source))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct StorageUse {
    cache: bool,
    state: bool,
}

fn storage_use(command: &Command) -> StorageUse {
    match command {
        Command::Run(args) if args.backend == "libkrun" => StorageUse {
            cache: true,
            state: true,
        },
        Command::Image { .. } | Command::Cache { .. } => StorageUse {
            cache: true,
            state: false,
        },
        Command::Box {
            command: BoxCommand::Create(_) | BoxCommand::Reset(_),
        } => StorageUse {
            cache: true,
            state: true,
        },
        Command::Box { .. } => StorageUse {
            cache: false,
            state: true,
        },
        Command::Benchmark(args) if args.backend == "libkrun" => StorageUse {
            cache: true,
            state: true,
        },
        Command::Doctor(_) | Command::Run(_) | Command::Benchmark(_) | Command::Completion(_) => {
            StorageUse::default()
        }
    }
}

fn warn_project_local_storage(global: &GlobalOptions, usage: StorageUse) {
    let Ok(current_dir) = std::env::current_dir() else {
        return;
    };
    let Ok(defaults) = StoragePaths::for_current_user() else {
        return;
    };
    if let Some(message) = project_local_storage_warning(global, usage, &current_dir, &defaults) {
        eprintln!("warning: {message}");
    }
}

fn project_local_storage_warning(
    global: &GlobalOptions,
    usage: StorageUse,
    current_dir: &Path,
    defaults: &StoragePaths,
) -> Option<String> {
    let root = current_dir.join(".moraebox");
    let cache = root.join("cache");
    let state = root.join("state");
    let mut overrides = Vec::new();

    if usage.cache
        && global.cache_dir.is_none()
        && cache != defaults.cache()
        && fs::symlink_metadata(&cache).is_ok()
    {
        overrides.push(format!("--cache-dir {}", cache.display()));
    }
    if usage.state
        && global.state_dir.is_none()
        && state != defaults.state()
        && fs::symlink_metadata(&state).is_ok()
    {
        overrides.push(format!("--state-dir {}", state.display()));
    }

    (!overrides.is_empty()).then(|| {
        format!(
            "found project-local data at {}, but it is not selected or moved automatically; use {} to continue using it",
            root.display(),
            overrides.join(" and ")
        )
    })
}

fn completion(args: &CompletionArgs) {
    let mut command = Cli::command();
    clap_complete::generate(args.shell, &mut command, "morae", &mut io::stdout());
}

fn doctor(args: &DoctorArgs, global: &GlobalOptions) -> Result<i32, CliErrorSource> {
    let paths = NativeRuntimePaths::discover_with_gvproxy(
        global.helper.clone(),
        global.libkrun.clone(),
        global.lib_dir.clone(),
        global.gvproxy.clone(),
    );
    let cache_dir = resolve_cache_dir(global.cache_dir.as_deref())?;
    let report = DoctorReport::collect_with_paths_and_cache_with_debugfs(
        paths,
        global.mke2fs.clone(),
        global.e2fsck.clone(),
        global.debugfs.clone(),
        cache_dir,
    );
    if global.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("host: {}/{}", report.os, report.architecture);
        println!(
            "libkrun: {}",
            report
                .libkrun
                .path
                .as_ref()
                .map_or_else(|| "missing".into(), |path| path.display().to_string())
        );
        println!(
            "libkrunfw: {}",
            report
                .libkrunfw
                .path
                .as_ref()
                .map_or_else(|| "missing".into(), |path| path.display().to_string())
        );
        println!(
            "hypervisor entitlement: {}",
            if report.hypervisor_entitlement {
                "present"
            } else {
                "missing"
            }
        );
        println!(
            "vmm helper: {}",
            report
                .helper_path
                .as_ref()
                .map_or_else(|| "missing".into(), |path| path.display().to_string())
        );
        println!("native backend ready: {}", report.native_backend_ready);
        println!(
            "gvproxy: {}",
            report
                .gvproxy
                .path
                .as_ref()
                .map_or_else(|| "missing".into(), |path| path.display().to_string())
        );
        println!("native network ready: {}", report.native_network_ready);
        for check in &report.checks {
            println!("check {}: {} - {}", check.id, check.status, check.summary);
            if let Some(remediation) = &check.remediation {
                println!("  remediation: {remediation}");
            }
        }
    }
    Ok(i32::from(args.strict && !report.native_backend_ready))
}

#[allow(clippy::too_many_lines)]
async fn run(args: RunArgs, global: &GlobalOptions) -> Result<i32, CliErrorSource> {
    if args.interactive && global.json {
        return Err("--interactive cannot be combined with --json".into());
    }
    let mut spec = RunSpec::command(args.command);
    spec.box_id = args.box_id;
    spec.image_pull_policy = args.pull_policy;
    spec.timeout = parse_timeout(&args.timeout)?;
    spec.output_limit = args.output_limit;
    spec.kill_grace = args.kill_grace;
    spec.tty = args.tty;
    spec.inherit_env = args.inherit_env;
    spec.network = args.network;
    spec.cwd = args.cwd;
    spec.env = args.env.into_iter().collect::<BTreeMap<_, _>>();
    let capabilities = selected_backend_capabilities(&args.backend);
    if !args.interactive && !io::stdin().is_terminal() {
        io::stdin().read_to_end(&mut spec.stdin)?;
    }

    if args.rootfs.is_some() && args.image.is_some() {
        return Err("--rootfs and --image are mutually exclusive".into());
    }
    if spec.box_id.is_some() && !capabilities.box_persistence.is_supported() {
        return Err("--box requires --backend libkrun".into());
    }
    if spec.box_id.is_some()
        && (args.rootfs.is_some() || args.image.is_some() || args.workspace.is_some())
    {
        return Err("--box cannot be combined with --rootfs, --image, or --workspace".into());
    }
    if args.pull_policy != ImagePullPolicy::Missing
        && (args.backend != "libkrun" || args.rootfs.is_some() || spec.box_id.is_some())
    {
        return Err("--pull always|never requires an image-backed libkrun run".into());
    }
    validate_network_option(capabilities, spec.network)?;
    validate_tty_option(capabilities, spec.tty)?;
    let progress = CliProgress::new(global.json);
    let budget = RunBudget::new(spec.timeout).with_progress(move |stage| {
        progress.runtime(stage);
    });
    let cache_dir = (args.backend == "libkrun")
        .then(|| resolve_cache_dir(global.cache_dir.as_deref()))
        .transpose()?;
    let state_dir = (args.backend == "libkrun")
        .then(|| resolve_state_dir(global.state_dir.as_deref()))
        .transpose()?;
    if let Some(source) = args.workspace.as_deref() {
        if !capabilities.workspace.is_supported() {
            return Err("--workspace requires --backend libkrun".into());
        }
        if spec.cwd.is_some() {
            return Err("--cwd and --workspace cannot be combined in this version".into());
        }
        let cache_dir = cache_dir
            .as_deref()
            .ok_or("--workspace requires a cache directory")?;
        let state_dir = state_dir
            .as_ref()
            .ok_or("--workspace requires a state directory")?;
        WorkspaceSnapshot::validate_managed_roots(
            source,
            cache_dir,
            std::slice::from_ref(state_dir),
        )?;
    }
    let image_reference = if spec.box_id.is_some() {
        None
    } else {
        select_image_reference(
            &args.backend,
            args.rootfs.is_some(),
            args.image,
            cache_dir.as_deref(),
        )?
    };
    let platform = Platform::host_linux();
    let prepared_image = if let Some(reference) = image_reference.as_deref() {
        let cache_dir = cache_dir
            .as_deref()
            .ok_or("libkrun image selection requires a cache directory")?;
        Some(
            budget
                .run(
                    RunStage::ImagePull,
                    resolve_or_pull(
                        reference,
                        cache_dir,
                        &platform,
                        credentials(args.registry_username, args.registry_password),
                        args.pull_policy,
                        progress,
                    ),
                )
                .await
                .map_err(|error| io::Error::other(error.to_string()))?,
        )
    } else {
        None
    };
    let disk_tools = DiskToolPaths::discover_with_debugfs(
        global.mke2fs.clone(),
        global.e2fsck.clone(),
        global.debugfs.clone(),
    );
    let mke2fs = disk_tools.mke2fs_command();

    let workspace = if let Some(source) = args.workspace.as_deref() {
        let cache_dir = cache_dir
            .as_deref()
            .ok_or("--workspace requires a cache directory")?;
        let state_dir = state_dir
            .as_ref()
            .ok_or("--workspace requires a state directory")?;
        let workspace_timeout = budget.remaining(RunStage::WorkspacePrepare)?;
        Some(
            budget
                .observe(
                    RunStage::WorkspacePrepare,
                    WorkspaceSnapshot::create_async_with_managed_roots(
                        source,
                        cache_dir,
                        std::slice::from_ref(state_dir),
                        &mke2fs,
                        workspace_timeout,
                        move |stage| progress.workspace(stage),
                    ),
                )
                .await
                .map_err(CliErrorSource::from_stage)?,
        )
    } else {
        None
    };

    let resolved_image_digest = prepared_image
        .as_ref()
        .map(|prepared| prepared.manifest_digest.clone());
    let mut report = match args.backend.as_str() {
        "process" => {
            if args.interactive {
                return run_interactive(ProcessBackend, spec, budget).await;
            }
            Supervisor::new(ProcessBackend)
                .run_with_budget(spec, budget)
                .await?
        }
        "libkrun" => {
            let cache_dir = cache_dir
                .as_deref()
                .ok_or("libkrun backend requires a cache directory")?;
            let state_dir = state_dir
                .as_deref()
                .ok_or("libkrun backend requires a state directory")?;
            let storage = ManagedStorage::open(cache_dir, state_dir)?;
            let native = NativeSandboxConfig::discover(
                NativeRuntimeOverrides {
                    helper: global.helper.clone(),
                    libkrun: global.libkrun.clone(),
                    library_search_path: global.lib_dir.clone(),
                    gvproxy: global.gvproxy.clone(),
                },
                disk_tools,
                args.disk_size,
                args.cpus,
                args.memory_mib,
            );
            let root_source = if spec.box_id.is_some() {
                None
            } else if let Some(prepared) = prepared_image.as_ref() {
                Some(native.prepared_image_source((*prepared).clone(), &platform))
            } else if let Some(rootfs) = args.rootfs.as_ref() {
                Some(native.rootfs_source(rootfs, &platform)?)
            } else {
                return Err("libkrun run requires an image, rootfs, or BoxId".into());
            };
            let config = native.libkrun_config(
                root_source
                    .as_ref()
                    .map(|source| source.rootfs_path.clone()),
                &storage,
                workspace
                    .as_ref()
                    .map(|snapshot| snapshot.image_path.clone()),
            )?;
            let runtime = native.box_runtime(&storage, root_source);
            let mut backend = LibkrunBackend::new(config).with_box_runtime(runtime);
            if let Some(workspace) = &workspace {
                backend = backend.with_workspace_digest(workspace.image_digest.to_string());
            }
            if workspace.is_some() {
                progress.workspace_message("attaching read-only image");
            }
            if args.interactive {
                return run_interactive(backend, spec, budget).await;
            }
            Supervisor::new(backend)
                .run_with_budget(spec, budget)
                .await?
        }
        _ => unreachable!("clap validates backend values"),
    };
    report.startup.resolved_image_digest = resolved_image_digest;
    if let Some(workspace) = &workspace {
        workspace.verify_source_unchanged()?;
    }
    if global.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let mut stdout = io::stdout().lock();
        let mut stderr = io::stderr().lock();
        for chunk in &report.output {
            match chunk.channel {
                OutputChannel::Stdout | OutputChannel::Tty => stdout.write_all(&chunk.data)?,
                OutputChannel::Stderr => stderr.write_all(&chunk.data)?,
            }
        }
        stdout.flush()?;
        stderr.flush()?;
    }
    if report.timed_out {
        Ok(124)
    } else if let Some(code) = report.exit_code {
        Ok(code)
    } else if let Some(signal) = report.signal {
        Ok(128 + signal)
    } else {
        Ok(125)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CliProgress {
    enabled: bool,
}

impl CliProgress {
    fn new(json: bool) -> Self {
        Self::for_output(json, io::stderr().is_terminal())
    }

    const fn for_output(json: bool, stderr_is_terminal: bool) -> Self {
        Self {
            enabled: !json && stderr_is_terminal,
        }
    }

    fn image(self, stage: ImageProgressStage) {
        if self.enabled {
            eprintln!("morae: image: {stage}");
        }
    }

    fn workspace(self, stage: WorkspaceStage) {
        if self.enabled {
            eprintln!("morae: workspace: {stage}");
        }
    }

    fn workspace_message(self, message: &str) {
        if self.enabled {
            eprintln!("morae: workspace: {message}");
        }
    }

    fn runtime(self, stage: RunStage) {
        if let (true, Some(message)) = (self.enabled, run_stage_progress_message(stage)) {
            eprintln!("morae: runtime: {message}");
        }
    }
}

fn run_stage_progress_message(stage: RunStage) -> Option<&'static str> {
    match stage {
        RunStage::BaseDiskPrepare => Some("preparing the immutable base disk"),
        RunStage::EphemeralDiskClone => Some("cloning the ephemeral root disk"),
        RunStage::HelperSpawn => Some("spawning the microVM helper"),
        _ => None,
    }
}

fn select_image_reference(
    backend: &str,
    has_rootfs: bool,
    explicit_image: Option<String>,
    cache_dir: Option<&std::path::Path>,
) -> Result<Option<String>, CliErrorSource> {
    if backend != "libkrun" {
        if has_rootfs {
            return Err("--rootfs requires --backend libkrun".into());
        }
        if explicit_image.is_some() {
            return Err("--image requires --backend libkrun".into());
        }
        return Ok(None);
    }
    if has_rootfs {
        return Ok(None);
    }
    let cache_dir = cache_dir.ok_or("libkrun image selection requires a cache directory")?;
    explicit_image
        .map_or_else(
            || ImageCache::new(cache_dir).default_reference().map(Some),
            |reference| Ok(Some(reference)),
        )
        .map_err(Into::into)
}

async fn resolve_or_pull(
    reference: &str,
    cache_dir: &std::path::Path,
    platform: &Platform,
    credentials: Option<Credentials>,
    policy: ImagePullPolicy,
    progress: CliProgress,
) -> Result<PreparedImage, CliErrorSource> {
    ImageCache::new(cache_dir)
        .prepare_with_progress(reference, platform, credentials, policy, move |stage| {
            progress.image(stage);
        })
        .await
        .map_err(Into::into)
}

async fn box_create(args: BoxCreateArgs, global: &GlobalOptions) -> Result<i32, CliErrorSource> {
    let cache_dir = resolve_cache_dir(global.cache_dir.as_deref())?;
    let state_dir = resolve_state_dir(global.state_dir.as_deref())?;
    let cache = ImageCache::new(&cache_dir);
    let reference = match args.image {
        Some(reference) => reference,
        None => cache.default_reference()?,
    };
    let platform = Platform::host_linux();
    let progress = CliProgress::new(global.json);
    let prepared = cache
        .prepare_with_progress(
            &reference,
            &platform,
            credentials(args.registry_username, args.registry_password),
            args.pull_policy,
            move |stage| progress.image(stage),
        )
        .await?;
    let spec = BaseDiskSpec::new(
        prepared.manifest_digest.clone(),
        platform_name(&platform),
        args.disk_size,
    );
    progress.runtime(RunStage::BaseDiskPrepare);
    let base = BaseDiskStore::new(&cache_dir).prepare(
        &spec,
        &prepared.rootfs,
        &DiskToolPaths::discover_with_debugfs(
            global.mke2fs.clone(),
            global.e2fsck.clone(),
            global.debugfs.clone(),
        )
        .mke2fs_command(),
    )?;
    let metadata = BoxStore::new(state_dir).create(
        &CreateBox::new(
            prepared.manifest_digest,
            platform_name(&platform),
            args.disk_size,
        ),
        base.disk_path(),
    )?;
    print_box_result(&metadata, global.json)?;
    Ok(0)
}

fn box_list(_args: &BoxListArgs, global: &GlobalOptions) -> Result<i32, CliErrorSource> {
    let state_dir = resolve_state_dir(global.state_dir.as_deref())?;
    let report = BoxStore::new(state_dir).list()?;
    if global.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("BOX ID\tSTATE\tIMAGE DIGEST\tPLATFORM\tSIZE\tGENERATION");
        for metadata in report.boxes {
            println!(
                "{}\t{:?}\t{}\t{}\t{}\t{}",
                metadata.box_id,
                metadata.state,
                metadata.manifest_digest,
                metadata.platform,
                format_bytes(metadata.virtual_size_bytes),
                metadata.generation
            );
        }
        let has_errors = !report.errors.is_empty();
        for error in report.errors {
            eprintln!(
                "warning: Box entry {} ({:?}): {}",
                error.entry_name, error.code, error.message
            );
        }
        if has_errors {
            eprintln!("warning: run `morae box repair --dry-run` to preview quarantine actions");
        }
    }
    Ok(0)
}

fn box_show(args: &BoxShowArgs, global: &GlobalOptions) -> Result<i32, CliErrorSource> {
    let state_dir = resolve_state_dir(global.state_dir.as_deref())?;
    let metadata = BoxStore::new(state_dir).get(args.box_id)?;
    print_box_result(&metadata, global.json)?;
    Ok(0)
}

fn box_delete(args: &BoxDeleteArgs, global: &GlobalOptions) -> Result<i32, CliErrorSource> {
    debug_assert!(args.yes, "clap requires --yes");
    let state_dir = resolve_state_dir(global.state_dir.as_deref())?;
    let metadata = BoxStore::new(state_dir).delete(args.box_id)?;
    if global.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&BoxMutationReport {
                operation: "delete",
                box_id: metadata.box_id,
                generation: metadata.generation,
            })?
        );
    } else {
        println!("deleted Box {}", metadata.box_id);
    }
    Ok(0)
}

fn box_reset(args: &BoxResetArgs, global: &GlobalOptions) -> Result<i32, CliErrorSource> {
    debug_assert!(args.yes, "clap requires --yes");
    let cache_dir = resolve_cache_dir(global.cache_dir.as_deref())?;
    let state_dir = resolve_state_dir(global.state_dir.as_deref())?;
    let store = BoxStore::new(state_dir);
    let current = store.get(args.box_id)?;
    let spec = BaseDiskSpec::new(
        current.manifest_digest.clone(),
        current.platform.clone(),
        current.virtual_size_bytes,
    );
    let base = BaseDiskStore::new(cache_dir)
        .get(&spec)?
        .ok_or_else(|| {
            format!(
                "the immutable base disk for Box {} is not cached; recreate the image-backed Box instead",
                args.box_id
            )
        })?;
    let metadata = store.reset(args.box_id, base.disk_path())?;
    print_box_result(&metadata, global.json)?;
    Ok(0)
}

fn box_clone(args: &BoxCloneArgs, global: &GlobalOptions) -> Result<i32, CliErrorSource> {
    debug_assert!(args.yes, "clap requires --yes");
    let state_dir = resolve_state_dir(global.state_dir.as_deref())?;
    let metadata = BoxStore::new(state_dir).clone_box(args.box_id)?;
    print_box_result(&metadata, global.json)?;
    Ok(0)
}

fn box_repair(args: &BoxRepairArgs, global: &GlobalOptions) -> Result<i32, CliErrorSource> {
    let apply = destructive_mode(args.dry_run, args.yes, "box repair")?;
    let state_dir = resolve_state_dir(global.state_dir.as_deref())?;
    let report = BoxStore::new(state_dir).repair(apply)?;
    if global.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_box_repair_report(&report);
    }
    Ok(i32::from(!report.failures.is_empty()))
}

fn print_box_repair_report(report: &BoxRepairReport) {
    println!("corrupt entries detected: {}", report.detected.len());
    if !report.applied {
        for error in &report.detected {
            println!(
                "would quarantine {} ({:?}): {}",
                error.entry_name, error.code, error.message
            );
        }
        return;
    }
    for entry in &report.quarantined {
        println!(
            "quarantined {} at {}",
            entry.entry_name,
            entry.destination.display()
        );
    }
    for error in &report.failures {
        eprintln!(
            "warning: could not quarantine {} ({:?}): {}",
            error.entry_name, error.code, error.message
        );
    }
}

fn print_box_result(metadata: &BoxMetadata, json: bool) -> Result<(), CliErrorSource> {
    if json {
        println!("{}", serde_json::to_string_pretty(metadata)?);
    } else {
        println!("Box ID: {}", metadata.box_id);
        println!("state: {:?}", metadata.state);
        println!("manifest: {}", metadata.manifest_digest);
        println!("platform: {}", metadata.platform);
        println!("disk size: {}", format_bytes(metadata.virtual_size_bytes));
        println!("generation: {}", metadata.generation);
    }
    Ok(())
}

async fn image_pull(args: ImagePullArgs, global: &GlobalOptions) -> Result<i32, CliErrorSource> {
    let cache_dir = resolve_cache_dir(global.cache_dir.as_deref())?;
    let platform = Platform {
        os: args.os,
        architecture: args
            .architecture
            .unwrap_or_else(|| Platform::host_linux().architecture),
        variant: None,
    };
    let progress = CliProgress::new(global.json);
    let prepared = pull_and_materialize(
        &args.reference,
        &cache_dir,
        &platform,
        credentials(args.registry_username, args.registry_password),
        progress,
    )
    .await?;
    if global.json {
        println!("{}", serde_json::to_string_pretty(&prepared)?);
    } else {
        println!("reference: {}", prepared.reference);
        println!("manifest: {}", prepared.manifest_digest);
        println!("rootfs: {}", prepared.rootfs.display());
    }
    Ok(0)
}

async fn pull_and_materialize(
    reference: &str,
    cache_dir: &std::path::Path,
    platform: &Platform,
    credentials: Option<Credentials>,
    progress: CliProgress,
) -> Result<PreparedImage, CliErrorSource> {
    ImageCache::new(cache_dir)
        .pull_with_progress(reference, platform, credentials, move |stage| {
            progress.image(stage);
        })
        .await
        .map_err(Into::into)
}

fn image_list(_args: &ImageListArgs, global: &GlobalOptions) -> Result<i32, CliErrorSource> {
    let cache_dir = resolve_cache_dir(global.cache_dir.as_deref())?;
    let images = ImageCache::new(cache_dir).list()?;
    if global.json {
        println!("{}", serde_json::to_string_pretty(&images)?);
    } else {
        print_image_list(&images);
    }
    Ok(0)
}

fn image_remove(args: &ImageRemoveArgs, global: &GlobalOptions) -> Result<i32, CliErrorSource> {
    let cache_dir = resolve_cache_dir(global.cache_dir.as_deref())?;
    let report = ImageCache::new(cache_dir).remove(&args.target, !args.dry_run)?;
    if global.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_remove_report(&report);
    }
    Ok(0)
}

fn image_default(args: &ImageDefaultArgs, global: &GlobalOptions) -> Result<i32, CliErrorSource> {
    let cache_dir = resolve_cache_dir(global.cache_dir.as_deref())?;
    let cache = ImageCache::new(cache_dir);
    let reference = if args.unset {
        cache.clear_default()?;
        cache.default_reference()?
    } else if let Some(reference) = args.image.as_deref() {
        cache.set_default(reference)?
    } else {
        cache.default_reference()?
    };
    if global.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&DefaultImageReport { reference })?
        );
    } else {
        println!("default image: {reference}");
    }
    Ok(0)
}

fn print_image_list(images: &[CachedImage]) {
    println!("DEFAULT\tREFERENCE\tDIGEST\tPLATFORM\tSTATUS\tLOGICAL\tALLOCATED");
    for image in images {
        let reference = image.reference.as_deref().unwrap_or("<unknown>");
        let platform = image.platform.as_ref().map_or_else(
            || "<unknown>".into(),
            |platform| match &platform.variant {
                Some(variant) => format!("{}/{}/{}", platform.os, platform.architecture, variant),
                None => format!("{}/{}", platform.os, platform.architecture),
            },
        );
        let (logical, allocated) = if image.size_indexed {
            (
                format_bytes(image.size_bytes),
                format_bytes(image.allocated_bytes),
            )
        } else {
            ("<unindexed>".into(), "<unindexed>".into())
        };
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            if image.default { "*" } else { "" },
            reference,
            image.manifest_digest,
            platform,
            if image.ready { "ready" } else { "missing" },
            logical,
            allocated,
        );
    }
}

fn print_remove_report(report: &RemoveReport) {
    let action = if report.applied {
        "removed"
    } else {
        "would remove"
    };
    println!("target: {}", report.target);
    println!("references {action}: {}", report.references_removed.len());
    println!("rootfs {action}: {}", report.rootfs_removed.len());
    println!("space reclaimed: {}", format_bytes(report.reclaimed_bytes));
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut unit = 0;
    let mut divisor = 1_u64;
    while bytes / divisor >= 1024 && unit < UNITS.len() - 1 {
        divisor = divisor.saturating_mul(1024);
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        let whole = bytes / divisor;
        let decimal = (bytes % divisor).saturating_mul(10) / divisor;
        format!("{whole}.{decimal} {}", UNITS[unit])
    }
}

fn cache_info(_args: &CacheInfoArgs, global: &GlobalOptions) -> Result<i32, CliErrorSource> {
    let cache_dir = resolve_cache_dir(global.cache_dir.as_deref())?;
    let usage = ImageCache::new(cache_dir).usage()?;
    if global.json {
        println!("{}", serde_json::to_string_pretty(&usage)?);
    } else {
        print_cache_usage(&usage);
    }
    Ok(0)
}

fn cache_reconcile(
    args: &CacheReconcileArgs,
    global: &GlobalOptions,
) -> Result<i32, CliErrorSource> {
    let apply = destructive_mode(args.dry_run, args.yes, "cache reconcile")?;
    let cache_dir = resolve_cache_dir(global.cache_dir.as_deref())?;
    let report = ImageCache::new(cache_dir).reconcile(apply)?;
    if global.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_reconcile_report(&report);
    }
    Ok(0)
}

fn cache_prune(args: &CachePruneArgs, global: &GlobalOptions) -> Result<i32, CliErrorSource> {
    let apply = destructive_mode(args.dry_run, args.yes, "cache prune")?;
    let cache_dir = resolve_cache_dir(global.cache_dir.as_deref())?;
    let report = ImageCache::new(cache_dir).prune(apply)?;
    if global.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_prune_report(&report);
    }
    Ok(0)
}

fn cache_clean(args: &CacheCleanArgs, global: &GlobalOptions) -> Result<i32, CliErrorSource> {
    debug_assert!(args.all, "clap requires --all");
    let apply = destructive_mode(args.dry_run, args.yes, "cache clean --all")?;
    let cache_dir = resolve_cache_dir(global.cache_dir.as_deref())?;
    let report = ImageCache::new(cache_dir).clean(apply)?;
    if global.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_clean_report(&report);
    }
    Ok(0)
}

fn destructive_mode(dry_run: bool, yes: bool, command: &str) -> Result<bool, String> {
    if dry_run {
        Ok(false)
    } else if yes {
        Ok(true)
    } else {
        Err(format!(
            "{command} requires --dry-run to preview or --yes to apply"
        ))
    }
}

fn print_cache_usage(usage: &CacheUsage) {
    println!("image references: {}", usage.references);
    println!("ready rootfs images: {}", usage.images);
    println!(
        "rootfs size: {} logical, {} allocated",
        format_bytes(usage.rootfs_bytes),
        format_bytes(usage.rootfs_allocated_bytes)
    );
    println!(
        "rootfs without valid size metadata: {}",
        usage.rootfs_without_size_metadata
    );
    println!("immutable Box base disks: {}", usage.base_disks);
    println!(
        "Box base disk size: {} logical, {} allocated",
        format_bytes(usage.base_disk_bytes),
        format_bytes(usage.base_disk_allocated_bytes)
    );
    println!("OCI blobs: {}", usage.oci_blobs);
    println!(
        "OCI size: {} logical, {} allocated",
        format_bytes(usage.oci_bytes),
        format_bytes(usage.oci_allocated_bytes)
    );
    println!("workspace snapshots: {}", usage.workspaces);
    println!(
        "workspace size: {} logical, {} allocated",
        format_bytes(usage.workspace_bytes),
        format_bytes(usage.workspace_allocated_bytes)
    );
    println!(
        "total managed size: {} logical, {} allocated",
        format_bytes(usage.total_bytes),
        format_bytes(usage.total_allocated_bytes)
    );
}

fn print_reconcile_report(report: &CacheReconcileReport) {
    println!("rootfs checked: {}", report.rootfs_checked);
    println!("metadata repairs required: {}", report.repairs_required);
    println!("metadata removals required: {}", report.removals_required);
    if report.applied {
        println!("metadata written: {}", report.metadata_written);
        println!("metadata removed: {}", report.metadata_removed);
    }
    for issue in &report.issues {
        let kind = match issue.kind {
            RootfsMetadataIssueKind::Missing => "missing",
            RootfsMetadataIssueKind::Invalid => "invalid",
            RootfsMetadataIssueKind::Stale => "stale",
            RootfsMetadataIssueKind::Orphan => "orphan",
            RootfsMetadataIssueKind::IncompleteRootfs => "incomplete-rootfs",
            RootfsMetadataIssueKind::InvalidRootfsName => "invalid-rootfs-name",
        };
        let digest = issue.manifest_digest.as_deref().unwrap_or("<unknown>");
        println!("{kind}\t{digest}\t{}", issue.path.display());
    }
}

fn print_prune_report(report: &PruneReport) {
    let action = if report.applied {
        "removed"
    } else {
        "would remove"
    };
    println!("OCI blobs {action}: {}", report.oci_blobs_removed);
    println!(
        "incomplete rootfs entries {action}: {}",
        report.incomplete_rootfs_removed
    );
    println!(
        "stale image records {action}: {}",
        report.stale_records_removed
    );
    println!("space reclaimed: {}", format_bytes(report.reclaimed_bytes));
}

fn print_clean_report(report: &CleanReport) {
    let action = if report.applied {
        "removed"
    } else {
        "would remove"
    };
    println!("managed cache entries {action}: {}", report.entries_removed);
    println!("space reclaimed: {}", format_bytes(report.reclaimed_bytes));
}

fn credentials(username: Option<String>, password: Option<String>) -> Option<Credentials> {
    username
        .zip(password)
        .map(|(username, password)| Credentials { username, password })
}

async fn benchmark(args: BenchmarkArgs, global: &GlobalOptions) -> Result<i32, CliErrorSource> {
    validate_benchmark_pull_policy(&args)?;
    let command = args.command;
    let report = match args.backend.as_str() {
        "process" => {
            if args.box_id.is_some() || args.image.is_some() || args.rootfs.is_some() {
                return Err("--box, --image, and --rootfs require --backend libkrun".into());
            }
            run_benchmark(
                &Supervisor::new(ProcessBackend),
                command,
                args.iterations,
                None,
                args.output_limit,
                args.kill_grace,
            )
            .await?
        }
        "libkrun" => {
            let cache_dir = resolve_cache_dir(global.cache_dir.as_deref())?;
            let state_dir = resolve_state_dir(global.state_dir.as_deref())?;
            let platform = Platform::host_linux();
            let prepared_image = if args.box_id.is_none() && args.rootfs.is_none() {
                Some(
                    prepare_benchmark_image(
                        &cache_dir,
                        &platform,
                        args.image,
                        credentials(args.registry_username, args.registry_password),
                        args.pull_policy,
                    )
                    .await?,
                )
            } else {
                None
            };
            let disk_tools = DiskToolPaths::discover_with_debugfs(
                global.mke2fs.clone(),
                global.e2fsck.clone(),
                global.debugfs.clone(),
            );
            let storage = ManagedStorage::open(&cache_dir, &state_dir)?;
            let native = NativeSandboxConfig::discover(
                NativeRuntimeOverrides {
                    helper: global.helper.clone(),
                    libkrun: global.libkrun.clone(),
                    library_search_path: global.lib_dir.clone(),
                    gvproxy: None,
                },
                disk_tools,
                args.disk_size,
                args.cpus,
                args.memory_mib,
            );
            let digest = prepared_digest(prepared_image.as_ref());
            let root_source = if args.box_id.is_some() {
                None
            } else if let Some(prepared) = prepared_image {
                Some(native.prepared_image_source(prepared, &platform))
            } else if let Some(rootfs) = args.rootfs {
                Some(native.rootfs_source(rootfs, &platform)?)
            } else {
                return Err("libkrun benchmark requires an image, rootfs, or BoxId".into());
            };
            let config = native.libkrun_config(
                root_source
                    .as_ref()
                    .map(|source| source.rootfs_path.clone()),
                &storage,
                None,
            )?;
            let runtime = native.box_runtime(&storage, root_source);
            let prepared_roots = Arc::new(
                PreparedRootPool::new(PoolConfig::default())
                    .expect("default prepared pool config is valid"),
            );
            let mut report = run_benchmark(
                &Supervisor::new(
                    LibkrunBackend::new(config)
                        .with_box_runtime(runtime)
                        .with_prepared_pool(prepared_roots),
                ),
                command,
                args.iterations,
                args.box_id,
                args.output_limit,
                args.kill_grace,
            )
            .await?;
            report.resolved_image_digest = digest;
            report
        }
        _ => unreachable!("clap validates backend values"),
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(i32::from(report.failures > 0))
}

fn validate_benchmark_pull_policy(args: &BenchmarkArgs) -> Result<(), CliErrorSource> {
    if args.pull_policy != ImagePullPolicy::Missing
        && (args.backend != "libkrun" || args.box_id.is_some() || args.rootfs.is_some())
    {
        return Err("--pull always|never requires an image-backed libkrun benchmark".into());
    }
    Ok(())
}

async fn prepare_benchmark_image(
    cache_dir: &Path,
    platform: &Platform,
    image: Option<String>,
    credentials: Option<Credentials>,
    policy: ImagePullPolicy,
) -> Result<PreparedImage, CliErrorSource> {
    let cache = ImageCache::new(cache_dir);
    let reference = image.map_or_else(|| cache.default_reference(), Ok)?;
    cache
        .prepare(&reference, platform, credentials, policy)
        .await
        .map_err(Into::into)
}

fn prepared_digest(image: Option<&PreparedImage>) -> Option<String> {
    image.map(|image| image.manifest_digest.clone())
}

async fn run_benchmark<B: Backend>(
    supervisor: &Supervisor<B>,
    command: Vec<String>,
    iterations: u32,
    box_id: Option<BoxId>,
    output_limit: usize,
    kill_grace: Duration,
) -> Result<BenchmarkReport, CliErrorSource> {
    let mut samples = Vec::with_capacity(iterations as usize);
    let mut backend_ready_samples = Vec::with_capacity(iterations as usize);
    let mut command_start_samples = Vec::with_capacity(iterations as usize);
    let mut root_prepare_samples = Vec::new();
    let mut cache_lookup_samples = Vec::new();
    let mut box_lock_samples = Vec::new();
    let mut disk_clone_samples = Vec::new();
    let mut helper_spawn_samples = Vec::new();
    let mut prepared_ready_samples = Vec::new();
    let mut prepared_lease_samples = Vec::new();
    let mut prepared_pool_hits = 0_u32;
    let mut prepared_pool_misses = 0_u32;
    let mut failures = 0_u32;
    for _ in 0..iterations {
        let mut spec = RunSpec::command(command.clone());
        spec.box_id = box_id;
        spec.output_limit = output_limit;
        spec.kill_grace = kill_grace;
        let report = supervisor.run(spec).await?;
        if report.exit_code != Some(0) || report.timed_out {
            failures += 1;
        }
        samples.push(report.elapsed_micros);
        let command_started = report
            .trace
            .iter()
            .find(|event| event.kind == moraebox_runtime::TraceKind::CommandStarted)
            .map(|event| event.elapsed_micros);
        if let Some(command_started) = command_started {
            backend_ready_samples.push(command_started);
        }
        match report.startup.prepared_pool_hit {
            Some(true) => {
                prepared_pool_hits += 1;
                if let Some(command_started) = command_started {
                    prepared_ready_samples.push(command_started);
                }
                extend_if_some(
                    &mut prepared_lease_samples,
                    report.startup.prepared_lease_micros,
                );
            }
            Some(false) => prepared_pool_misses += 1,
            None => {}
        }
        if let Some(first_output) = report
            .trace
            .iter()
            .find(|event| event.kind == moraebox_runtime::TraceKind::FirstOutput)
        {
            command_start_samples.push(first_output.elapsed_micros);
        }
        extend_if_some(
            &mut root_prepare_samples,
            report.startup.root_prepare_micros,
        );
        extend_if_some(
            &mut cache_lookup_samples,
            report.startup.cache_lookup_micros,
        );
        extend_if_some(&mut box_lock_samples, report.startup.box_lock_micros);
        extend_if_some(&mut disk_clone_samples, report.startup.disk_clone_micros);
        extend_if_some(
            &mut helper_spawn_samples,
            report.startup.helper_spawn_micros,
        );
    }
    samples.sort_unstable();
    let backend = supervisor.backend_name();
    Ok(BenchmarkReport {
        backend: backend.into(),
        mode: benchmark_mode(supervisor.backend_capabilities()).into(),
        resolved_image_digest: None,
        iterations,
        failures,
        min_micros: samples[0],
        p50_micros: percentile(&samples, 50),
        p95_micros: percentile(&samples, 95),
        p99_micros: percentile(&samples, 99),
        max_micros: *samples.last().expect("iterations is non-zero"),
        command_start_p95_micros: optional_percentile(&mut command_start_samples, 95),
        backend_ready_p95_micros: optional_percentile(&mut backend_ready_samples, 95),
        root_prepare_p95_micros: optional_percentile(&mut root_prepare_samples, 95),
        cache_lookup_p95_micros: optional_percentile(&mut cache_lookup_samples, 95),
        box_lock_p95_micros: optional_percentile(&mut box_lock_samples, 95),
        disk_clone_p95_micros: optional_percentile(&mut disk_clone_samples, 95),
        helper_spawn_p95_micros: optional_percentile(&mut helper_spawn_samples, 95),
        prepared_pool_hits,
        prepared_pool_misses,
        prepared_ready_p50_micros: optional_percentile(&mut prepared_ready_samples, 50),
        prepared_ready_p95_micros: optional_percentile(&mut prepared_ready_samples, 95),
        prepared_ready_p99_micros: optional_percentile(&mut prepared_ready_samples, 99),
        prepared_lease_p50_micros: optional_percentile(&mut prepared_lease_samples, 50),
        prepared_lease_p95_micros: optional_percentile(&mut prepared_lease_samples, 95),
        prepared_lease_p99_micros: optional_percentile(&mut prepared_lease_samples, 99),
    })
}

fn benchmark_mode(capabilities: BackendCapabilities) -> &'static str {
    match capabilities.isolation {
        IsolationLevel::MicroVm => "cached-one-shot",
        IsolationLevel::HostProcess => "host-process",
    }
}

fn extend_if_some(samples: &mut Vec<u64>, value: Option<u64>) {
    if let Some(value) = value {
        samples.push(value);
    }
}

fn optional_percentile(samples: &mut [u64], percentile_value: usize) -> Option<u64> {
    if samples.is_empty() {
        None
    } else {
        samples.sort_unstable();
        Some(percentile(samples, percentile_value))
    }
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    let rank = (sorted.len() * percentile).div_ceil(100).saturating_sub(1);
    sorted[rank.min(sorted.len() - 1)]
}

#[derive(Debug, Serialize)]
struct DefaultImageReport {
    reference: String,
}

#[derive(Debug, Serialize)]
struct BoxMutationReport {
    operation: &'static str,
    box_id: BoxId,
    generation: u64,
}

#[derive(Debug, Serialize)]
struct BenchmarkReport {
    backend: String,
    mode: String,
    resolved_image_digest: Option<String>,
    iterations: u32,
    failures: u32,
    min_micros: u64,
    p50_micros: u64,
    p95_micros: u64,
    p99_micros: u64,
    max_micros: u64,
    command_start_p95_micros: Option<u64>,
    backend_ready_p95_micros: Option<u64>,
    root_prepare_p95_micros: Option<u64>,
    cache_lookup_p95_micros: Option<u64>,
    box_lock_p95_micros: Option<u64>,
    disk_clone_p95_micros: Option<u64>,
    helper_spawn_p95_micros: Option<u64>,
    prepared_pool_hits: u32,
    prepared_pool_misses: u32,
    prepared_ready_p50_micros: Option<u64>,
    prepared_ready_p95_micros: Option<u64>,
    prepared_ready_p99_micros: Option<u64>,
    prepared_lease_p50_micros: Option<u64>,
    prepared_lease_p95_micros: Option<u64>,
    prepared_lease_p99_micros: Option<u64>,
}

fn platform_name(platform: &Platform) -> String {
    match &platform.variant {
        Some(variant) => format!("{}/{}/{}", platform.os, platform.architecture, variant),
        None => format!("{}/{}", platform.os, platform.architecture),
    }
}

pub(super) fn parse_disk_size(input: &str) -> Result<u64, String> {
    parse_byte_size(input, "disk size")
}

pub(super) fn parse_output_limit(input: &str) -> Result<usize, String> {
    let bytes = parse_byte_size(input, "output limit")?;
    let bytes = usize::try_from(bytes).map_err(|_| "output limit is too large".to_owned())?;
    if bytes > MAX_OUTPUT_LIMIT {
        return Err("output limit must not exceed 1 GiB".into());
    }
    Ok(bytes)
}

fn parse_byte_size(input: &str, label: &str) -> Result<u64, String> {
    let input = input.trim();
    let split = input
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(input.len());
    let (number, suffix) = input.split_at(split);
    let value = number
        .parse::<u64>()
        .map_err(|_| format!("{label} must start with a positive integer"))?;
    if value == 0 {
        return Err(format!("{label} must be greater than zero"));
    }
    let multiplier = match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kib" => 1024,
        "mib" => 1024 * 1024,
        "gib" => 1024 * 1024 * 1024,
        "kb" => 1000,
        "mb" => 1000 * 1000,
        "gb" => 1000 * 1000 * 1000,
        _ => {
            return Err(format!(
                "{label} suffix must be B, KiB, MiB, GiB, KB, MB, or GB"
            ));
        }
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("{label} is too large"))
}

pub(super) fn parse_kill_grace(input: &str) -> Result<Duration, String> {
    let duration = humantime::parse_duration(input).map_err(|error| error.to_string())?;
    if duration.is_zero() {
        return Err("kill grace must be greater than zero".into());
    }
    if duration > MAX_KILL_GRACE {
        return Err("kill grace must not exceed 60 seconds".into());
    }
    Ok(duration)
}

fn parse_timeout(input: &str) -> Result<TimeoutPolicy, CliErrorSource> {
    if input.eq_ignore_ascii_case("none") || input == "0" {
        return Ok(TimeoutPolicy::Unlimited);
    }
    let duration: Duration = humantime::parse_duration(input).map_err(|error| error.to_string())?;
    let milliseconds = u64::try_from(duration.as_millis()).map_err(|error| error.to_string())?;
    if milliseconds == 0 {
        return Err("timeout must be non-zero or 'none'".into());
    }
    Ok(TimeoutPolicy::Limited(milliseconds))
}

fn selected_backend_capabilities(backend: &str) -> BackendCapabilities {
    match backend {
        "process" => ProcessBackend::CAPABILITIES,
        "libkrun" => LibkrunBackend::CAPABILITIES,
        _ => unreachable!("clap validates backend values"),
    }
}

fn validate_network_option(
    capabilities: BackendCapabilities,
    network: bool,
) -> Result<(), &'static str> {
    if network && !capabilities.network.is_supported() {
        return Err("--network requires --backend libkrun");
    }
    Ok(())
}

fn validate_tty_option(capabilities: BackendCapabilities, tty: bool) -> Result<(), &'static str> {
    if tty && !capabilities.tty.is_supported() {
        return Err("--tty requires a backend with TTY support");
    }
    Ok(())
}

pub(super) fn parse_env(input: &str) -> Result<(String, String), String> {
    let Some((key, value)) = input.split_once('=') else {
        return Err("environment values must use KEY=VALUE".into());
    };
    if key.is_empty() || key.contains('\0') || value.contains('\0') {
        return Err("environment keys and values must be non-empty and NUL-free".into());
    }
    Ok((key.to_owned(), value.to_owned()))
}

pub(super) fn exit_code(code: i32) -> ExitCode {
    let clamped = code.clamp(0, 255);
    ExitCode::from(u8::try_from(clamped).expect("value was clamped to u8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize_help_alias;
    use clap::Parser;
    use clap_complete::Shell;
    use std::path::PathBuf;

    #[test]
    fn parses_timeout_and_unlimited() {
        assert_eq!(
            parse_timeout("1h").unwrap(),
            TimeoutPolicy::Limited(3_600_000)
        );
        assert_eq!(parse_timeout("none").unwrap(), TimeoutPolicy::Unlimited);
    }

    #[test]
    fn parses_bounded_output_and_kill_grace_controls() {
        assert_eq!(parse_output_limit("8MiB").unwrap(), 8 * 1024 * 1024);
        assert!(parse_output_limit("0").is_err());
        assert!(parse_output_limit("2GiB").is_err());
        assert_eq!(
            parse_kill_grace("250ms").unwrap(),
            Duration::from_millis(250)
        );
        assert!(parse_kill_grace("0s").is_err());
        assert!(parse_kill_grace("61s").is_err());

        let defaults = Cli::try_parse_from(["morae", "run", "--", "/bin/true"]).unwrap();
        let Command::Run(defaults) = defaults.command else {
            panic!("expected run command");
        };
        assert_eq!(defaults.output_limit, moraebox_core::DEFAULT_OUTPUT_LIMIT);
        assert_eq!(defaults.kill_grace, moraebox_core::DEFAULT_KILL_GRACE);

        let explicit = Cli::try_parse_from([
            "morae",
            "run",
            "--output-limit",
            "4KiB",
            "--kill-grace",
            "750ms",
            "--",
            "/bin/true",
        ])
        .unwrap();
        let Command::Run(explicit) = explicit.command else {
            panic!("expected run command");
        };
        assert_eq!(explicit.output_limit, 4096);
        assert_eq!(explicit.kill_grace, Duration::from_millis(750));
    }

    #[test]
    fn parses_image_pull_policy_for_image_backed_commands() {
        let default = Cli::try_parse_from(["morae", "run", "--", "/bin/true"]).unwrap();
        let Command::Run(default) = default.command else {
            panic!("expected run command");
        };
        assert_eq!(default.pull_policy, ImagePullPolicy::Missing);

        let run =
            Cli::try_parse_from(["morae", "run", "--pull", "never", "--", "/bin/true"]).unwrap();
        let Command::Run(run) = run.command else {
            panic!("expected run command");
        };
        assert_eq!(run.pull_policy, ImagePullPolicy::Never);

        let create = Cli::try_parse_from(["morae", "box", "create", "--pull", "always"]).unwrap();
        let Command::Box {
            command: BoxCommand::Create(create),
        } = create.command
        else {
            panic!("expected box create command");
        };
        assert_eq!(create.pull_policy, ImagePullPolicy::Always);

        assert!(
            Cli::try_parse_from([
                "morae",
                "benchmark",
                "--pull",
                "sometimes",
                "--",
                "/bin/true",
            ])
            .is_err()
        );
    }

    #[test]
    fn parses_network_as_an_explicit_opt_in() {
        let default = Cli::try_parse_from(["morae", "run", "--", "/usr/bin/true"]).unwrap();
        let Command::Run(default) = default.command else {
            panic!("expected run command");
        };
        assert!(!default.network);

        let enabled = Cli::try_parse_from([
            "morae",
            "run",
            "--network",
            "--gvproxy",
            "/opt/tools/gvproxy",
            "--",
            "/usr/bin/true",
        ])
        .unwrap();
        assert_eq!(
            enabled.global.gvproxy,
            Some(PathBuf::from("/opt/tools/gvproxy"))
        );
        let Command::Run(enabled) = enabled.command else {
            panic!("expected run command");
        };
        assert!(enabled.network);
    }

    #[test]
    fn user_execution_commands_default_to_libkrun() {
        let run = Cli::try_parse_from(["morae", "run", "--", "/bin/true"]).unwrap();
        let Command::Run(run) = run.command else {
            panic!("expected run command");
        };
        assert_eq!(run.backend, "libkrun");

        let benchmark = Cli::try_parse_from(["morae", "benchmark", "--", "/bin/true"]).unwrap();
        let Command::Benchmark(benchmark) = benchmark.command else {
            panic!("expected benchmark command");
        };
        assert_eq!(benchmark.backend, "libkrun");

        let process = Cli::try_parse_from([
            "morae",
            "benchmark",
            "--backend",
            "process",
            "--",
            "/bin/true",
        ])
        .unwrap();
        let Command::Benchmark(process) = process.command else {
            panic!("expected benchmark command");
        };
        assert_eq!(process.backend, "process");
    }

    #[test]
    fn storage_paths_are_optional_explicit_overrides() {
        let default = Cli::try_parse_from(["morae", "run", "--", "/bin/true"]).unwrap();
        assert!(default.global.cache_dir.is_none());
        assert!(default.global.state_dir.is_none());

        let after_subcommand = Cli::try_parse_from([
            "morae",
            "run",
            "--cache-dir",
            "custom-cache",
            "--state-dir",
            "custom-state",
            "--",
            "/bin/true",
        ])
        .unwrap();
        assert_eq!(
            after_subcommand.global.cache_dir,
            Some("custom-cache".into())
        );
        assert_eq!(
            after_subcommand.global.state_dir,
            Some("custom-state".into())
        );

        let before_subcommand = Cli::try_parse_from([
            "morae",
            "--cache-dir",
            "custom-cache",
            "--state-dir",
            "custom-state",
            "run",
            "--",
            "/bin/true",
        ])
        .unwrap();
        assert_eq!(
            before_subcommand.global.cache_dir,
            Some("custom-cache".into())
        );
        assert_eq!(
            before_subcommand.global.state_dir,
            Some("custom-state".into())
        );
    }

    #[test]
    fn project_local_storage_warning_is_scoped_to_used_default_roots() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join(".moraebox/cache")).unwrap();
        fs::create_dir_all(project.path().join(".moraebox/state")).unwrap();
        let defaults = StoragePaths::from_home(project.path().join("home")).unwrap();
        let usage = StorageUse {
            cache: true,
            state: true,
        };

        let warning = project_local_storage_warning(
            &GlobalOptions::default(),
            usage,
            project.path(),
            &defaults,
        )
        .unwrap();
        assert!(warning.contains("--cache-dir"));
        assert!(warning.contains("--state-dir"));
        assert!(warning.contains("not selected or moved automatically"));

        let explicit_cache = GlobalOptions {
            cache_dir: Some(project.path().join("explicit-cache")),
            ..GlobalOptions::default()
        };
        let warning =
            project_local_storage_warning(&explicit_cache, usage, project.path(), &defaults)
                .unwrap();
        assert!(!warning.contains("--cache-dir"));
        assert!(warning.contains("--state-dir"));

        let same_as_default = StoragePaths::from_home(project.path()).unwrap();
        assert!(
            project_local_storage_warning(
                &GlobalOptions::default(),
                usage,
                project.path(),
                &same_as_default,
            )
            .is_none()
        );
    }

    #[test]
    fn storage_warning_skips_commands_that_do_not_use_managed_storage() {
        let doctor = Cli::try_parse_from(["morae", "doctor"]).unwrap();
        assert_eq!(storage_use(&doctor.command), StorageUse::default());

        let process =
            Cli::try_parse_from(["morae", "run", "--backend", "process", "--", "/bin/true"])
                .unwrap();
        assert_eq!(storage_use(&process.command), StorageUse::default());

        let image = Cli::try_parse_from(["morae", "image", "list"]).unwrap();
        assert_eq!(
            storage_use(&image.command),
            StorageUse {
                cache: true,
                state: false,
            }
        );
    }

    #[test]
    fn completion_includes_commands_and_global_options() {
        let mut command = Cli::command();
        let mut output = Vec::new();
        clap_complete::generate(Shell::Bash, &mut command, "morae", &mut output);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("completion"));
        assert!(output.contains("--cache-dir"));
        assert!(output.contains("--json"));
    }

    #[test]
    fn parses_environment_without_shell_expansion() {
        assert_eq!(
            parse_env("A=hello world").unwrap(),
            ("A".into(), "hello world".into())
        );
        assert!(parse_env("MISSING").is_err());
    }

    #[test]
    fn run_help_alias_preserves_literal_help_command() {
        for args in [["morae", "run", "help"], ["morae", "run", "--help"]] {
            let error = Cli::try_parse_from(normalize_help_alias(args)).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
            assert!(
                error
                    .to_string()
                    .contains("Usage: morae run [OPTIONS] <COMMAND>...")
            );
        }

        let cli =
            Cli::try_parse_from(normalize_help_alias(["morae", "run", "--", "help"])).unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(args.command, ["help"]);
    }

    #[test]
    fn parses_image_management_aliases() {
        let list = Cli::try_parse_from(["morae", "image", "ls", "--json"]).unwrap();
        assert!(list.global.json);
        let Command::Image {
            command: ImageCommand::List(_args),
        } = list.command
        else {
            panic!("expected image list command");
        };

        let remove =
            Cli::try_parse_from(["morae", "image", "rm", "python:3.12", "--dry-run"]).unwrap();
        let Command::Image {
            command: ImageCommand::Remove(args),
        } = remove.command
        else {
            panic!("expected image remove command");
        };
        assert_eq!(args.target, "python:3.12");
        assert!(args.dry_run);

        let default = Cli::try_parse_from(["morae", "image", "default", "python:3.13"]).unwrap();
        let Command::Image {
            command: ImageCommand::Default(args),
        } = default.command
        else {
            panic!("expected image default command");
        };
        assert_eq!(args.image.as_deref(), Some("python:3.13"));
        assert!(!args.unset);
    }

    #[test]
    fn formats_cache_sizes_for_humans() {
        assert_eq!(format_bytes(42), "42 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
    }

    #[test]
    fn reports_only_user_visible_runtime_preparation_stages() {
        assert_eq!(
            run_stage_progress_message(RunStage::BaseDiskPrepare),
            Some("preparing the immutable base disk")
        );
        assert_eq!(
            run_stage_progress_message(RunStage::EphemeralDiskClone),
            Some("cloning the ephemeral root disk")
        );
        assert_eq!(
            run_stage_progress_message(RunStage::HelperSpawn),
            Some("spawning the microVM helper")
        );
        assert_eq!(run_stage_progress_message(RunStage::CacheLookup), None);
        assert_eq!(run_stage_progress_message(RunStage::CommandRun), None);
    }

    #[test]
    fn progress_is_quiet_for_json_and_non_terminal_stderr() {
        assert_eq!(
            CliProgress::for_output(false, true),
            CliProgress { enabled: true }
        );
        assert_eq!(
            CliProgress::for_output(true, true),
            CliProgress { enabled: false }
        );
        assert_eq!(
            CliProgress::for_output(false, false),
            CliProgress { enabled: false }
        );
    }

    #[test]
    fn parses_cache_management_and_requires_all_for_clean() {
        let reconcile = Cli::try_parse_from(["morae", "cache", "reconcile", "--dry-run"]).unwrap();
        let Command::Cache {
            command: CacheCommand::Reconcile(args),
        } = reconcile.command
        else {
            panic!("expected cache reconcile command");
        };
        assert!(args.dry_run);

        let repair = Cli::try_parse_from(["morae", "cache", "repair", "--yes"]).unwrap();
        let Command::Cache {
            command: CacheCommand::Reconcile(args),
        } = repair.command
        else {
            panic!("expected cache repair alias");
        };
        assert!(args.yes);

        let prune = Cli::try_parse_from(["morae", "cache", "prune", "--dry-run"]).unwrap();
        let Command::Cache {
            command: CacheCommand::Prune(args),
        } = prune.command
        else {
            panic!("expected cache prune command");
        };
        assert!(args.dry_run);

        let clean = Cli::try_parse_from(["morae", "cache", "clean", "--all", "--yes"]).unwrap();
        let Command::Cache {
            command: CacheCommand::Clean(args),
        } = clean.command
        else {
            panic!("expected cache clean command");
        };
        assert!(args.all);
        assert!(args.yes);

        assert!(Cli::try_parse_from(["morae", "cache", "clean", "--yes"]).is_err());
    }

    #[test]
    fn destructive_cache_operations_require_an_explicit_mode() {
        assert!(!destructive_mode(true, false, "cache prune").unwrap());
        assert!(destructive_mode(false, true, "cache prune").unwrap());
        assert!(destructive_mode(false, false, "cache prune").is_err());
        assert!(destructive_mode(false, false, "cache reconcile").is_err());
    }

    #[test]
    fn parses_box_management_and_run_selection() {
        let create = Cli::try_parse_from([
            "morae",
            "box",
            "create",
            "--image",
            "alpine:latest",
            "--disk-size",
            "512MiB",
            "--json",
        ])
        .unwrap();
        assert!(create.global.json);
        let Command::Box {
            command: BoxCommand::Create(create),
        } = create.command
        else {
            panic!("expected box create command");
        };
        assert_eq!(create.disk_size, 512 * 1024 * 1024);

        let repair = Cli::try_parse_from(["morae", "box", "repair", "--dry-run"]).unwrap();
        let Command::Box {
            command: BoxCommand::Repair(repair),
        } = repair.command
        else {
            panic!("expected box repair command");
        };
        assert!(repair.dry_run);
        assert!(!repair.yes);
        assert!(Cli::try_parse_from(["morae", "box", "repair", "--dry-run", "--yes"]).is_err());

        let box_id = BoxId::new();
        let run = Cli::try_parse_from([
            "morae",
            "run",
            "--box",
            &box_id.to_string(),
            "--",
            "/bin/true",
        ])
        .unwrap();
        let Command::Run(run) = run.command else {
            panic!("expected run command");
        };
        assert_eq!(run.box_id, Some(box_id));

        assert!(
            Cli::try_parse_from([
                "morae",
                "run",
                "--box",
                &box_id.to_string(),
                "--image",
                "alpine:latest",
                "--",
                "/bin/true",
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["morae", "box", "delete", &box_id.to_string()]).is_err());
        assert!(Cli::try_parse_from(["morae", "box", "reset", &box_id.to_string()]).is_err());
        assert!(Cli::try_parse_from(["morae", "box", "clone", &box_id.to_string()]).is_err());
    }

    #[test]
    fn parses_disk_size_units_and_rejects_zero() {
        assert_eq!(parse_disk_size("8GiB").unwrap(), 8 * 1024 * 1024 * 1024);
        assert_eq!(parse_disk_size("500MB").unwrap(), 500_000_000);
        assert!(parse_disk_size("0").is_err());
        assert!(parse_disk_size("1TB").is_err());
    }

    #[test]
    fn selects_explicit_rootfs_then_image_then_python_default() {
        let cache_dir =
            std::env::temp_dir().join(format!("moraebox-cli-default-image-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache_dir);

        assert_eq!(
            select_image_reference("process", false, None, Some(&cache_dir)).unwrap(),
            None
        );
        assert_eq!(
            select_image_reference("process", true, None, Some(&cache_dir))
                .unwrap_err()
                .to_string(),
            "--rootfs requires --backend libkrun"
        );
        assert_eq!(
            select_image_reference("libkrun", true, None, Some(&cache_dir)).unwrap(),
            None
        );
        assert_eq!(
            select_image_reference(
                "libkrun",
                false,
                Some("debian:bookworm".into()),
                Some(&cache_dir),
            )
            .unwrap()
            .as_deref(),
            Some("debian:bookworm")
        );
        assert_eq!(
            select_image_reference("libkrun", false, None, Some(&cache_dir))
                .unwrap()
                .as_deref(),
            Some("docker.io/library/python:3.12")
        );

        ImageCache::new(&cache_dir)
            .set_default("debian:bookworm")
            .unwrap();
        assert_eq!(
            select_image_reference("libkrun", false, None, Some(&cache_dir))
                .unwrap()
                .as_deref(),
            Some("docker.io/library/debian:bookworm")
        );
        std::fs::remove_dir_all(cache_dir).unwrap();
    }

    #[test]
    fn benchmark_modes_distinguish_microvms_from_host_processes() {
        assert_eq!(
            benchmark_mode(LibkrunBackend::CAPABILITIES),
            "cached-one-shot"
        );
        assert_eq!(benchmark_mode(ProcessBackend::CAPABILITIES), "host-process");
    }

    #[test]
    fn prepared_benchmark_percentiles_use_only_available_warm_samples() {
        let mut samples = [50, 10, 40, 20, 30];
        assert_eq!(optional_percentile(&mut samples, 50), Some(30));
        assert_eq!(optional_percentile(&mut samples, 95), Some(50));
        assert_eq!(optional_percentile(&mut samples, 99), Some(50));
        assert_eq!(optional_percentile(&mut [], 50), None);
    }
}
