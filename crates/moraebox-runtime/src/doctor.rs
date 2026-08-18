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
    "krun_add_vsock",
    "krun_add_virtio_console_default",
    "krun_start_enter",
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
    pub smolvm: ToolProbe,
    pub krunvm: ToolProbe,
    pub native_backend_ready: bool,
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
            &[
                "/opt/homebrew/lib/libkrun.dylib",
                "/opt/homebrew/opt/libkrun/lib/libkrun.dylib",
                "/usr/local/lib/libkrun.dylib",
            ],
            REQUIRED_LIBKRUN_SYMBOLS,
        );
        let libkrunfw = probe_library(
            "MORAE_LIBKRUNFW_PATH",
            &[
                "/opt/homebrew/lib/libkrunfw.dylib",
                "/opt/homebrew/opt/libkrunfw/lib/libkrunfw.dylib",
                "/usr/local/lib/libkrunfw.dylib",
            ],
            &[],
        );
        let smolvm = probe_tool("smolvm");
        let krunvm = probe_tool("krunvm");
        let native_backend_ready = host_supported
            && hypervisor_framework
            && hypervisor_entitlement
            && libkrun.found
            && libkrun.required_symbols_present == Some(true)
            && libkrunfw.found;
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
        if !smolvm.found {
            warnings.push("smolvm is unavailable for the Phase 0 performance baseline".into());
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
            smolvm,
            krunvm,
            native_backend_ready,
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
    let path = find_in_path(name);
    let version = path
        .as_ref()
        .and_then(|path| command_output(path.to_string_lossy().as_ref(), &["--version"]));
    ToolProbe {
        found: path.is_some(),
        path,
        version,
    }
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

fn find_helper() -> Option<PathBuf> {
    if let Some(configured) = env::var_os("MORAE_HELPER_PATH").map(PathBuf::from)
        && configured.is_file()
    {
        return Some(configured);
    }
    let sibling = env::current_exe().ok()?.with_file_name(if cfg!(windows) {
        "morae-vmm-helper.exe"
    } else {
        "morae-vmm-helper"
    });
    sibling.is_file().then_some(sibling)
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
}
