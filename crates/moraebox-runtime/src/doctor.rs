use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::libkrun::{
    NETWORK_PROXY_STDERR_LIMIT, append_bounded_tail, probe_vfkit_endpoint, stderr_diagnostics,
};

const REQUIRED_LIBKRUN_SYMBOLS: &[&str] = &[
    "krun_create_ctx",
    "krun_free_ctx",
    "krun_set_vm_config",
    "krun_add_virtiofs3",
    "krun_set_exec",
    "krun_set_workdir",
    "krun_add_disk",
    "krun_set_root_disk_remount",
    "krun_add_vsock",
    "krun_add_vsock_port",
    "krun_add_virtio_console_default",
    "krun_start_enter",
];
const LIBKRUN_CANDIDATES: &[&str] = &[
    "/opt/homebrew/lib/libkrun.dylib",
    "/opt/homebrew/opt/libkrun/lib/libkrun.dylib",
    "/usr/local/lib/libkrun.dylib",
];
const LIBKRUNFW_CANDIDATES: &[&str] = &[
    "/opt/homebrew/lib/libkrunfw.dylib",
    "/opt/homebrew/opt/libkrunfw/lib/libkrunfw.dylib",
    "/usr/local/lib/libkrunfw.dylib",
];
const GVPROXY_CANDIDATES: &[&str] = &[
    "/opt/homebrew/bin/gvproxy",
    "/usr/local/bin/gvproxy",
    "/usr/bin/gvproxy",
];
const MKE2FS_CANDIDATES: &[&str] = &[
    "/opt/homebrew/opt/e2fsprogs/sbin/mke2fs",
    "/usr/local/opt/e2fsprogs/sbin/mke2fs",
    "/usr/local/sbin/mke2fs",
    "/usr/sbin/mke2fs",
];
const E2FSCK_CANDIDATES: &[&str] = &[
    "/opt/homebrew/opt/e2fsprogs/sbin/e2fsck",
    "/usr/local/opt/e2fsprogs/sbin/e2fsck",
    "/usr/local/sbin/e2fsck",
    "/usr/sbin/e2fsck",
];
const DEBUGFS_CANDIDATES: &[&str] = &[
    "/opt/homebrew/opt/e2fsprogs/sbin/debugfs",
    "/usr/local/opt/e2fsprogs/sbin/debugfs",
    "/usr/local/sbin/debugfs",
    "/usr/sbin/debugfs",
];
const EXPECTED_LIBKRUN_VERSION: &str = "1.19.4";
const EXPECTED_LIBKRUNFW_VERSION: &str = "5.5.0";
const MIN_RECOMMENDED_CACHE_FREE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const NETWORK_SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
const NETWORK_SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(50);
const NETWORK_STDERR_FINISH_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct DoctorReport {
    pub expected_libkrun_version: String,
    pub expected_libkrunfw_version: String,
    pub os: String,
    pub architecture: String,
    pub os_version: Option<String>,
    pub host_supported: bool,
    pub hypervisor_framework: bool,
    pub helper_path: Option<PathBuf>,
    pub hypervisor_entitlement: bool,
    #[serde(default)]
    pub helper: NativeBinaryProbe,
    pub libkrun: LibraryProbe,
    pub libkrunfw: LibraryProbe,
    pub krunvm: ToolProbe,
    pub gvproxy: ToolProbe,
    pub mke2fs: ToolProbe,
    pub e2fsck: ToolProbe,
    #[serde(default)]
    pub debugfs: ToolProbe,
    #[serde(default)]
    pub cache_volume: CacheVolumeProbe,
    #[serde(default)]
    pub network: NetworkProbe,
    pub cow_clone_supported: Option<bool>,
    pub libkrun_network_api: Option<bool>,
    pub native_backend_ready: bool,
    pub native_network_ready: bool,
    #[serde(default)]
    pub checks: Vec<DoctorCheck>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LibraryProbe {
    pub found: bool,
    pub path: Option<PathBuf>,
    pub required_symbols_present: Option<bool>,
    pub missing_symbols: Vec<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub version_matches: Option<bool>,
    #[serde(default)]
    pub architecture: Option<String>,
    #[serde(default)]
    pub architecture_matches: Option<bool>,
    #[serde(default)]
    pub code_signature_valid: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NativeBinaryProbe {
    pub path: Option<PathBuf>,
    pub regular_file: bool,
    pub executable: Option<bool>,
    pub architecture: Option<String>,
    pub architecture_matches: Option<bool>,
    pub code_signature_valid: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheVolumeProbe {
    pub configured_path: PathBuf,
    pub probe_path: Option<PathBuf>,
    pub available_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub minimum_recommended_free_bytes: u64,
    pub free_space_sufficient: Option<bool>,
    pub reflink_supported: Option<bool>,
    pub free_space_error: Option<String>,
    pub reflink_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkProbe {
    pub helper_executable: bool,
    pub helper_architecture: Option<String>,
    pub helper_architecture_matches: Option<bool>,
    pub socket_created: Option<bool>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorCheckStatus {
    Pass,
    #[default]
    Warn,
    Fail,
}

impl std::fmt::Display for DoctorCheckStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Fail => "fail",
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub id: String,
    pub status: DoctorCheckStatus,
    pub summary: String,
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolProbe {
    pub found: bool,
    pub path: Option<PathBuf>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeRuntimePaths {
    pub helper: Option<PathBuf>,
    pub libkrun: Option<PathBuf>,
    pub libkrunfw: Option<PathBuf>,
    pub gvproxy: Option<PathBuf>,
    pub library_search_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskToolPaths {
    pub mke2fs: Option<PathBuf>,
    pub e2fsck: Option<PathBuf>,
    pub debugfs: Option<PathBuf>,
}

impl DiskToolPaths {
    /// Resolve disk tools without replacing an explicit caller path.
    pub fn discover(mke2fs: Option<PathBuf>, e2fsck: Option<PathBuf>) -> Self {
        Self::discover_with_debugfs(mke2fs, e2fsck, None)
    }

    /// Resolve disk tools, including the trusted-agent injection utility.
    pub fn discover_with_debugfs(
        mke2fs: Option<PathBuf>,
        e2fsck: Option<PathBuf>,
        debugfs: Option<PathBuf>,
    ) -> Self {
        Self {
            mke2fs: resolve_tool_path(mke2fs, "MORAE_MKE2FS", "mke2fs", MKE2FS_CANDIDATES),
            e2fsck: resolve_tool_path(e2fsck, "MORAE_E2FSCK", "e2fsck", E2FSCK_CANDIDATES),
            debugfs: resolve_tool_path(debugfs, "MORAE_DEBUGFS", "debugfs", DEBUGFS_CANDIDATES),
        }
    }

    pub fn mke2fs_command(&self) -> PathBuf {
        self.mke2fs
            .clone()
            .unwrap_or_else(|| PathBuf::from("mke2fs"))
    }

    pub fn e2fsck_command(&self) -> PathBuf {
        self.e2fsck
            .clone()
            .unwrap_or_else(|| PathBuf::from("e2fsck"))
    }

    pub fn debugfs_command(&self) -> PathBuf {
        self.debugfs
            .clone()
            .unwrap_or_else(|| PathBuf::from("debugfs"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("native runtime preflight failed: {details}")]
pub(crate) struct NativeRuntimePreflightError {
    details: String,
}

impl NativeRuntimePaths {
    /// Resolve native runtime paths without overriding explicit caller configuration.
    pub fn discover(
        helper: Option<PathBuf>,
        libkrun: Option<PathBuf>,
        library_search_path: Option<PathBuf>,
    ) -> Self {
        Self::discover_with_gvproxy(helper, libkrun, library_search_path, None)
    }

    /// Resolve native runtime and optional gvproxy paths without overriding caller configuration.
    pub fn discover_with_gvproxy(
        helper: Option<PathBuf>,
        libkrun: Option<PathBuf>,
        library_search_path: Option<PathBuf>,
        gvproxy: Option<PathBuf>,
    ) -> Self {
        let helper = helper
            .or_else(|| configured_path("MORAE_HELPER_PATH"))
            .or_else(find_sibling_helper);
        let libkrun = libkrun
            .or_else(|| configured_path("MORAE_LIBKRUN_PATH"))
            .or_else(|| find_candidate(LIBKRUN_CANDIDATES));
        let libkrunfw = configured_path("MORAE_LIBKRUNFW_PATH")
            .or_else(|| find_sibling_library(libkrun.as_deref(), "libkrunfw.dylib"))
            .or_else(|| find_candidate(LIBKRUNFW_CANDIDATES));
        let library_search_path = library_search_path
            .or_else(|| configured_path("MORAE_LIB_DIR"))
            .or_else(|| library_parent_path(libkrun.as_deref(), libkrunfw.as_deref()));
        let gvproxy = gvproxy
            .or_else(|| configured_path("MORAE_GVPROXY_PATH"))
            .or_else(|| find_in_path("gvproxy"))
            .or_else(|| find_candidate(GVPROXY_CANDIDATES));
        Self {
            helper,
            libkrun,
            libkrunfw,
            gvproxy,
            library_search_path,
        }
    }
}

impl DoctorReport {
    pub fn collect() -> Self {
        let paths = NativeRuntimePaths::discover_with_gvproxy(None, None, None, None);
        let cache_dir = moraebox_core::resolve_cache_dir(None).unwrap_or_else(|_| env::temp_dir());
        Self::collect_with_paths_and_cache_with_debugfs(
            paths,
            configured_path("MORAE_MKE2FS"),
            configured_path("MORAE_E2FSCK"),
            configured_path("MORAE_DEBUGFS"),
            cache_dir,
        )
    }

    pub fn collect_with_paths(
        paths: NativeRuntimePaths,
        mke2fs_override: Option<PathBuf>,
        e2fsck_override: Option<PathBuf>,
    ) -> Self {
        Self::collect_with_paths_and_debugfs(paths, mke2fs_override, e2fsck_override, None)
    }

    pub fn collect_with_paths_and_debugfs(
        paths: NativeRuntimePaths,
        mke2fs_override: Option<PathBuf>,
        e2fsck_override: Option<PathBuf>,
        debugfs_override: Option<PathBuf>,
    ) -> Self {
        Self::collect_with_paths_and_cache_with_debugfs(
            paths,
            mke2fs_override,
            e2fsck_override,
            debugfs_override,
            env::temp_dir(),
        )
    }

    pub fn collect_with_paths_and_cache(
        paths: NativeRuntimePaths,
        mke2fs_override: Option<PathBuf>,
        e2fsck_override: Option<PathBuf>,
        cache_dir: PathBuf,
    ) -> Self {
        Self::collect_with_paths_and_cache_with_debugfs(
            paths,
            mke2fs_override,
            e2fsck_override,
            None,
            cache_dir,
        )
    }

    #[expect(
        clippy::too_many_lines,
        reason = "doctor assembles one stable report from all native capability probes"
    )]
    pub fn collect_with_paths_and_cache_with_debugfs(
        paths: NativeRuntimePaths,
        mke2fs_override: Option<PathBuf>,
        e2fsck_override: Option<PathBuf>,
        debugfs_override: Option<PathBuf>,
        cache_dir: PathBuf,
    ) -> Self {
        let os = env::consts::OS.to_owned();
        let architecture = env::consts::ARCH.to_owned();
        let os_version = command_output("sw_vers", &["-productVersion"]);
        let host_supported = os == "macos" && architecture == "aarch64";
        // On current macOS releases the binary may live in the dyld shared cache,
        // leaving a dangling-looking compatibility symlink in the framework bundle.
        let hypervisor_framework =
            Path::new("/System/Library/Frameworks/Hypervisor.framework").is_dir();
        let helper_path = paths.helper.clone();
        let helper = probe_native_binary(helper_path.as_deref(), &architecture, true);
        let hypervisor_entitlement = helper_path
            .as_deref()
            .filter(|path| path.is_file())
            .is_some_and(binary_has_hypervisor_entitlement);
        let libkrun = probe_libkrun(paths.libkrun, &architecture);
        let libkrunfw = probe_libkrunfw(paths.libkrunfw, &architecture);
        let krunvm = probe_tool("krunvm");
        let gvproxy = probe_tool_path(paths.gvproxy);
        let disk_tools = DiskToolPaths::discover_with_debugfs(
            mke2fs_override,
            e2fsck_override,
            debugfs_override,
        );
        let mke2fs = probe_tool_path(disk_tools.mke2fs);
        let e2fsck = probe_tool_path(disk_tools.e2fsck);
        let debugfs = probe_tool_path(disk_tools.debugfs);
        let cache_volume = probe_cache_volume(cache_dir);
        let cow_clone_supported = cache_volume.reflink_supported;
        let network_root = nearest_existing_directory(&env::temp_dir());
        let network = probe_network(&gvproxy, network_root.as_deref(), &architecture);
        let libkrun_network_api = libkrun
            .path
            .as_deref()
            .and_then(|path| library_has_symbol(path, "krun_add_net_unixgram"));
        let native_runtime_ready = validate_native_runtime_probes(
            host_supported,
            &helper,
            hypervisor_entitlement,
            &libkrun,
            &libkrunfw,
            false,
            None,
        )
        .is_ok();
        let native_network_abi_ready = validate_native_runtime_probes(
            host_supported,
            &helper,
            hypervisor_entitlement,
            &libkrun,
            &libkrunfw,
            true,
            libkrun_network_api,
        )
        .is_ok();
        let native_backend_ready = native_runtime_ready
            && hypervisor_framework
            && mke2fs.found
            && e2fsck.found
            && debugfs.found
            && cow_clone_supported == Some(true);
        let native_network_ready = native_backend_ready
            && gvproxy.found
            && network.helper_executable
            && network.helper_architecture_matches == Some(true)
            && network.socket_created == Some(true)
            && native_network_abi_ready;
        let checks = build_checks(
            &cache_volume,
            &network,
            &helper,
            hypervisor_entitlement,
            &libkrun,
            &libkrunfw,
            libkrun_network_api,
            &mke2fs,
            &e2fsck,
            &debugfs,
        );
        let warnings = checks
            .iter()
            .filter(|check| check.status != DoctorCheckStatus::Pass)
            .map(format_check_warning)
            .collect();
        Self {
            expected_libkrun_version: EXPECTED_LIBKRUN_VERSION.into(),
            expected_libkrunfw_version: EXPECTED_LIBKRUNFW_VERSION.into(),
            os,
            architecture,
            os_version,
            host_supported,
            hypervisor_framework,
            helper_path,
            hypervisor_entitlement,
            helper,
            libkrun,
            libkrunfw,
            krunvm,
            gvproxy,
            mke2fs,
            e2fsck,
            debugfs,
            cache_volume,
            network,
            cow_clone_supported,
            libkrun_network_api,
            native_backend_ready,
            native_network_ready,
            checks,
            warnings,
        }
    }
}

pub(crate) fn validate_native_runtime_for_spawn(
    helper_path: &Path,
    libkrun_path: &Path,
    libkrunfw_path: Option<&Path>,
    require_network_api: bool,
) -> Result<(), NativeRuntimePreflightError> {
    let host_architecture = env::consts::ARCH;
    let helper = probe_native_binary(Some(helper_path), host_architecture, true);
    let hypervisor_entitlement =
        helper.regular_file && binary_has_hypervisor_entitlement(helper_path);
    let libkrun = probe_libkrun(Some(libkrun_path.to_path_buf()), host_architecture);
    let libkrunfw = probe_libkrunfw(libkrunfw_path.map(Path::to_path_buf), host_architecture);
    let network_api = require_network_api
        .then(|| library_has_symbol(libkrun_path, "krun_add_net_unixgram"))
        .flatten();
    validate_native_runtime_probes(
        env::consts::OS == "macos" && host_architecture == "aarch64",
        &helper,
        hypervisor_entitlement,
        &libkrun,
        &libkrunfw,
        require_network_api,
        network_api,
    )
}

fn validate_native_runtime_probes(
    host_supported: bool,
    helper: &NativeBinaryProbe,
    hypervisor_entitlement: bool,
    libkrun: &LibraryProbe,
    libkrunfw: &LibraryProbe,
    require_network_api: bool,
    network_api: Option<bool>,
) -> Result<(), NativeRuntimePreflightError> {
    let mut issues = Vec::new();
    if !host_supported {
        issues.push("native libkrun execution requires Apple Silicon macOS".to_owned());
    }
    validate_helper_probe(helper, hypervisor_entitlement, &mut issues);
    validate_library_probe("libkrun", libkrun, EXPECTED_LIBKRUN_VERSION, &mut issues);
    validate_library_probe(
        "libkrunfw",
        libkrunfw,
        EXPECTED_LIBKRUNFW_VERSION,
        &mut issues,
    );
    if require_network_api && network_api != Some(true) {
        issues.push(
            "libkrun does not expose the released krun_add_net_unixgram network ABI".to_owned(),
        );
    }
    if issues.is_empty() {
        Ok(())
    } else {
        issues.push(
            "run `morae doctor --json` and install or sign the pinned native dependencies"
                .to_owned(),
        );
        Err(NativeRuntimePreflightError {
            details: issues.join("; "),
        })
    }
}

fn validate_helper_probe(
    helper: &NativeBinaryProbe,
    hypervisor_entitlement: bool,
    issues: &mut Vec<String>,
) {
    if !helper.regular_file {
        issues.push("morae-vmm-helper is not a regular file".to_owned());
        return;
    }
    if helper.executable != Some(true) {
        issues.push("morae-vmm-helper is not executable".to_owned());
    }
    if helper.architecture_matches != Some(true) {
        issues.push("morae-vmm-helper architecture does not match the host".to_owned());
    }
    if helper.code_signature_valid != Some(true) {
        issues.push("morae-vmm-helper code signature is invalid or unverifiable".to_owned());
    }
    if !hypervisor_entitlement {
        issues.push("morae-vmm-helper lacks the Hypervisor entitlement".to_owned());
    }
}

fn validate_library_probe(
    name: &str,
    probe: &LibraryProbe,
    expected_version: &str,
    issues: &mut Vec<String>,
) {
    if !probe.found {
        issues.push(format!(
            "{name} is not a regular file or its path was not resolved"
        ));
        return;
    }
    if probe.required_symbols_present != Some(true) {
        let missing = if probe.missing_symbols.is_empty() {
            String::new()
        } else {
            format!(": missing {}", probe.missing_symbols.join(", "))
        };
        issues.push(format!(
            "{name} does not expose the required released ABI{missing}"
        ));
    }
    if probe.version_matches != Some(true) {
        issues.push(format!(
            "{name} version is {} but the pinned released version is {expected_version}",
            probe.version.as_deref().unwrap_or("unverifiable")
        ));
    }
    if probe.architecture_matches != Some(true) {
        issues.push(format!("{name} architecture does not match the host"));
    }
    if probe.code_signature_valid != Some(true) {
        issues.push(format!("{name} code signature is invalid or unverifiable"));
    }
}

fn probe_library_path(
    path: Option<PathBuf>,
    required_symbols: &[&str],
    package: &str,
    expected_version: &str,
    host_architecture: &str,
) -> LibraryProbe {
    let found = path.as_ref().is_some_and(|path| path.is_file());
    if !found {
        return LibraryProbe {
            found: false,
            path,
            ..LibraryProbe::default()
        };
    }
    let Some(path) = path else {
        unreachable!("a found library path must be present");
    };
    let (required_symbols_present, missing_symbols) = probe_symbols(&path, required_symbols);
    let version = homebrew_version(&path, package);
    let architecture = probe_architecture(&path);
    let code_signature_valid = probe_code_signature(&path);
    LibraryProbe {
        found: true,
        path: Some(path),
        required_symbols_present,
        missing_symbols,
        version_matches: version
            .as_deref()
            .map(|version| version == expected_version),
        version,
        architecture_matches: architecture
            .as_deref()
            .map(|architecture| architecture_matches(architecture, host_architecture)),
        architecture,
        code_signature_valid,
    }
}

fn probe_libkrun(path: Option<PathBuf>, host_architecture: &str) -> LibraryProbe {
    probe_library_path(
        path,
        REQUIRED_LIBKRUN_SYMBOLS,
        "libkrun",
        EXPECTED_LIBKRUN_VERSION,
        host_architecture,
    )
}

fn probe_libkrunfw(path: Option<PathBuf>, host_architecture: &str) -> LibraryProbe {
    probe_library_path(
        path,
        &[],
        "libkrunfw",
        EXPECTED_LIBKRUNFW_VERSION,
        host_architecture,
    )
}

fn probe_symbols(path: &Path, required_symbols: &[&str]) -> (Option<bool>, Vec<String>) {
    if required_symbols.is_empty() {
        return (Some(true), Vec::new());
    }
    let Some(symbols) = command_output("nm", &["-gU", path.to_string_lossy().as_ref()]) else {
        return (None, Vec::new());
    };
    let missing = required_symbols
        .iter()
        .filter(|symbol| !symbols.contains(**symbol))
        .map(|symbol| (*symbol).to_owned())
        .collect::<Vec<_>>();
    (Some(missing.is_empty()), missing)
}

fn probe_tool(name: &str) -> ToolProbe {
    probe_tool_path(find_in_path(name))
}

fn probe_tool_path(path: Option<PathBuf>) -> ToolProbe {
    let found = path.as_ref().is_some_and(|path| path.is_file());
    let version = found
        .then_some(())
        .and(path.as_ref())
        .and_then(|path| command_output(path.to_string_lossy().as_ref(), &["--version"]));
    ToolProbe {
        found,
        path,
        version,
    }
}

fn library_has_symbol(path: &Path, symbol: &str) -> Option<bool> {
    command_output("nm", &["-gU", path.to_string_lossy().as_ref()])
        .map(|symbols| symbols.contains(symbol))
}

fn probe_native_binary(
    path: Option<&Path>,
    host_architecture: &str,
    require_executable: bool,
) -> NativeBinaryProbe {
    let regular_file = path.is_some_and(Path::is_file);
    let executable = regular_file.then(|| !require_executable || is_executable(path.unwrap()));
    let architecture = path.filter(|_| regular_file).and_then(probe_architecture);
    NativeBinaryProbe {
        path: path.map(Path::to_path_buf),
        regular_file,
        executable,
        architecture_matches: architecture
            .as_deref()
            .map(|architecture| architecture_matches(architecture, host_architecture)),
        architecture,
        code_signature_valid: path.filter(|_| regular_file).and_then(probe_code_signature),
    }
}

fn probe_cache_volume(configured_path: PathBuf) -> CacheVolumeProbe {
    let probe_path = nearest_existing_directory(&configured_path);
    let (available_bytes, total_bytes, free_space_error) = probe_path.as_deref().map_or_else(
        || {
            (
                None,
                None,
                Some("no existing cache path ancestor was found".into()),
            )
        },
        |path| match (fs2::available_space(path), fs2::total_space(path)) {
            (Ok(available), Ok(total)) => (Some(available), Some(total), None),
            (available, total) => (
                available.ok(),
                total.ok(),
                Some("failed to query cache volume capacity".into()),
            ),
        },
    );
    let (reflink_supported, reflink_error) = probe_path.as_deref().map_or_else(
        || {
            (
                None,
                Some("no writable cache volume probe path is available".into()),
            )
        },
        probe_cow_clone_at,
    );
    CacheVolumeProbe {
        configured_path,
        probe_path,
        available_bytes,
        total_bytes,
        minimum_recommended_free_bytes: MIN_RECOMMENDED_CACHE_FREE_BYTES,
        free_space_sufficient: available_bytes
            .map(|available| available >= MIN_RECOMMENDED_CACHE_FREE_BYTES),
        reflink_supported,
        free_space_error,
        reflink_error,
    }
}

fn nearest_existing_directory(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|candidate| candidate.is_dir())
        .and_then(|candidate| fs::canonicalize(candidate).ok())
}

fn probe_cow_clone_at(directory: &Path) -> (Option<bool>, Option<String>) {
    let temporary = match tempfile::Builder::new()
        .prefix("morae-doctor-cow-")
        .tempdir_in(directory)
    {
        Ok(temporary) => temporary,
        Err(error) => return (None, Some(format!("cannot write cache volume: {error}"))),
    };
    let source = temporary.path().join("source");
    let destination = temporary.path().join("destination");
    let result = fs::File::create(&source).and_then(|file| file.set_len(1024 * 1024));
    if let Err(error) = result {
        return (
            None,
            Some(format!("cannot create reflink probe file: {error}")),
        );
    }

    #[cfg(target_os = "macos")]
    let output = Command::new("/bin/cp")
        .arg("-c")
        .arg(&source)
        .arg(&destination)
        .output();

    #[cfg(target_os = "linux")]
    let output = Command::new("cp")
        .args(["--reflink=always", "--sparse=always", "--"])
        .arg(&source)
        .arg(&destination)
        .output();

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return (
        None,
        Some("reflink probing is unsupported on this host".into()),
    );

    match output {
        Ok(output) if output.status.success() && destination.is_file() => (Some(true), None),
        Ok(output) => (
            Some(false),
            Some(String::from_utf8_lossy(&output.stderr).trim().to_owned()),
        ),
        Err(error) => (
            None,
            Some(format!("failed to execute reflink probe: {error}")),
        ),
    }
}

fn probe_network(
    tool: &ToolProbe,
    probe_directory: Option<&Path>,
    host_architecture: &str,
) -> NetworkProbe {
    let Some(path) = tool.path.as_deref().filter(|_| tool.found) else {
        return NetworkProbe {
            error: Some("gvproxy was not found".into()),
            ..NetworkProbe::default()
        };
    };
    let helper_executable = is_executable(path);
    let helper_architecture = probe_architecture(path);
    let helper_architecture_matches = helper_architecture
        .as_deref()
        .map(|architecture| architecture_matches(architecture, host_architecture));
    if !helper_executable {
        return NetworkProbe {
            helper_executable,
            helper_architecture,
            helper_architecture_matches,
            error: Some("gvproxy is not executable".into()),
            ..NetworkProbe::default()
        };
    }
    let (socket_created, error) = probe_directory.map_or_else(
        || {
            (
                None,
                Some("no cache volume path is available for a socket probe".into()),
            )
        },
        |directory| probe_network_socket(path, directory),
    );
    NetworkProbe {
        helper_executable,
        helper_architecture,
        helper_architecture_matches,
        socket_created,
        error,
    }
}

fn probe_network_socket(executable: &Path, directory: &Path) -> (Option<bool>, Option<String>) {
    let state = match tempfile::Builder::new()
        .prefix("morae-doctor-net-")
        .tempdir_in(directory)
    {
        Ok(state) => state,
        Err(error) => {
            return (
                None,
                Some(format!("cannot create network probe state: {error}")),
            );
        }
    };
    let socket_path = state.path().join("gvproxy.sock");
    let Some(socket_uri) = socket_uri(&socket_path) else {
        return (
            Some(false),
            Some("gvproxy socket path exceeds the Unix limit".into()),
        );
    };
    let child = Command::new(executable)
        .arg("--listen-vfkit")
        .arg(socket_uri)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .env_clear()
        .spawn();
    let Ok(mut child) = child else {
        return (None, Some("failed to start gvproxy".into()));
    };
    let stderr = child.stderr.take().map(start_bounded_stderr_capture);
    let result = wait_for_network_socket(&mut child, &socket_path);
    stop_child(&mut child);
    let diagnostics = stderr.map_or_else(Vec::new, finish_bounded_stderr_capture);
    with_stderr_diagnostics(result, &diagnostics)
}

fn wait_for_network_socket(
    child: &mut Child,
    socket_path: &Path,
) -> (Option<bool>, Option<String>) {
    let started = Instant::now();
    loop {
        if socket_path.exists() && !path_is_socket(socket_path) {
            return (
                Some(false),
                Some("gvproxy created a non-socket path".into()),
            );
        }
        let probe_error = match probe_vfkit_endpoint(socket_path) {
            Ok(()) => return (Some(true), None),
            Err(error) => error,
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                return (
                    Some(false),
                    Some(format!("gvproxy exited before socket readiness: {status}")),
                );
            }
            Err(error) => return (None, Some(format!("cannot inspect gvproxy: {error}"))),
            Ok(None) => {}
        }
        if started.elapsed() >= NETWORK_SOCKET_TIMEOUT {
            return (
                Some(false),
                Some(format!(
                    "gvproxy socket was not connectable within 5s; last socket error: {probe_error}"
                )),
            );
        }
        thread::sleep(NETWORK_SOCKET_POLL_INTERVAL);
    }
}

type StderrCapture = (Arc<Mutex<Vec<u8>>>, thread::JoinHandle<()>);

fn start_bounded_stderr_capture(mut stderr: impl Read + Send + 'static) -> StderrCapture {
    let retained = Arc::new(Mutex::new(Vec::with_capacity(NETWORK_PROXY_STDERR_LIMIT)));
    let output = retained.clone();
    let task = thread::spawn(move || {
        let mut chunk = [0_u8; 4096];
        while let Ok(read) = stderr.read(&mut chunk) {
            if read == 0 {
                break;
            }
            if let Ok(mut retained) = output.lock() {
                append_bounded_tail(&mut retained, &chunk[..read], NETWORK_PROXY_STDERR_LIMIT);
            } else {
                break;
            }
        }
    });
    (retained, task)
}

fn finish_bounded_stderr_capture((retained, task): StderrCapture) -> Vec<u8> {
    let started = Instant::now();
    while !task.is_finished() && started.elapsed() < NETWORK_STDERR_FINISH_TIMEOUT {
        thread::sleep(Duration::from_millis(1));
    }
    if task.is_finished() {
        let _ = task.join();
    }
    retained
        .lock()
        .map_or_else(|_| Vec::new(), |bytes| bytes.clone())
}

fn with_stderr_diagnostics(
    result: (Option<bool>, Option<String>),
    stderr: &[u8],
) -> (Option<bool>, Option<String>) {
    let (ready, error) = result;
    if ready == Some(true) || stderr.is_empty() {
        return (ready, error);
    }
    let suffix = stderr_diagnostics(stderr);
    (
        ready,
        Some(format!(
            "{}{}",
            error.unwrap_or_else(|| "gvproxy probe failed".into()),
            suffix
        )),
    )
}

fn stop_child(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn socket_uri(path: &Path) -> Option<String> {
    let path = path.to_str()?;
    #[cfg(unix)]
    if path.len() >= 104 {
        return None;
    }
    Some(format!("unixgram://{path}"))
}

#[cfg(unix)]
fn path_is_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket())
}

#[cfg(not(unix))]
fn path_is_socket(path: &Path) -> bool {
    path.exists()
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if stdout.is_empty() {
        None
    } else {
        Some(stdout)
    }
}

fn homebrew_version(path: &Path, package: &str) -> Option<String> {
    let canonical = fs::canonicalize(path).ok()?;
    let parts = canonical
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>();
    parts.windows(3).find_map(|parts| {
        (parts[0] == "Cellar" && parts[1] == package).then(|| parts[2].clone().into_owned())
    })
}

fn probe_architecture(path: &Path) -> Option<String> {
    #[cfg(target_os = "macos")]
    if let Some(architectures) =
        command_output("lipo", &["-archs", path.to_string_lossy().as_ref()])
    {
        return Some(architectures);
    }
    command_output("file", &["-b", path.to_string_lossy().as_ref()])
}

fn architecture_matches(actual: &str, expected: &str) -> bool {
    let expected = match expected {
        "aarch64" => "arm64",
        other => other,
    };
    actual
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .any(|architecture| architecture == expected)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(target_os = "macos")]
fn probe_code_signature(path: &Path) -> Option<bool> {
    Command::new("codesign")
        .args(["--verify", "--strict"])
        .arg(path)
        .output()
        .ok()
        .map(|output| output.status.success())
}

#[cfg(not(target_os = "macos"))]
fn probe_code_signature(_path: &Path) -> Option<bool> {
    None
}

#[allow(clippy::too_many_arguments)]
fn build_checks(
    cache: &CacheVolumeProbe,
    network: &NetworkProbe,
    helper: &NativeBinaryProbe,
    hypervisor_entitlement: bool,
    libkrun: &LibraryProbe,
    libkrunfw: &LibraryProbe,
    libkrun_network_api: Option<bool>,
    mke2fs: &ToolProbe,
    e2fsck: &ToolProbe,
    debugfs: &ToolProbe,
) -> Vec<DoctorCheck> {
    vec![
        make_check(
            "cache_reflink",
            status_for(&[cache.reflink_supported]),
            format!(
                "cache volume reflink support is {} at {}",
                probe_state(cache.reflink_supported),
                cache
                    .probe_path
                    .as_deref()
                    .unwrap_or(&cache.configured_path)
                    .display()
            ),
            "Use an APFS/reflink-capable cache volume and pass it with --cache-dir.",
        ),
        make_check(
            "cache_free_space",
            status_for(&[cache.free_space_sufficient]),
            format!(
                "cache volume has {} free; {} is recommended",
                cache
                    .available_bytes
                    .map_or_else(|| "unknown".into(), format_bytes),
                format_bytes(cache.minimum_recommended_free_bytes)
            ),
            "Free cache space or select a larger volume with --cache-dir.",
        ),
        make_check(
            "network_helper",
            status_for(&[
                Some(network.helper_executable),
                network.helper_architecture_matches,
            ]),
            format!(
                "gvproxy executable={}, architecture={}",
                network.helper_executable,
                network.helper_architecture.as_deref().unwrap_or("unknown")
            ),
            "Install a released executable gvproxy for the host architecture or pass --gvproxy.",
        ),
        make_check(
            "network_socket",
            status_for(&[network.socket_created]),
            format!(
                "gvproxy endpoint handshake is {}",
                probe_state(network.socket_created)
            ),
            "Check gvproxy compatibility and cache path permissions/length, then retry doctor.",
        ),
        make_check(
            "helper_signing",
            status_for(&[
                Some(helper.regular_file),
                helper.executable,
                helper.architecture_matches,
                helper.code_signature_valid,
                Some(hypervisor_entitlement),
            ]),
            format!(
                "helper executable={}, architecture={}, signature={}, entitlement={hypervisor_entitlement}",
                probe_state(helper.executable),
                helper.architecture.as_deref().unwrap_or("unknown"),
                probe_state(helper.code_signature_valid)
            ),
            "Ad-hoc sign morae-vmm-helper with assets/moraebox-vmm.entitlements for this architecture.",
        ),
        library_check("libkrun_abi", libkrun, EXPECTED_LIBKRUN_VERSION),
        library_check("libkrunfw_abi", libkrunfw, EXPECTED_LIBKRUNFW_VERSION),
        make_check(
            "network_abi",
            status_for(&[libkrun_network_api]),
            format!(
                "krun_add_net_unixgram availability is {}",
                probe_state(libkrun_network_api)
            ),
            "Install the pinned released libkrun build that exports krun_add_net_unixgram.",
        ),
        make_check(
            "disk_tools",
            status_for(&[Some(mke2fs.found), Some(e2fsck.found), Some(debugfs.found)]),
            format!(
                "mke2fs found={}, e2fsck found={}, debugfs found={}",
                mke2fs.found, e2fsck.found, debugfs.found
            ),
            "Install e2fsprogs or pass --mke2fs, --e2fsck, and --debugfs explicitly.",
        ),
    ]
}

fn library_check(id: &str, probe: &LibraryProbe, expected_version: &str) -> DoctorCheck {
    make_check(
        id,
        status_for(&[
            Some(probe.found),
            probe.required_symbols_present,
            probe.version_matches,
            probe.architecture_matches,
            probe.code_signature_valid,
        ]),
        format!(
            "found={}, version={} (expected {expected_version}), architecture={}, symbols={}, signature={}",
            probe.found,
            probe.version.as_deref().unwrap_or("unknown"),
            probe.architecture.as_deref().unwrap_or("unknown"),
            probe_state(probe.required_symbols_present),
            probe_state(probe.code_signature_valid)
        ),
        "Install the pinned released Homebrew library for this architecture and ensure its code signature is valid.",
    )
}

fn status_for(values: &[Option<bool>]) -> DoctorCheckStatus {
    if values.contains(&Some(false)) {
        DoctorCheckStatus::Fail
    } else if values.iter().any(Option::is_none) {
        DoctorCheckStatus::Warn
    } else {
        DoctorCheckStatus::Pass
    }
}

fn probe_state(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "ready",
        Some(false) => "unavailable",
        None => "unknown",
    }
}

fn format_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    if bytes >= GIB {
        let whole = bytes / GIB;
        let tenth = (bytes % GIB) * 10 / GIB;
        format!("{whole}.{tenth} GiB")
    } else {
        format!("{bytes} bytes")
    }
}

fn make_check(
    id: &str,
    status: DoctorCheckStatus,
    summary: String,
    remediation: &str,
) -> DoctorCheck {
    DoctorCheck {
        id: id.into(),
        status,
        summary,
        remediation: (status != DoctorCheckStatus::Pass).then(|| remediation.into()),
    }
}

fn format_check_warning(check: &DoctorCheck) -> String {
    check.remediation.as_ref().map_or_else(
        || format!("{}: {}", check.id, check.summary),
        |remediation| {
            format!(
                "{}: {}; remediation: {remediation}",
                check.id, check.summary
            )
        },
    )
}

fn configured_path(key: &str) -> Option<PathBuf> {
    env::var_os(key).map(PathBuf::from)
}

fn resolve_tool_path(
    explicit: Option<PathBuf>,
    environment: &str,
    name: &str,
    candidates: &[&str],
) -> Option<PathBuf> {
    explicit
        .or_else(|| configured_path(environment))
        .or_else(|| find_in_path(name))
        .or_else(|| find_candidate(candidates))
}

fn find_sibling_helper() -> Option<PathBuf> {
    let sibling = env::current_exe().ok()?.with_file_name(if cfg!(windows) {
        "morae-vmm-helper.exe"
    } else {
        "morae-vmm-helper"
    });
    sibling.is_file().then_some(sibling)
}

fn find_candidate(candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

fn find_sibling_library(library: Option<&Path>, name: &str) -> Option<PathBuf> {
    let sibling = library?.parent()?.join(name);
    sibling.is_file().then_some(sibling)
}

fn library_parent_path(libkrun: Option<&Path>, libkrunfw: Option<&Path>) -> Option<PathBuf> {
    let mut directories = Vec::new();
    for directory in [libkrun, libkrunfw]
        .into_iter()
        .flatten()
        .filter_map(Path::parent)
    {
        if !directories.iter().any(|existing| existing == directory) {
            directories.push(directory.to_path_buf());
        }
    }
    (!directories.is_empty())
        .then(|| env::join_paths(directories).ok().map(PathBuf::from))
        .flatten()
}

fn binary_has_hypervisor_entitlement(executable: &Path) -> bool {
    let Ok(output) = Command::new("codesign")
        .args(["-d", "--entitlements", ":-"])
        .arg(executable)
        .output()
    else {
        return false;
    };
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    text.contains("com.apple.security.hypervisor")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn network_readiness_requires_a_connectable_datagram_endpoint() {
        let state = tempfile::tempdir().unwrap();
        let socket_path = state.path().join("gvproxy.sock");
        let _listener = std::os::unix::net::UnixDatagram::bind(&socket_path).unwrap();
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exec sleep 10"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        assert_eq!(
            wait_for_network_socket(&mut child, &socket_path),
            (Some(true), None)
        );
        stop_child(&mut child);
    }

    #[cfg(unix)]
    #[test]
    fn network_readiness_rejects_a_path_without_a_socket() {
        let state = tempfile::tempdir().unwrap();
        let socket_path = state.path().join("gvproxy.sock");
        fs::write(&socket_path, []).unwrap();
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exec sleep 10"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let (ready, error) = wait_for_network_socket(&mut child, &socket_path);
        assert_eq!(ready, Some(false));
        assert!(error.is_some_and(|message| message.contains("non-socket")));
        stop_child(&mut child);
    }

    #[test]
    fn doctor_failure_appends_bounded_stderr_diagnostics() {
        let (ready, error) = with_stderr_diagnostics(
            (Some(false), Some("probe failed".into())),
            b"diagnostic-tail",
        );

        assert_eq!(ready, Some(false));
        let error = error.unwrap();
        assert!(error.contains("probe failed"));
        assert!(error.contains("diagnostic-tail"));
    }

    #[test]
    fn doctor_never_claims_ready_without_libraries() {
        let report = DoctorReport::collect();
        if !report.libkrun.found || !report.libkrunfw.found {
            assert!(!report.native_backend_ready);
        }
    }

    #[test]
    fn missing_library_probe_is_explicit() {
        let path = PathBuf::from("/definitely/not/a/library");
        let probe = probe_library_path(
            Some(path.clone()),
            REQUIRED_LIBKRUN_SYMBOLS,
            "libkrun",
            EXPECTED_LIBKRUN_VERSION,
            env::consts::ARCH,
        );
        assert!(!probe.found);
        assert_eq!(probe.path, Some(path));
        assert_eq!(probe.required_symbols_present, None);
    }

    #[test]
    fn native_preflight_requires_pinned_released_probes() {
        let helper = NativeBinaryProbe {
            path: Some(PathBuf::from("/native/morae-vmm-helper")),
            regular_file: true,
            executable: Some(true),
            architecture: Some("arm64".into()),
            architecture_matches: Some(true),
            code_signature_valid: Some(true),
        };
        let library = |name: &str, version: &str| LibraryProbe {
            found: true,
            path: Some(PathBuf::from(format!("/native/{name}.dylib"))),
            required_symbols_present: Some(true),
            missing_symbols: Vec::new(),
            version: Some(version.into()),
            version_matches: Some(true),
            architecture: Some("arm64".into()),
            architecture_matches: Some(true),
            code_signature_valid: Some(true),
        };
        let libkrun = library("libkrun", EXPECTED_LIBKRUN_VERSION);
        let libkrunfw = library("libkrunfw", EXPECTED_LIBKRUNFW_VERSION);

        assert!(
            validate_native_runtime_probes(
                true,
                &helper,
                true,
                &libkrun,
                &libkrunfw,
                true,
                Some(true),
            )
            .is_ok()
        );

        let mut unverifiable = libkrun;
        unverifiable.version = None;
        unverifiable.version_matches = None;
        let error = validate_native_runtime_probes(
            true,
            &helper,
            true,
            &unverifiable,
            &libkrunfw,
            true,
            Some(false),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("libkrun version is unverifiable"));
        assert!(error.contains("krun_add_net_unixgram"));
        assert!(error.contains("morae doctor --json"));
    }

    #[test]
    fn doctor_uses_resolved_tool_overrides_without_fallback() {
        let missing_helper = PathBuf::from("/configured/missing-helper");
        let missing_library = PathBuf::from("/configured/missing-libkrun");
        let missing_proxy = PathBuf::from("/configured/missing-gvproxy");
        let missing_mke2fs = PathBuf::from("/configured/missing-mke2fs");
        let missing_e2fsck = PathBuf::from("/configured/missing-e2fsck");
        let missing_debugfs = PathBuf::from("/configured/missing-debugfs");
        let report = DoctorReport::collect_with_paths_and_debugfs(
            NativeRuntimePaths {
                helper: Some(missing_helper.clone()),
                libkrun: Some(missing_library.clone()),
                libkrunfw: None,
                gvproxy: Some(missing_proxy.clone()),
                library_search_path: None,
            },
            Some(missing_mke2fs.clone()),
            Some(missing_e2fsck.clone()),
            Some(missing_debugfs.clone()),
        );

        assert_eq!(report.helper_path, Some(missing_helper));
        assert_eq!(report.libkrun.path, Some(missing_library));
        assert_eq!(report.gvproxy.path, Some(missing_proxy));
        assert_eq!(report.mke2fs.path, Some(missing_mke2fs));
        assert_eq!(report.e2fsck.path, Some(missing_e2fsck));
        assert_eq!(report.debugfs.path, Some(missing_debugfs));
        assert!(!report.native_backend_ready);
    }

    #[test]
    fn explicit_native_paths_take_precedence() {
        let paths = NativeRuntimePaths::discover_with_gvproxy(
            Some(PathBuf::from("/configured/helper")),
            Some(PathBuf::from("/configured/lib/libkrun.dylib")),
            Some(PathBuf::from("/configured/search")),
            Some(PathBuf::from("/configured/gvproxy")),
        );
        assert_eq!(paths.helper, Some(PathBuf::from("/configured/helper")));
        assert_eq!(
            paths.libkrun,
            Some(PathBuf::from("/configured/lib/libkrun.dylib"))
        );
        assert_eq!(
            paths.library_search_path,
            Some(PathBuf::from("/configured/search"))
        );
        assert_eq!(paths.gvproxy, Some(PathBuf::from("/configured/gvproxy")));
    }

    #[test]
    fn explicit_disk_tool_paths_take_precedence_and_commands_have_fallbacks() {
        let paths = DiskToolPaths::discover_with_debugfs(
            Some(PathBuf::from("/configured/mke2fs")),
            Some(PathBuf::from("/configured/e2fsck")),
            Some(PathBuf::from("/configured/debugfs")),
        );
        assert_eq!(paths.mke2fs_command(), PathBuf::from("/configured/mke2fs"));
        assert_eq!(paths.e2fsck_command(), PathBuf::from("/configured/e2fsck"));
        assert_eq!(
            paths.debugfs_command(),
            PathBuf::from("/configured/debugfs")
        );

        let unavailable = DiskToolPaths {
            mke2fs: None,
            e2fsck: None,
            debugfs: None,
        };
        assert_eq!(unavailable.mke2fs_command(), PathBuf::from("mke2fs"));
        assert_eq!(unavailable.e2fsck_command(), PathBuf::from("e2fsck"));
        assert_eq!(unavailable.debugfs_command(), PathBuf::from("debugfs"));
    }

    #[test]
    fn library_search_path_uses_unique_library_parents() {
        let joined = library_parent_path(
            Some(Path::new("/opt/native/lib/libkrun.dylib")),
            Some(Path::new("/opt/native/lib/libkrunfw.dylib")),
        );
        assert_eq!(joined, Some(PathBuf::from("/opt/native/lib")));
    }

    #[test]
    fn homebrew_version_comes_from_the_canonical_cellar_path() {
        let directory = tempfile::tempdir().unwrap();
        let library = directory
            .path()
            .join("Cellar/libkrun/1.19.4/lib/libkrun.dylib");
        fs::create_dir_all(library.parent().unwrap()).unwrap();
        fs::write(&library, b"fixture").unwrap();

        assert_eq!(
            homebrew_version(&library, "libkrun").as_deref(),
            Some("1.19.4")
        );
        assert_eq!(homebrew_version(&library, "libkrunfw"), None);
    }

    #[test]
    fn cache_probe_uses_the_nearest_existing_volume_ancestor() {
        let directory = tempfile::tempdir().unwrap();
        let configured = directory.path().join("not-created/cache");
        let probe = probe_cache_volume(configured.clone());

        assert_eq!(probe.configured_path, configured);
        assert_eq!(
            probe.probe_path,
            Some(fs::canonicalize(directory.path()).unwrap())
        );
        assert!(probe.available_bytes.is_some());
        assert!(probe.total_bytes.is_some());
    }

    #[test]
    fn every_non_pass_check_has_remediation() {
        let missing_tool = ToolProbe {
            found: false,
            path: None,
            version: None,
        };
        let checks = build_checks(
            &CacheVolumeProbe::default(),
            &NetworkProbe::default(),
            &NativeBinaryProbe::default(),
            false,
            &LibraryProbe::default(),
            &LibraryProbe::default(),
            None,
            &missing_tool,
            &missing_tool,
            &missing_tool,
        );

        assert!(checks.iter().all(|check| {
            check.status == DoctorCheckStatus::Pass || check.remediation.is_some()
        }));
    }
}
