//! Minimal process boundary around libkrun's process-consuming `krun_start_enter` API.

#![allow(unsafe_code)]

use std::{
    collections::BTreeMap,
    ffi::{CStr, CString, c_char},
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::Parser;
use libloading::Library;
use thiserror::Error;

type CreateCtx = unsafe extern "C" fn() -> i32;
type FreeCtx = unsafe extern "C" fn(u32) -> i32;
type SetVmConfig = unsafe extern "C" fn(u32, u8, u32) -> i32;
type SetRoot = unsafe extern "C" fn(u32, *const c_char) -> i32;
type AddVirtioFs3 = unsafe extern "C" fn(u32, *const c_char, *const c_char, u64, bool) -> i32;
type SetExec =
    unsafe extern "C" fn(u32, *const c_char, *const *const c_char, *const *const c_char) -> i32;
type SetWorkdir = unsafe extern "C" fn(u32, *const c_char) -> i32;
type DisableImplicitVsock = unsafe extern "C" fn(u32) -> i32;
type AddVsock = unsafe extern "C" fn(u32, u32) -> i32;
type AddDisk = unsafe extern "C" fn(u32, *const c_char, *const c_char, bool) -> i32;
type AddVirtioConsoleDefault = unsafe extern "C" fn(u32, i32, i32, i32) -> i32;
type StartEnter = unsafe extern "C" fn(u32) -> i32;

const ROOT_TAG: &CStr = c"/dev/root";
const DEFAULT_DAX_WINDOW: u64 = 256 * 1024 * 1024;

#[derive(Debug, Parser)]
#[command(about = "Signed libkrun VMM helper; normally invoked by fastmvm")]
struct Args {
    #[arg(long)]
    libkrun: PathBuf,
    #[arg(long)]
    root: PathBuf,
    #[arg(long, default_value_t = 2)]
    cpus: u8,
    #[arg(long, default_value_t = 512)]
    memory_mib: u32,
    #[arg(long)]
    cwd: Option<String>,
    /// Read-only ext4 workspace disk; mounted at /workspace before command execution.
    #[arg(long)]
    workspace_disk: Option<PathBuf>,
    /// Supervisor PID. The helper self-terminates if ownership is lost.
    #[arg(long)]
    parent_pid: Option<u32>,
    #[arg(long = "env", value_parser = parse_env)]
    env: Vec<(String, String)>,
    #[arg(required = true, last = true)]
    command: Vec<String>,
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(code) => exit_code(code),
        Err(error) => {
            eprintln!("fastmvm-vmm-helper: {error}");
            ExitCode::from(125)
        }
    }
}

fn run(args: Args) -> Result<i32, HelperError> {
    if args.command.is_empty() || args.command[0].is_empty() {
        return Err(HelperError::Invalid("command must not be empty"));
    }
    if args.cpus == 0 || args.memory_mib == 0 {
        return Err(HelperError::Invalid(
            "CPU and memory limits must be non-zero",
        ));
    }
    if let Some(parent_pid) = args.parent_pid {
        spawn_parent_watchdog(parent_pid);
    }

    let api = KrunApi::load(&args.libkrun)?;
    let mut context = api.create_context()?;
    KrunApi::check("krun_set_vm_config", unsafe {
        // SAFETY: the function pointer was resolved from the loaded libkrun ABI and the
        // context is live for the duration of this call.
        (api.set_vm_config)(context.id, args.cpus, args.memory_mib)
    })?;
    api.configure_root(context.id, &args.root)?;
    api.configure_no_tsi(context.id)?;
    api.configure_console(context.id)?;
    if let Some(workspace) = args.workspace_disk.as_deref() {
        api.configure_workspace_disk(context.id, workspace)?;
    }

    if let Some(cwd) = args.cwd.as_deref() {
        let cwd = CString::new(cwd)?;
        KrunApi::check("krun_set_workdir", unsafe {
            // SAFETY: `cwd` is NUL-terminated and remains alive during the call.
            (api.set_workdir)(context.id, cwd.as_ptr())
        })?;
    }

    let effective_command = if args.workspace_disk.is_some() {
        workspace_command(&args.command)
    } else {
        args.command
    };
    let executable = CString::new(effective_command[0].as_str())?;
    // libkrun's injected init supplies argv[0] from `exec_path`; this array contains
    // only the user arguments that follow the executable.
    let command = CStringArray::new(effective_command.iter().skip(1).map(String::as_str))?;
    let environment = BTreeMap::from_iter(args.env);
    let environment = CStringArray::new(
        environment
            .iter()
            .map(|(key, value)| format!("{key}={value}")),
    )?;
    KrunApi::check("krun_set_exec", unsafe {
        // SAFETY: all strings and pointer arrays are NUL-terminated and live through this call.
        (api.set_exec)(
            context.id,
            executable.as_ptr(),
            command.pointers.as_ptr(),
            environment.pointers.as_ptr(),
        )
    })?;

    context.consume();
    let result = unsafe {
        // SAFETY: the context is fully configured and ownership is transferred to libkrun.
        // On success libkrun terminates this helper with the guest workload's exit status.
        (api.start_enter)(context.id)
    };
    if result < 0 {
        Err(HelperError::KrunCall {
            operation: "krun_start_enter",
            code: result,
        })
    } else {
        Ok(result)
    }
}

struct KrunApi {
    _library: Library,
    create_ctx: CreateCtx,
    free_ctx: FreeCtx,
    set_vm_config: SetVmConfig,
    set_root: Option<SetRoot>,
    add_virtiofs3: Option<AddVirtioFs3>,
    set_exec: SetExec,
    set_workdir: SetWorkdir,
    disable_implicit_vsock: Option<DisableImplicitVsock>,
    add_vsock: AddVsock,
    add_disk: Option<AddDisk>,
    add_virtio_console_default: Option<AddVirtioConsoleDefault>,
    start_enter: StartEnter,
}

impl KrunApi {
    fn load(path: &Path) -> Result<Self, HelperError> {
        let library = unsafe {
            // SAFETY: loading an explicit operator-provided dylib is the purpose of this isolated
            // helper. No symbols are invoked until their exact ABI signatures are resolved below.
            Library::new(path)
        }?;
        let api = Self {
            create_ctx: required(&library, b"krun_create_ctx\0")?,
            free_ctx: required(&library, b"krun_free_ctx\0")?,
            set_vm_config: required(&library, b"krun_set_vm_config\0")?,
            set_root: optional(&library, b"krun_set_root\0"),
            add_virtiofs3: optional(&library, b"krun_add_virtiofs3\0"),
            set_exec: required(&library, b"krun_set_exec\0")?,
            set_workdir: required(&library, b"krun_set_workdir\0")?,
            disable_implicit_vsock: optional(&library, b"krun_disable_implicit_vsock\0"),
            add_vsock: required(&library, b"krun_add_vsock\0")?,
            add_disk: optional(&library, b"krun_add_disk\0"),
            add_virtio_console_default: optional(&library, b"krun_add_virtio_console_default\0"),
            start_enter: required(&library, b"krun_start_enter\0")?,
            _library: library,
        };
        Ok(api)
    }

    fn create_context(&self) -> Result<ContextGuard, HelperError> {
        let id = unsafe {
            // SAFETY: this zero-argument function was resolved with its documented ABI.
            (self.create_ctx)()
        };
        if id < 0 {
            return Err(HelperError::KrunCall {
                operation: "krun_create_ctx",
                code: id,
            });
        }
        Ok(ContextGuard {
            id: u32::try_from(id).map_err(|_| HelperError::Invalid("invalid context id"))?,
            free_ctx: self.free_ctx,
            consumed: false,
        })
    }

    fn configure_root(&self, context: u32, path: &Path) -> Result<(), HelperError> {
        let root = path_to_cstring(path)?;
        if let Some(set_root) = self.set_root {
            return Self::check("krun_set_root", unsafe {
                // SAFETY: `root` remains alive for the call and the context is live.
                set_root(context, root.as_ptr())
            });
        }
        let add = self.add_virtiofs3.ok_or(HelperError::MissingRootApi)?;
        Self::check("krun_add_virtiofs3(root)", unsafe {
            // SAFETY: both C strings remain alive and the arguments match libkrun's ABI.
            add(
                context,
                ROOT_TAG.as_ptr(),
                root.as_ptr(),
                DEFAULT_DAX_WINDOW,
                false,
            )
        })
    }

    fn configure_no_tsi(&self, context: u32) -> Result<(), HelperError> {
        if let Some(disable) = self.disable_implicit_vsock {
            Self::check("krun_disable_implicit_vsock", unsafe {
                // SAFETY: function ABI and live context are validated by construction.
                disable(context)
            })?;
        }
        Self::check("krun_add_vsock(tsi=0)", unsafe {
            // SAFETY: zero is the documented no-TSI feature mask.
            (self.add_vsock)(context, 0)
        })
    }

    fn configure_console(&self, context: u32) -> Result<(), HelperError> {
        let Some(add_console) = self.add_virtio_console_default else {
            return Ok(());
        };
        Self::check("krun_add_virtio_console_default", unsafe {
            // SAFETY: descriptors 0/1/2 are owned by this helper and remain valid for VM life.
            add_console(context, 0, 1, 2)
        })
    }

    fn configure_workspace_disk(&self, context: u32, path: &Path) -> Result<(), HelperError> {
        let add_disk = self.add_disk.ok_or(HelperError::MissingDiskApi)?;
        let path = path_to_cstring(path)?;
        Self::check("krun_add_disk(workspace, read_only=true)", unsafe {
            // SAFETY: block ID and path are valid C strings and true requests host-enforced RO.
            add_disk(context, c"workspace".as_ptr(), path.as_ptr(), true)
        })
    }

    fn check(operation: &'static str, result: i32) -> Result<(), HelperError> {
        if result < 0 {
            Err(HelperError::KrunCall {
                operation,
                code: result,
            })
        } else {
            Ok(())
        }
    }
}

struct ContextGuard {
    id: u32,
    free_ctx: FreeCtx,
    consumed: bool,
}

impl ContextGuard {
    fn consume(&mut self) {
        self.consumed = true;
    }
}

impl Drop for ContextGuard {
    fn drop(&mut self) {
        if !self.consumed {
            let _ = unsafe {
                // SAFETY: the context has not been consumed and `free_ctx` matches the ABI.
                (self.free_ctx)(self.id)
            };
        }
    }
}

struct CStringArray {
    _values: Vec<CString>,
    pointers: Vec<*const c_char>,
}

impl CStringArray {
    fn new<I, S>(values: I) -> Result<Self, HelperError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let values = values
            .into_iter()
            .map(|value| CString::new(value.as_ref()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut pointers = values
            .iter()
            .map(|value| value.as_ptr())
            .collect::<Vec<_>>();
        pointers.push(std::ptr::null());
        Ok(Self {
            _values: values,
            pointers,
        })
    }
}

fn required<T: Copy>(library: &Library, symbol: &'static [u8]) -> Result<T, HelperError> {
    unsafe {
        // SAFETY: callers bind each symbol to the exact function pointer declared by libkrun.h.
        library
            .get::<T>(symbol)
            .map(|resolved| *resolved)
            .map_err(HelperError::Load)
    }
}

fn optional<T: Copy>(library: &Library, symbol: &'static [u8]) -> Option<T> {
    unsafe {
        // SAFETY: callers bind each optional symbol to its exact documented ABI.
        library.get::<T>(symbol).ok().map(|resolved| *resolved)
    }
}

fn path_to_cstring(path: &Path) -> Result<CString, HelperError> {
    CString::new(path.as_os_str().as_encoded_bytes()).map_err(HelperError::Nul)
}

fn parse_env(input: &str) -> Result<(String, String), String> {
    let Some((key, value)) = input.split_once('=') else {
        return Err("environment values must use KEY=VALUE".into());
    };
    if key.is_empty() || key.contains('\0') || value.contains('\0') {
        return Err("environment keys and values must be non-empty and NUL-free".into());
    }
    Ok((key.to_owned(), value.to_owned()))
}

fn workspace_command(command: &[String]) -> Vec<String> {
    let mut wrapped = vec![
        "/bin/sh".into(),
        "-c".into(),
        "mkdir -p /workspace && mount -t ext4 -o ro /dev/vda /workspace && cd /workspace && exec \"$@\"".into(),
        "fastmvm-workspace".into(),
    ];
    wrapped.extend_from_slice(command);
    wrapped
}

#[cfg(unix)]
fn spawn_parent_watchdog(parent_pid: u32) {
    std::thread::spawn(move || {
        let expected = libc::pid_t::try_from(parent_pid).unwrap_or(libc::pid_t::MAX);
        loop {
            let current = unsafe {
                // SAFETY: getppid has no preconditions and does not dereference memory.
                libc::getppid()
            };
            if current != expected {
                std::process::exit(137);
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    });
}

#[cfg(not(unix))]
fn spawn_parent_watchdog(_parent_pid: u32) {
    // Windows will use a process/job handle once the native WHP backend is qualified.
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code.clamp(0, 255)).expect("value was clamped to u8"))
}

#[derive(Debug, Error)]
enum HelperError {
    #[error("invalid helper request: {0}")]
    Invalid(&'static str),
    #[error("libkrun has neither krun_set_root nor krun_add_virtiofs3")]
    MissingRootApi,
    #[error("libkrun does not provide krun_add_disk required for workspace isolation")]
    MissingDiskApi,
    #[error("failed to load libkrun symbol: {0}")]
    Load(#[from] libloading::Error),
    #[error("string contains an interior NUL byte: {0}")]
    Nul(#[from] std::ffi::NulError),
    #[error("{operation} failed with libkrun error {code}")]
    KrunCall { operation: &'static str, code: i32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_null_terminated_arrays() {
        let values = CStringArray::new(["a", "b"]).unwrap();
        assert_eq!(values.pointers.len(), 3);
        assert!(values.pointers.last().unwrap().is_null());
    }

    #[test]
    fn rejects_invalid_environment() {
        assert!(parse_env("MISSING").is_err());
        assert!(parse_env("=value").is_err());
    }

    #[test]
    fn workspace_wrapper_preserves_argv() {
        let wrapped = workspace_command(&["/bin/echo".into(), "hello".into()]);
        assert_eq!(&wrapped[4..], ["/bin/echo", "hello"]);
        assert!(wrapped[2].contains("-o ro"));
    }
}
