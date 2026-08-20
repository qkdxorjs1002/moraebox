use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

use serde::{Deserialize, Serialize};

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
    pub libkrun: LibraryProbe,
    pub libkrunfw: LibraryProbe,
    pub krunvm: ToolProbe,
    pub gvproxy: ToolProbe,
    pub mke2fs: ToolProbe,
    pub e2fsck: ToolProbe,
    pub cow_clone_supported: Option<bool>,
    pub libkrun_network_api: Option<bool>,
    pub native_backend_ready: bool,
    pub native_network_ready: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryProbe {
    pub found: bool,
    pub path: Option<PathBuf>,
    pub required_symbols_present: Option<bool>,
    pub missing_symbols: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
        let os = env::consts::OS.to_owned();
        let architecture = env::consts::ARCH.to_owned();
        let os_version = command_output("sw_vers", &["-productVersion"]);
        let host_supported = os == "macos" && architecture == "aarch64";
        // On current macOS releases the binary may live in the dyld shared cache,
        // leaving a dangling-looking compatibility symlink in the framework bundle.
        let hypervisor_framework =
            Path::new("/System/Library/Frameworks/Hypervisor.framework").is_dir();
        let helper_path = find_helper();
        let hypervisor_entitlement = helper_path
            .as_deref()
            .is_some_and(binary_has_hypervisor_entitlement);
        let libkrun = probe_library(
            "MORAE_LIBKRUN_PATH",
            LIBKRUN_CANDIDATES,
            REQUIRED_LIBKRUN_SYMBOLS,
        );
        let libkrunfw = probe_library("MORAE_LIBKRUNFW_PATH", LIBKRUNFW_CANDIDATES, &[]);
        let krunvm = probe_tool("krunvm");
        let gvproxy = probe_tool_path(
            configured_path("MORAE_GVPROXY_PATH")
                .filter(|path| path.is_file())
                .or_else(|| find_in_path("gvproxy"))
                .or_else(|| find_candidate(GVPROXY_CANDIDATES)),
        );
        let mke2fs = probe_tool_with_candidates("mke2fs", MKE2FS_CANDIDATES);
        let e2fsck = probe_tool_with_candidates("e2fsck", E2FSCK_CANDIDATES);
        let cow_clone_supported = probe_cow_clone();
        let libkrun_network_api = libkrun
            .path
            .as_deref()
            .and_then(|path| library_has_symbol(path, "krun_add_net_unixgram"));
        let native_backend_ready = host_supported
            && hypervisor_framework
            && hypervisor_entitlement
            && libkrun.found
            && libkrun.required_symbols_present == Some(true)
            && libkrunfw.found
            && mke2fs.found
            && e2fsck.found
            && cow_clone_supported == Some(true);
        let native_network_ready =
            native_backend_ready && gvproxy.found && libkrun_network_api == Some(true);
        let mut warnings = Vec::new();
        if !libkrun.found {
            warnings.push("libkrun was not found; the process backend remains available".into());
        }
        if !libkrunfw.found {
            warnings.push("libkrunfw was not found; a native Linux guest cannot boot".into());
        }
        if !hypervisor_entitlement {
            warnings.push(
                "the current executable lacks com.apple.security.hypervisor; sign the vmm helper before native execution"
                    .into(),
            );
        }
        if !gvproxy.found {
            warnings.push(
                "gvproxy was not found; native runs remain network-isolated unless it is installed and configured"
                    .into(),
            );
        }
        if libkrun_network_api == Some(false) {
            warnings.push(
                "libkrun does not export krun_add_net_unixgram; native network opt-in is unavailable"
                    .into(),
            );
        }
        if !mke2fs.found || !e2fsck.found {
            warnings.push(
                "e2fsprogs mke2fs/e2fsck are required to prepare and recover Box root disks".into(),
            );
        }
        if cow_clone_supported == Some(false) {
            warnings.push(
                "the runtime volume does not support strict copy-on-write cloning; ephemeral native runs are unavailable"
                    .into(),
            );
        }
        Self {
            expected_libkrun_version: "1.19.4".into(),
            expected_libkrunfw_version: "5.5.0".into(),
            os,
            architecture,
            os_version,
            host_supported,
            hypervisor_framework,
            helper_path,
            hypervisor_entitlement,
            libkrun,
            libkrunfw,
            krunvm,
            gvproxy,
            mke2fs,
            e2fsck,
            cow_clone_supported,
            libkrun_network_api,
            native_backend_ready,
            native_network_ready,
            warnings,
        }
    }
}

fn probe_library(
    environment_key: &str,
    candidates: &[&str],
    required_symbols: &[&str],
) -> LibraryProbe {
    let configured = env::var_os(environment_key).map(PathBuf::from);
    let path = configured.filter(|path| path.is_file()).or_else(|| {
        candidates
            .iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
    });
    let Some(path) = path else {
        return LibraryProbe {
            found: false,
            path: None,
            required_symbols_present: None,
            missing_symbols: Vec::new(),
        };
    };
    if required_symbols.is_empty() {
        return LibraryProbe {
            found: true,
            path: Some(path),
            required_symbols_present: Some(true),
            missing_symbols: Vec::new(),
        };
    }
    let symbols = command_output("nm", &["-gU", path.to_string_lossy().as_ref()]);
    let Some(symbols) = symbols else {
        return LibraryProbe {
            found: true,
            path: Some(path),
            required_symbols_present: None,
            missing_symbols: Vec::new(),
        };
    };
    let missing_symbols = required_symbols
        .iter()
        .filter(|symbol| !symbols.contains(**symbol))
        .map(|symbol| (*symbol).to_owned())
        .collect::<Vec<_>>();
    LibraryProbe {
        found: true,
        path: Some(path),
        required_symbols_present: Some(missing_symbols.is_empty()),
        missing_symbols,
    }
}

fn probe_tool(name: &str) -> ToolProbe {
    probe_tool_path(find_in_path(name))
}

fn probe_tool_with_candidates(name: &str, candidates: &[&str]) -> ToolProbe {
    probe_tool_path(find_in_path(name).or_else(|| {
        candidates
            .iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
    }))
}

fn probe_tool_path(path: Option<PathBuf>) -> ToolProbe {
    let version = path
        .as_ref()
        .and_then(|path| command_output(path.to_string_lossy().as_ref(), &["--version"]));
    ToolProbe {
        found: path.is_some(),
        path,
        version,
    }
}

fn library_has_symbol(path: &Path, symbol: &str) -> Option<bool> {
    command_output("nm", &["-gU", path.to_string_lossy().as_ref()])
        .map(|symbols| symbols.contains(symbol))
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

fn probe_cow_clone() -> Option<bool> {
    let directory = tempfile::tempdir().ok()?;
    let source = directory.path().join("source");
    let destination = directory.path().join("destination");
    let file = std::fs::File::create(&source).ok()?;
    file.set_len(1024 * 1024).ok()?;
    drop(file);

    #[cfg(target_os = "macos")]
    let output = Command::new("/bin/cp")
        .arg("-c")
        .arg(&source)
        .arg(&destination)
        .output()
        .ok()?;

    #[cfg(target_os = "linux")]
    let output = Command::new("cp")
        .args(["--reflink=always", "--sparse=always", "--"])
        .arg(&source)
        .arg(&destination)
        .output()
        .ok()?;

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return None;

    Some(output.status.success())
}

fn find_helper() -> Option<PathBuf> {
    configured_path("MORAE_HELPER_PATH")
        .filter(|path| path.is_file())
        .or_else(find_sibling_helper)
}

fn configured_path(key: &str) -> Option<PathBuf> {
    env::var_os(key).map(PathBuf::from)
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

    #[test]
    fn doctor_never_claims_ready_without_libraries() {
        let report = DoctorReport::collect();
        if !report.libkrun.found || !report.libkrunfw.found {
            assert!(!report.native_backend_ready);
        }
    }

    #[test]
    fn missing_library_probe_is_explicit() {
        let probe = probe_library(
            "MORAE_TEST_MISSING_LIBRARY",
            &["/definitely/not/a/library"],
            REQUIRED_LIBKRUN_SYMBOLS,
        );
        assert!(!probe.found);
        assert_eq!(probe.required_symbols_present, None);
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
    fn library_search_path_uses_unique_library_parents() {
        let joined = library_parent_path(
            Some(Path::new("/opt/native/lib/libkrun.dylib")),
            Some(Path::new("/opt/native/lib/libkrunfw.dylib")),
        );
        assert_eq!(joined, Some(PathBuf::from("/opt/native/lib")));
    }
}
