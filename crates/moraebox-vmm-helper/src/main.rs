//! Minimal process boundary around libkrun's process-consuming `krun_start_enter` API.

#![allow(unsafe_code)]

#[cfg(unix)]
use std::os::{
    fd::FromRawFd as _,
    unix::{
        fs::PermissionsExt as _,
        net::{UnixListener, UnixStream},
    },
};
use std::{
    collections::BTreeMap,
    ffi::{CStr, CString, c_char},
    io,
    path::{Path, PathBuf},
    process::ExitCode,
    thread::JoinHandle,
};
#[cfg(unix)]
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{Read as _, Seek as _, Write as _},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI32, Ordering},
    },
};

use clap::Parser;
use libloading::Library;
#[cfg(unix)]
use moraebox_protocol::{
    CopyChunk, CopyInEnd, CopyInStart, CopyOutEnd, CopyOutRequest, EXEC_STREAM_ID, ExecRequest,
    Exit, Frame, FrameSequence, Hello, InboundValidator, MAX_FRAME_SIZE, MAX_TRANSFER_SIZE, Output,
    PeerRole, ProtocolError, Resize, SignalRequest, Stdin, StdinEof, WireOutputChannel, WireSignal,
    decode_frame, encode_frame, frame, validate_guest_path,
};
#[cfg(unix)]
use sha2::{Digest as _, Sha256};
#[cfg(unix)]
use tempfile::TempDir;
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
type AddVsockPort = unsafe extern "C" fn(u32, u32, *const c_char) -> i32;
type AddNetUnixgram = unsafe extern "C" fn(u32, *const c_char, i32, *mut u8, u32, u32) -> i32;
type AddDisk = unsafe extern "C" fn(u32, *const c_char, *const c_char, bool) -> i32;
type SetRootDiskRemount =
    unsafe extern "C" fn(u32, *const c_char, *const c_char, *const c_char) -> i32;
type DisableImplicitConsole = unsafe extern "C" fn(u32) -> i32;
type AddVirtioConsoleDefault = unsafe extern "C" fn(u32, i32, i32, i32) -> i32;
type StartEnter = unsafe extern "C" fn(u32) -> i32;

const ROOT_TAG: &CStr = c"/dev/root";
const CONTROL_VSOCK_PORT: u32 = 1070;
const GUEST_AGENT_PATH: &str = "/.moraebox-agent";
#[cfg(unix)]
const GUEST_AGENT: &[u8] = include_bytes!(env!("MORAE_GUEST_AGENT_PATH"));
#[cfg(unix)]
const REQUIRED_AGENT_CAPABILITIES: [&str; 7] = [
    "exec",
    "stdin",
    "signal",
    "resize",
    "tty",
    "output-v1",
    "copy-tar-v1",
];
const DEFAULT_COPY_LIMIT: u64 = 64 * 1024 * 1024;
#[cfg(unix)]
const COPY_CHUNK_SIZE: usize = 64 * 1024;
#[cfg(unix)]
const MAX_COPY_ENTRIES: usize = 100_000;
const DEFAULT_DAX_WINDOW: u64 = 256 * 1024 * 1024;
const NETWORK_MAC: [u8; 6] = [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee];
// Released libkrun 1.19.4 ABI constants from libkrun.h.
const NETWORK_FEATURES: u32 = (1 << 0) | (1 << 1) | (1 << 7) | (1 << 10) | (1 << 11) | (1 << 14);
const NET_FLAG_VFKIT: u32 = 1 << 0;
const NET_FLAG_DHCP_CLIENT: u32 = 1 << 1;
const NETWORK_FLAGS: u32 = NET_FLAG_VFKIT | NET_FLAG_DHCP_CLIENT;
#[cfg(unix)]
const PARENT_CONTROL_MESSAGE_BYTES: usize = 5;
#[cfg(unix)]
const PARENT_CONTROL_RESIZE: u8 = 1;
#[cfg(unix)]
const PARENT_CONTROL_INTERRUPT: u8 = 2;
#[cfg(unix)]
const PARENT_CONTROL_TERMINATE: u8 = 3;
#[cfg(unix)]
const PARENT_CONTROL_HANGUP: u8 = 4;

#[derive(Debug, Parser)]
#[command(about = "Signed libkrun VMM helper; normally invoked by morae")]
struct Args {
    #[arg(long)]
    libkrun: PathBuf,
    #[arg(long)]
    #[arg(required_unless_present = "root_disk", conflicts_with = "root_disk")]
    root: Option<PathBuf>,
    /// Writable raw ext4 block device to pivot to as the guest root filesystem.
    #[arg(long, required_unless_present = "root", conflicts_with = "root")]
    root_disk: Option<PathBuf>,
    /// Path to debugfs, used to restore the trusted guest agent before each disk boot.
    #[arg(long, requires = "root_disk")]
    debugfs: Option<PathBuf>,
    /// Session identity enforced by the host/guest protocol.
    #[arg(long, requires = "root_disk")]
    session_id: Option<String>,
    #[arg(long, default_value_t = 2)]
    cpus: u8,
    #[arg(long, default_value_t = 512)]
    memory_mib: u32,
    #[arg(long)]
    cwd: Option<String>,
    /// Read-only ext4 workspace disk; mounted at /workspace before command execution.
    #[arg(long)]
    workspace_disk: Option<PathBuf>,
    /// Mount the immutable workspace disk as an overlay lower with a disposable tmpfs upper.
    #[arg(long, requires = "workspace_disk")]
    workspace_writable: bool,
    /// gvproxy vfkit Unix datagram endpoint for opt-in guest egress.
    #[arg(long)]
    network_socket: Option<PathBuf>,
    /// Supervisor PID. The helper self-terminates if ownership is lost.
    #[arg(long)]
    parent_pid: Option<u32>,
    #[arg(long)]
    tty: bool,
    #[arg(long, default_value_t = 24)]
    tty_rows: u16,
    #[arg(long, default_value_t = 80)]
    tty_cols: u16,
    /// Private FIFO used by the parent runtime for guest signals and terminal resize.
    #[arg(long, requires = "root_disk")]
    control_fifo: Option<PathBuf>,
    #[arg(long = "env", value_parser = parse_env)]
    env: Vec<(String, String)>,
    /// Host source to archive and copy into the guest before execution.
    #[arg(long = "copy-in-source", requires = "root_disk")]
    copy_in_sources: Vec<PathBuf>,
    /// Absolute guest destination paired with --copy-in-source by position.
    #[arg(long = "copy-in-destination", requires = "root_disk")]
    copy_in_destinations: Vec<String>,
    /// Absolute guest source to archive after execution.
    #[arg(long = "copy-out-source", requires = "root_disk")]
    copy_out_sources: Vec<String>,
    /// Host destination paired with --copy-out-source by position.
    #[arg(long = "copy-out-destination", requires = "root_disk")]
    copy_out_destinations: Vec<PathBuf>,
    /// Maximum encoded bytes for each copy operation.
    #[arg(long, default_value_t = DEFAULT_COPY_LIMIT)]
    copy_limit_bytes: u64,
    #[arg(required = true, last = true)]
    command: Vec<String>,
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(code) => exit_code(code),
        Err(error) => {
            eprintln!("morae-vmm-helper: {error}");
            ExitCode::from(125)
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "the helper configures one ordered libkrun context before ownership transfer"
)]
fn run(args: Args) -> Result<i32, HelperError> {
    if args.command.is_empty() || args.command[0].is_empty() {
        return Err(HelperError::Invalid("command must not be empty"));
    }
    if args.cpus == 0 || args.memory_mib == 0 {
        return Err(HelperError::Invalid(
            "CPU and memory limits must be non-zero",
        ));
    }
    if args.tty && (args.tty_rows == 0 || args.tty_cols == 0) {
        return Err(HelperError::Invalid(
            "terminal rows and columns must be non-zero",
        ));
    }
    let transfers = validate_copy_arguments(&args)?;
    if let Some(parent_pid) = args.parent_pid {
        spawn_parent_watchdog(parent_pid);
    }

    if args.workspace_disk.is_some() && args.root_disk.is_none() {
        return Err(HelperError::Unsupported(
            "workspace mounting requires a block root and the guest control agent",
        ));
    }
    let effective_command = args.command.clone();
    let environment = BTreeMap::from_iter(args.env);
    let environment = environment
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>();
    let control = if let Some(root_disk) = args.root_disk.as_deref() {
        let debugfs = args.debugfs.as_deref().ok_or(HelperError::Invalid(
            "--debugfs is required with --root-disk",
        ))?;
        let session_id = args.session_id.as_deref().ok_or(HelperError::Invalid(
            "--session-id is required with --root-disk",
        ))?;
        if session_id.is_empty() {
            return Err(HelperError::Invalid("session ID must not be empty"));
        }
        Some(ControlEndpoint::prepare(debugfs, root_disk)?)
    } else {
        None
    };

    let api = KrunApi::load(&args.libkrun)?;
    let mut context = api.create_context()?;
    KrunApi::check("krun_set_vm_config", unsafe {
        // SAFETY: the function pointer was resolved from the loaded libkrun ABI and the
        // context is live for the duration of this call.
        (api.set_vm_config)(context.id, args.cpus, args.memory_mib)
    })?;
    if let Some(root_disk) = args.root_disk.as_deref() {
        api.configure_root_disk(context.id, root_disk)?;
    } else if let Some(root) = args.root.as_deref() {
        api.configure_root(context.id, root)?;
    } else {
        return Err(HelperError::Invalid(
            "exactly one of --root or --root-disk is required",
        ));
    }
    api.configure_networking(
        context.id,
        control
            .as_ref()
            .map(|endpoint| endpoint.socket_path.as_path()),
        args.network_socket.as_deref(),
    )?;
    #[cfg(unix)]
    let protocol_console_input = if control.is_some() {
        Some(File::open("/dev/null")?)
    } else {
        None
    };
    #[cfg(unix)]
    let protocol_input_fd = protocol_console_input
        .as_ref()
        .map_or(0, std::os::fd::AsRawFd::as_raw_fd);
    #[cfg(not(unix))]
    let protocol_input_fd = 0;
    api.configure_console(
        context.id,
        protocol_input_fd,
        if control.is_some() { 2 } else { 1 },
        2,
    )?;
    if let Some(workspace) = args.workspace_disk.as_deref() {
        api.configure_workspace_disk(context.id, workspace)?;
    }

    if control.is_none() {
        if let Some(cwd) = args.cwd.as_deref() {
            let cwd = CString::new(cwd)?;
            KrunApi::check("krun_set_workdir", unsafe {
                // SAFETY: `cwd` is NUL-terminated and remains alive during the call.
                (api.set_workdir)(context.id, cwd.as_ptr())
            })?;
        }
    }

    let (executable, command, guest_environment) = if control.is_some() {
        let session_id = args
            .session_id
            .as_deref()
            .expect("control endpoint requires a session ID");
        let mut agent_arguments = vec![
            "--port".to_owned(),
            CONTROL_VSOCK_PORT.to_string(),
            "--session".to_owned(),
            session_id.to_owned(),
        ];
        if args.workspace_disk.is_some() {
            agent_arguments.extend(["--workspace-device".to_owned(), "/dev/vdb".to_owned()]);
            if args.workspace_writable {
                agent_arguments.push("--workspace-writable".to_owned());
            }
        }
        (
            CString::new(GUEST_AGENT_PATH)?,
            CStringArray::new(agent_arguments)?,
            CStringArray::new(std::iter::empty::<&str>())?,
        )
    } else {
        (
            CString::new(effective_command[0].as_str())?,
            CStringArray::new(effective_command.iter().skip(1).map(String::as_str))?,
            CStringArray::new(environment.iter().map(String::as_str))?,
        )
    };
    // libkrun's injected init supplies argv[0] from `exec_path`; this array contains
    // only the user arguments that follow the executable.
    KrunApi::check("krun_set_exec", unsafe {
        // SAFETY: all strings and pointer arrays are NUL-terminated and live through this call.
        (api.set_exec)(
            context.id,
            executable.as_ptr(),
            command.pointers.as_ptr(),
            guest_environment.pointers.as_ptr(),
        )
    })?;

    let bridge = control
        .map(|endpoint| {
            endpoint.spawn(BridgeRequest {
                session_id: args
                    .session_id
                    .expect("control endpoint requires a session ID"),
                command: effective_command,
                cwd: args.cwd.unwrap_or_else(|| {
                    args.workspace_disk
                        .as_ref()
                        .map_or_else(String::new, |_| "/workspace".to_owned())
                }),
                environment,
                tty: args.tty,
                rows: args.tty_rows,
                cols: args.tty_cols,
                control_fifo: args.control_fifo,
                copy_in: transfers.copy_in,
                copy_out: transfers.copy_out,
                copy_limit: args.copy_limit_bytes,
            })
        })
        .transpose()?;
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
    } else if let Some(bridge) = bridge {
        bridge.join().map_err(|_| HelperError::BridgeThread)?;
        Ok(result)
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
    add_vsock_port: Option<AddVsockPort>,
    add_net_unixgram: Option<AddNetUnixgram>,
    add_disk: Option<AddDisk>,
    set_root_disk_remount: Option<SetRootDiskRemount>,
    disable_implicit_console: Option<DisableImplicitConsole>,
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
            add_vsock_port: optional(&library, b"krun_add_vsock_port\0"),
            add_net_unixgram: optional(&library, b"krun_add_net_unixgram\0"),
            add_disk: optional(&library, b"krun_add_disk\0"),
            set_root_disk_remount: optional(&library, b"krun_set_root_disk_remount\0"),
            disable_implicit_console: optional(&library, b"krun_disable_implicit_console\0"),
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

    fn configure_networking(
        &self,
        context: u32,
        control_socket: Option<&Path>,
        network_socket: Option<&Path>,
    ) -> Result<(), HelperError> {
        Self::configure_networking_with(
            context,
            control_socket,
            network_socket,
            self.disable_implicit_vsock,
            self.add_vsock,
            self.add_vsock_port,
            self.add_net_unixgram,
        )
    }

    fn configure_root_disk(&self, context: u32, path: &Path) -> Result<(), HelperError> {
        Self::configure_root_disk_with(context, path, self.add_disk, self.set_root_disk_remount)
    }

    fn configure_root_disk_with(
        context: u32,
        path: &Path,
        add_disk: Option<AddDisk>,
        set_root_disk_remount: Option<SetRootDiskRemount>,
    ) -> Result<(), HelperError> {
        let add_disk = add_disk.ok_or(HelperError::MissingDiskApi)?;
        let set_root_disk_remount = set_root_disk_remount.ok_or(HelperError::MissingRootDiskApi)?;
        let path = path_to_cstring(path)?;
        Self::check("krun_add_disk(root, read_only=false)", unsafe {
            // SAFETY: block ID and path are valid C strings and false requests writable access.
            add_disk(context, c"root".as_ptr(), path.as_ptr(), false)
        })?;
        Self::check("krun_set_root_disk_remount(/dev/vda, ext4)", unsafe {
            // SAFETY: all static strings are NUL-terminated and the root disk was added first.
            set_root_disk_remount(
                context,
                c"/dev/vda".as_ptr(),
                c"ext4".as_ptr(),
                std::ptr::null(),
            )
        })
    }

    fn configure_networking_with(
        context: u32,
        control_socket: Option<&Path>,
        network_socket: Option<&Path>,
        disable_implicit_vsock: Option<DisableImplicitVsock>,
        add_vsock: AddVsock,
        add_vsock_port: Option<AddVsockPort>,
        add_network: Option<AddNetUnixgram>,
    ) -> Result<(), HelperError> {
        if let Some(disable) = disable_implicit_vsock {
            Self::check("krun_disable_implicit_vsock", unsafe {
                // SAFETY: function ABI and live context are validated by construction.
                disable(context)
            })?;
        }
        Self::check("krun_add_vsock(tsi=0)", unsafe {
            // SAFETY: zero is the documented no-TSI feature mask.
            add_vsock(context, 0)
        })?;
        if let Some(socket) = control_socket {
            let add_port = add_vsock_port.ok_or(HelperError::MissingVsockPortApi)?;
            let socket = path_to_cstring(socket)?;
            Self::check("krun_add_vsock_port(control)", unsafe {
                // SAFETY: the socket path is a live C string and the port is reserved by moraebox.
                add_port(context, CONTROL_VSOCK_PORT, socket.as_ptr())
            })?;
        }
        if let Some(socket) = network_socket {
            Self::configure_network_with(context, socket, add_network)?;
        }
        Ok(())
    }

    fn configure_network_with(
        context: u32,
        socket: &Path,
        add_network: Option<AddNetUnixgram>,
    ) -> Result<(), HelperError> {
        let add_network = add_network.ok_or(HelperError::MissingNetworkApi)?;
        let socket = path_to_cstring(socket)?;
        let mut mac = NETWORK_MAC;
        Self::check("krun_add_net_unixgram(gvproxy)", unsafe {
            // SAFETY: the socket and MAC buffers remain alive for the call. The flags select
            // gvproxy's vfkit framing and libkrun's guest DHCP client, without enabling TSI.
            add_network(
                context,
                socket.as_ptr(),
                -1,
                mac.as_mut_ptr(),
                NETWORK_FEATURES,
                NETWORK_FLAGS,
            )
        })
    }

    fn configure_console(
        &self,
        context: u32,
        input_fd: i32,
        output_fd: i32,
        error_fd: i32,
    ) -> Result<(), HelperError> {
        Self::configure_console_with(
            context,
            input_fd,
            output_fd,
            error_fd,
            self.disable_implicit_console,
            self.add_virtio_console_default,
        )
    }

    fn configure_console_with(
        context: u32,
        input_fd: i32,
        output_fd: i32,
        error_fd: i32,
        disable_implicit: Option<DisableImplicitConsole>,
        add_console: Option<AddVirtioConsoleDefault>,
    ) -> Result<(), HelperError> {
        let (Some(disable_implicit), Some(add_console)) = (disable_implicit, add_console) else {
            // Older libkrun releases still provide one implicit console wired to stdio.
            return Ok(());
        };
        Self::check("krun_disable_implicit_console", unsafe {
            // SAFETY: the function ABI and live context are validated by construction.
            disable_implicit(context)
        })?;
        Self::check("krun_add_virtio_console_default", unsafe {
            // SAFETY: descriptors 0/1/2 are owned by this helper and remain valid for VM life.
            add_console(context, input_fd, output_fd, error_fd)
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

#[derive(Debug)]
#[cfg_attr(
    not(unix),
    expect(
        dead_code,
        reason = "the non-Unix native stub rejects the request before reading its fields"
    )
)]
struct BridgeRequest {
    session_id: String,
    command: Vec<String>,
    cwd: String,
    environment: Vec<String>,
    tty: bool,
    rows: u16,
    cols: u16,
    control_fifo: Option<PathBuf>,
    copy_in: Vec<CopyInMapping>,
    copy_out: Vec<CopyOutMapping>,
    copy_limit: u64,
}

#[derive(Debug)]
#[cfg_attr(
    not(unix),
    expect(
        dead_code,
        reason = "the non-Unix native stub rejects copy transfers before reading mappings"
    )
)]
struct CopyInMapping {
    source: PathBuf,
    destination: String,
}

#[derive(Debug)]
#[cfg_attr(
    not(unix),
    expect(
        dead_code,
        reason = "the non-Unix native stub rejects copy transfers before reading mappings"
    )
)]
struct CopyOutMapping {
    source: String,
    destination: PathBuf,
}

struct ValidatedTransfers {
    copy_in: Vec<CopyInMapping>,
    copy_out: Vec<CopyOutMapping>,
}

#[cfg(unix)]
struct ControlEndpoint {
    directory: TempDir,
    listener: UnixListener,
    socket_path: PathBuf,
}

#[cfg(unix)]
impl ControlEndpoint {
    fn prepare(debugfs: &Path, root_disk: &Path) -> Result<Self, HelperError> {
        let directory = tempfile::Builder::new()
            .prefix("morae-control-")
            .tempdir_in("/tmp")?;
        inject_guest_agent(debugfs, root_disk, directory.path())?;
        let socket_path = directory.path().join("control.sock");
        let listener = UnixListener::bind(&socket_path)?;
        Ok(Self {
            directory,
            listener,
            socket_path,
        })
    }

    fn spawn(self, request: BridgeRequest) -> Result<JoinHandle<()>, HelperError> {
        std::thread::Builder::new()
            .name("morae-vsock-control".into())
            .spawn(move || match self.serve(request) {
                Ok(code) => std::process::exit(code),
                Err(error) => {
                    eprintln!("morae-vmm-helper: host/guest protocol failed: {error}");
                    std::process::exit(125);
                }
            })
            .map_err(HelperError::Io)
    }

    fn serve(self, request: BridgeRequest) -> Result<i32, BridgeError> {
        let parent_control = request
            .control_fifo
            .as_deref()
            .map(open_parent_control)
            .transpose()?;
        let (mut reader, _) = self.listener.accept()?;
        let sender = Arc::new(Mutex::new(HostSender {
            stream: reader.try_clone()?,
            sequence: FrameSequence::new(&request.session_id, EXEC_STREAM_ID),
        }));
        let mut validator =
            InboundValidator::new(&request.session_id, EXEC_STREAM_ID, PeerRole::Guest);
        let hello_frame = read_protocol_frame(&mut reader)?;
        validator.accept(&hello_frame)?;
        let Some(frame::Payload::Hello(hello)) = hello_frame.payload.as_ref() else {
            return Err(BridgeError::ExpectedHello);
        };
        validate_agent_hello(hello)?;

        let mut next_transfer_id = 1_u64;
        for mapping in &request.copy_in {
            let transfer_id = take_transfer_id(&mut next_transfer_id)?;
            send_copy_in(
                &sender,
                transfer_id,
                mapping,
                request.copy_limit,
                self.directory.path(),
            )?;
        }
        let mut pending_copy_out = BTreeMap::new();
        for mapping in request.copy_out {
            let transfer_id = take_transfer_id(&mut next_transfer_id)?;
            send_host_frame(
                &sender,
                frame::Payload::CopyOutRequest(CopyOutRequest {
                    transfer_id,
                    source: mapping.source.clone(),
                    max_bytes: request.copy_limit,
                }),
            )?;
            pending_copy_out.insert(transfer_id, mapping);
        }

        send_host_frame(
            &sender,
            frame::Payload::Exec(ExecRequest {
                argv: request.command,
                cwd: request.cwd,
                env: request.environment,
                tty: request.tty,
                rows: u32::from(request.rows),
                cols: u32::from(request.cols),
            }),
        )?;
        spawn_stdin_forwarder(Arc::clone(&sender))?;
        spawn_runtime_control_forwarder(Arc::clone(&sender), parent_control, request.tty)?;

        let mut active_copy_out = None;
        loop {
            let frame = read_protocol_frame(&mut reader)?;
            validator.accept(&frame)?;
            match frame.payload.expect("validated frame contains a payload") {
                frame::Payload::Output(output) => write_guest_output(&output)?,
                frame::Payload::CopyOutStart(start) => {
                    if active_copy_out.is_some() {
                        return Err(BridgeError::UnexpectedGuestPayload);
                    }
                    let mapping = pending_copy_out
                        .remove(&start.transfer_id)
                        .ok_or(BridgeError::UnexpectedCopyTransfer(start.transfer_id))?;
                    active_copy_out = Some(ReceivedCopy::new(
                        start.transfer_id,
                        mapping,
                        request.copy_limit,
                        self.directory.path(),
                    )?);
                }
                frame::Payload::CopyOutChunk(chunk) => active_copy_out
                    .as_mut()
                    .ok_or(BridgeError::UnexpectedGuestPayload)?
                    .append(&chunk)?,
                frame::Payload::CopyOutEnd(end) => {
                    let received = active_copy_out
                        .take()
                        .ok_or(BridgeError::UnexpectedGuestPayload)?;
                    received.finish(&end)?;
                }
                frame::Payload::Exit(exit) => {
                    if active_copy_out.is_some() || !pending_copy_out.is_empty() {
                        return Err(BridgeError::IncompleteCopyOut);
                    }
                    return protocol_exit_code(&exit);
                }
                frame::Payload::Shutdown(shutdown) => {
                    return Err(BridgeError::AgentShutdown(shutdown.reason));
                }
                _ => return Err(BridgeError::UnexpectedGuestPayload),
            }
        }
    }
}

#[cfg(unix)]
fn protocol_exit_code(exit: &Exit) -> Result<i32, BridgeError> {
    if let Some(signal) = exit.signal {
        if !(1..=127).contains(&signal) {
            return Err(BridgeError::InvalidExit(format!(
                "signal {signal} is outside 1..=127"
            )));
        }
        if exit.code != 0 {
            return Err(BridgeError::InvalidExit(
                "code must be zero when signal is present".into(),
            ));
        }
        return Ok(128 + signal);
    }
    if !(0..=255).contains(&exit.code) {
        return Err(BridgeError::InvalidExit(format!(
            "code {} is outside 0..=255",
            exit.code
        )));
    }
    Ok(exit.code)
}

#[cfg(not(unix))]
struct ControlEndpoint {
    socket_path: PathBuf,
}

#[cfg(not(unix))]
impl ControlEndpoint {
    fn prepare(_debugfs: &Path, _root_disk: &Path) -> Result<Self, HelperError> {
        Err(HelperError::Unsupported(
            "the vsock guest protocol requires a Unix host",
        ))
    }

    fn spawn(self, _request: BridgeRequest) -> Result<JoinHandle<()>, HelperError> {
        let _ = self;
        Err(HelperError::Unsupported(
            "the vsock guest protocol requires a Unix host",
        ))
    }
}

#[cfg(unix)]
fn inject_guest_agent(debugfs: &Path, root_disk: &Path, staging: &Path) -> Result<(), HelperError> {
    let root_disk = fs::canonicalize(root_disk)?;
    let agent = staging.join("agent");
    fs::write(&agent, GUEST_AGENT)?;
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o700))?;

    let _ = run_debugfs(
        debugfs,
        staging,
        &root_disk,
        "remove previous guest agent",
        "rm /.moraebox-agent",
    );
    run_debugfs(
        debugfs,
        staging,
        &root_disk,
        "write guest agent",
        "write agent /.moraebox-agent",
    )?;
    run_debugfs(
        debugfs,
        staging,
        &root_disk,
        "set guest agent mode",
        "set_inode_field /.moraebox-agent mode 0100755",
    )?;
    let stat = run_debugfs(
        debugfs,
        staging,
        &root_disk,
        "inspect guest agent",
        "stat /.moraebox-agent",
    )?;
    let stat = String::from_utf8_lossy(&stat.stdout);
    if !stat.contains("Mode:  0755") && !stat.contains("Mode:  0100755") {
        return Err(HelperError::GuestAgentVerification(
            "injected agent is not executable".into(),
        ));
    }
    run_debugfs(
        debugfs,
        staging,
        &root_disk,
        "read back guest agent",
        "dump /.moraebox-agent verified-agent",
    )?;
    if fs::read(staging.join("verified-agent"))? != GUEST_AGENT {
        return Err(HelperError::GuestAgentVerification(
            "injected agent content does not match the signed helper".into(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn run_debugfs(
    debugfs: &Path,
    staging: &Path,
    root_disk: &Path,
    operation: &'static str,
    request: &str,
) -> Result<std::process::Output, HelperError> {
    let output = Command::new(debugfs)
        .current_dir(staging)
        .args(["-w", "-R", request])
        .arg(root_disk)
        .output()?;
    if !output.status.success() {
        return Err(HelperError::Debugfs {
            operation,
            status: output.status.code(),
            stderr: bounded_diagnostic(&output.stderr),
        });
    }
    Ok(output)
}

#[cfg(unix)]
fn bounded_diagnostic(bytes: &[u8]) -> String {
    const LIMIT: usize = 4096;
    let start = bytes.len().saturating_sub(LIMIT);
    String::from_utf8_lossy(&bytes[start..]).trim().to_owned()
}

#[cfg(unix)]
struct HostSender {
    stream: UnixStream,
    sequence: FrameSequence,
}

#[cfg(unix)]
impl HostSender {
    fn send(&mut self, payload: frame::Payload) -> Result<(), BridgeError> {
        let frame = self.sequence.next(payload)?;
        let bytes = encode_frame(&frame)?;
        self.stream.write_all(&bytes)?;
        self.stream.flush()?;
        Ok(())
    }
}

#[cfg(unix)]
fn send_host_frame(
    sender: &Arc<Mutex<HostSender>>,
    payload: frame::Payload,
) -> Result<(), BridgeError> {
    sender
        .lock()
        .map_err(|_| BridgeError::SenderPoisoned)?
        .send(payload)
}

#[cfg(unix)]
fn take_transfer_id(next: &mut u64) -> Result<u64, BridgeError> {
    let current = *next;
    *next = next
        .checked_add(1)
        .ok_or(BridgeError::TransferIdsExhausted)?;
    Ok(current)
}

#[cfg(unix)]
fn send_copy_in(
    sender: &Arc<Mutex<HostSender>>,
    transfer_id: u64,
    mapping: &CopyInMapping,
    limit: u64,
    staging: &Path,
) -> Result<(), BridgeError> {
    let mut archive = build_copy_archive(&mapping.source, limit, staging)?;
    send_host_frame(
        sender,
        frame::Payload::CopyInStart(CopyInStart {
            transfer_id,
            destination: mapping.destination.clone(),
            archive_size: archive.size,
            sha256: archive.digest.clone(),
        }),
    )?;
    let mut buffer = vec![0_u8; COPY_CHUNK_SIZE];
    loop {
        let count = archive.file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        send_host_frame(
            sender,
            frame::Payload::CopyInChunk(CopyChunk {
                transfer_id,
                data: buffer[..count].to_vec(),
            }),
        )?;
    }
    send_host_frame(sender, frame::Payload::CopyInEnd(CopyInEnd { transfer_id }))
}

#[cfg(unix)]
struct PreparedCopy {
    file: tempfile::NamedTempFile,
    size: u64,
    digest: String,
}

#[cfg(unix)]
struct BoundedArchiveWriter<'a> {
    file: &'a mut File,
    hasher: Sha256,
    written: u64,
    limit: u64,
}

#[cfg(unix)]
impl io::Write for BoundedArchiveWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = u64::try_from(bytes.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "copy archive chunk is too large",
            )
        })?;
        if self
            .written
            .checked_add(count)
            .is_none_or(|size| size > self.limit)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "copy archive exceeds its byte limit",
            ));
        }
        let written = self.file.write(bytes)?;
        self.hasher.update(&bytes[..written]);
        self.written += u64::try_from(written).expect("usize fits u64 on supported hosts");
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(unix)]
fn build_copy_archive(
    source: &Path,
    limit: u64,
    staging: &Path,
) -> Result<PreparedCopy, BridgeError> {
    let mut file = tempfile::NamedTempFile::new_in(staging)?;
    let (size, digest) = {
        let mut writer = BoundedArchiveWriter {
            file: file.as_file_mut(),
            hasher: Sha256::new(),
            written: 0,
            limit,
        };
        {
            let mut builder = tar::Builder::new(&mut writer);
            let mut entries = 0_usize;
            append_copy_entry(&mut builder, source, Path::new("root"), &mut entries)?;
            builder.finish()?;
        }
        writer.flush()?;
        (
            writer.written,
            format!("sha256:{:x}", writer.hasher.finalize()),
        )
    };
    file.as_file_mut().seek(io::SeekFrom::Start(0))?;
    Ok(PreparedCopy { file, size, digest })
}

#[cfg(unix)]
fn append_copy_entry<W: io::Write>(
    builder: &mut tar::Builder<W>,
    source: &Path,
    name: &Path,
    entries: &mut usize,
) -> Result<(), BridgeError> {
    *entries = entries
        .checked_add(1)
        .ok_or(BridgeError::TooManyCopyEntries)?;
    if *entries > MAX_COPY_ENTRIES {
        return Err(BridgeError::TooManyCopyEntries);
    }
    let metadata = fs::symlink_metadata(source)?;
    let kind = metadata.file_type();
    let mut header = tar::Header::new_gnu();
    header.set_uid(0);
    header.set_gid(0);
    header.set_mode(metadata.permissions().mode() & 0o777);
    header.set_mtime(0);
    if metadata.is_file() {
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(metadata.len());
        header.set_cksum();
        let mut input = File::open(source)?;
        builder.append_data(&mut header, name, &mut input)?;
        return Ok(());
    }
    if metadata.is_dir() {
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_cksum();
        builder.append_data(&mut header, name, io::empty())?;
        let mut children = fs::read_dir(source)?.collect::<Result<Vec<_>, _>>()?;
        children.sort_by_key(std::fs::DirEntry::file_name);
        for child in children {
            append_copy_entry(
                builder,
                &child.path(),
                &name.join(child.file_name()),
                entries,
            )?;
        }
        return Ok(());
    }
    if kind.is_symlink() {
        let target = fs::read_link(source)?;
        if !valid_archive_symlink(name, &target) {
            return Err(BridgeError::UnsafeCopyArchive(format!(
                "symlink {} escapes the copied root",
                source.display()
            )));
        }
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_link_name(target)?;
        header.set_cksum();
        builder.append_data(&mut header, name, io::empty())?;
        return Ok(());
    }
    Err(BridgeError::UnsafeCopyArchive(format!(
        "unsupported file type at {}",
        source.display()
    )))
}

#[cfg(unix)]
struct ReceivedCopy {
    transfer_id: u64,
    mapping: CopyOutMapping,
    file: tempfile::NamedTempFile,
    hasher: Sha256,
    received: u64,
    limit: u64,
}

#[cfg(unix)]
impl ReceivedCopy {
    fn new(
        transfer_id: u64,
        mapping: CopyOutMapping,
        limit: u64,
        staging: &Path,
    ) -> Result<Self, BridgeError> {
        Ok(Self {
            transfer_id,
            mapping,
            file: tempfile::NamedTempFile::new_in(staging)?,
            hasher: Sha256::new(),
            received: 0,
            limit,
        })
    }

    fn append(&mut self, chunk: &CopyChunk) -> Result<(), BridgeError> {
        if chunk.transfer_id != self.transfer_id {
            return Err(BridgeError::UnexpectedCopyTransfer(chunk.transfer_id));
        }
        let count = u64::try_from(chunk.data.len())
            .map_err(|_| BridgeError::CopyLimitExceeded(self.limit))?;
        let next = self
            .received
            .checked_add(count)
            .filter(|size| *size <= self.limit)
            .ok_or(BridgeError::CopyLimitExceeded(self.limit))?;
        self.file.write_all(&chunk.data)?;
        self.hasher.update(&chunk.data);
        self.received = next;
        Ok(())
    }

    fn finish(mut self, end: &CopyOutEnd) -> Result<(), BridgeError> {
        if end.transfer_id != self.transfer_id {
            return Err(BridgeError::UnexpectedCopyTransfer(end.transfer_id));
        }
        if end.total_bytes != self.received {
            return Err(BridgeError::CopySizeMismatch {
                expected: end.total_bytes,
                actual: self.received,
            });
        }
        let digest = format!("sha256:{:x}", self.hasher.finalize());
        if !digest.eq_ignore_ascii_case(&end.sha256) {
            return Err(BridgeError::CopyDigestMismatch);
        }
        self.file.as_file_mut().seek(io::SeekFrom::Start(0))?;
        extract_copy_archive(self.file.as_file_mut(), &self.mapping.destination)
    }
}

#[cfg(unix)]
fn extract_copy_archive<R: io::Read>(
    reader: &mut R,
    destination: &Path,
) -> Result<(), BridgeError> {
    if fs::symlink_metadata(destination).is_ok() {
        return Err(BridgeError::CopyDestinationExists(
            destination.to_path_buf(),
        ));
    }
    let parent = destination.parent().ok_or_else(|| {
        BridgeError::UnsafeCopyArchive("copy-out destination has no parent".into())
    })?;
    fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".morae-copy-")
        .tempdir_in(parent)?;
    let mut archive = tar::Archive::new(reader);
    let mut directories = Vec::new();
    let mut seen = BTreeSet::new();
    let mut count = 0_usize;
    for entry in archive.entries()? {
        count = count
            .checked_add(1)
            .ok_or(BridgeError::TooManyCopyEntries)?;
        if count > MAX_COPY_ENTRIES {
            return Err(BridgeError::TooManyCopyEntries);
        }
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if !valid_archive_path(&path) || !seen.insert(path.clone()) {
            return Err(BridgeError::UnsafeCopyArchive(format!(
                "invalid or duplicate path {}",
                path.display()
            )));
        }
        let target = staging.path().join(&path);
        ensure_real_directory_path(staging.path(), target.parent().expect("entry has parent"))?;
        let mode = entry.header().mode()? & 0o777;
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            fs::create_dir(&target)?;
            directories.push((target, mode));
        } else if kind.is_file() {
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)?;
            io::copy(&mut entry, &mut output)?;
            fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
        } else if kind.is_symlink() {
            let link = entry
                .link_name()?
                .ok_or_else(|| BridgeError::UnsafeCopyArchive("symlink has no target".into()))?;
            if !valid_archive_symlink(&path, &link) {
                return Err(BridgeError::UnsafeCopyArchive(format!(
                    "symlink {} escapes the copied root",
                    path.display()
                )));
            }
            std::os::unix::fs::symlink(link, target)?;
        } else {
            return Err(BridgeError::UnsafeCopyArchive(format!(
                "unsupported tar entry type {}",
                kind.as_byte()
            )));
        }
    }
    let root = staging.path().join("root");
    if fs::symlink_metadata(&root).is_err() {
        return Err(BridgeError::UnsafeCopyArchive(
            "archive is missing its root entry".into(),
        ));
    }
    for (directory, mode) in directories.into_iter().rev() {
        fs::set_permissions(directory, fs::Permissions::from_mode(mode))?;
    }
    fs::rename(root, destination)?;
    Ok(())
}

#[cfg(unix)]
fn valid_archive_path(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(std::path::Component::Normal(root)) if root == "root")
        && components.all(|component| matches!(component, std::path::Component::Normal(_)))
}

#[cfg(unix)]
fn valid_archive_symlink(name: &Path, target: &Path) -> bool {
    if target.as_os_str().is_empty() || target.is_absolute() {
        return false;
    }
    let mut resolved = name
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value.to_os_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    for component in target.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(value) => resolved.push(value.to_os_string()),
            std::path::Component::ParentDir if !resolved.is_empty() => {
                resolved.pop();
            }
            _ => return false,
        }
    }
    resolved
        .first()
        .is_some_and(|component| component == "root")
}

#[cfg(unix)]
fn ensure_real_directory_path(root: &Path, directory: &Path) -> Result<(), BridgeError> {
    let relative = directory.strip_prefix(root).map_err(|_| {
        BridgeError::UnsafeCopyArchive("archive path escapes its staging directory".into())
    })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(BridgeError::UnsafeCopyArchive(
                "archive parent path is not normalized".into(),
            ));
        };
        current.push(component);
        let metadata = fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(BridgeError::UnsafeCopyArchive(
                "archive parent is not a real directory".into(),
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn read_protocol_frame(stream: &mut UnixStream) -> Result<Frame, BridgeError> {
    let mut header = [0_u8; 4];
    stream.read_exact(&mut header)?;
    let declared = u32::from_be_bytes(header) as usize;
    if declared > MAX_FRAME_SIZE {
        return Err(BridgeError::FrameTooLarge(declared));
    }
    let mut bytes = vec![0_u8; 4 + declared];
    bytes[..4].copy_from_slice(&header);
    stream.read_exact(&mut bytes[4..])?;
    Ok(decode_frame(&bytes)?)
}

#[cfg(unix)]
fn validate_agent_hello(hello: &Hello) -> Result<(), BridgeError> {
    if hello.agent_version.is_empty() {
        return Err(BridgeError::MissingAgentVersion);
    }
    let missing = REQUIRED_AGENT_CAPABILITIES
        .iter()
        .filter(|required| {
            !hello
                .capabilities
                .iter()
                .any(|capability| capability == **required)
        })
        .copied()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(BridgeError::MissingCapabilities(missing.join(", ")))
    }
}

#[cfg(unix)]
fn write_guest_output(output: &Output) -> Result<(), BridgeError> {
    match WireOutputChannel::try_from(output.channel)
        .map_err(|_| BridgeError::InvalidOutputChannel(output.channel))?
    {
        WireOutputChannel::Stdout | WireOutputChannel::Tty => {
            let mut stdout = io::stdout().lock();
            stdout.write_all(&output.data)?;
            stdout.flush()?;
        }
        WireOutputChannel::Stderr => {
            let mut stderr = io::stderr().lock();
            stderr.write_all(&output.data)?;
            stderr.flush()?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn spawn_stdin_forwarder(sender: Arc<Mutex<HostSender>>) -> Result<(), BridgeError> {
    std::thread::Builder::new()
        .name("morae-vsock-stdin".into())
        .spawn(move || {
            let mut stdin = io::stdin().lock();
            let mut buffer = vec![0_u8; 32 * 1024].into_boxed_slice();
            loop {
                match stdin.read(&mut buffer) {
                    Ok(0) => {
                        let _ = send_host_frame(&sender, frame::Payload::StdinEof(StdinEof {}));
                        return;
                    }
                    Ok(count) => {
                        if send_host_frame(
                            &sender,
                            frame::Payload::Stdin(Stdin {
                                data: buffer[..count].to_vec(),
                            }),
                        )
                        .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => return,
                }
            }
        })?;
    Ok(())
}

#[cfg(unix)]
fn spawn_runtime_control_forwarder(
    sender: Arc<Mutex<HostSender>>,
    parent_control: Option<File>,
    tty: bool,
) -> Result<(), BridgeError> {
    if let Some(control) = parent_control {
        spawn_parent_control_forwarder(sender, control)
    } else {
        // Compatibility for direct helper invocations that predate the parent control FIFO.
        // Install after guest connection because libkrun may replace dispositions on entry.
        spawn_signal_forwarder(sender, install_signal_relay()?, tty)
    }
}

#[cfg(unix)]
fn spawn_signal_forwarder(
    sender: Arc<Mutex<HostSender>>,
    mut signals: File,
    tty: bool,
) -> Result<(), BridgeError> {
    std::thread::Builder::new()
        .name("morae-vsock-signals".into())
        .spawn(move || {
            let mut signal = [0_u8; 1];
            while signals.read_exact(&mut signal).is_ok() {
                let payload = match i32::from(signal[0]) {
                    libc::SIGINT => Some(frame::Payload::Signal(SignalRequest {
                        signal: WireSignal::Interrupt as i32,
                    })),
                    libc::SIGTERM => Some(frame::Payload::Signal(SignalRequest {
                        signal: WireSignal::Terminate as i32,
                    })),
                    libc::SIGHUP => Some(frame::Payload::Signal(SignalRequest {
                        signal: WireSignal::Hangup as i32,
                    })),
                    libc::SIGWINCH if tty => terminal_size().map(|(rows, cols)| {
                        frame::Payload::Resize(Resize {
                            rows: u32::from(rows),
                            cols: u32::from(cols),
                        })
                    }),
                    _ => None,
                };
                if let Some(payload) = payload {
                    if send_host_frame(&sender, payload).is_err() {
                        return;
                    }
                }
            }
        })?;
    Ok(())
}

#[cfg(unix)]
fn spawn_parent_control_forwarder(
    sender: Arc<Mutex<HostSender>>,
    mut control: File,
) -> Result<(), BridgeError> {
    std::thread::Builder::new()
        .name("morae-parent-control".into())
        .spawn(move || {
            loop {
                let mut message = [0_u8; PARENT_CONTROL_MESSAGE_BYTES];
                if control.read_exact(&mut message).is_err() {
                    return;
                }
                let Some(payload) = decode_parent_control(message) else {
                    return;
                };
                if send_host_frame(&sender, payload).is_err() {
                    return;
                }
            }
        })?;
    Ok(())
}

#[cfg(unix)]
fn open_parent_control(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::{FileTypeExt as _, OpenOptionsExt as _};

    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_fifo() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "parent control path is not a FIFO",
        ));
    }
    let control = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    std::fs::remove_file(path)?;
    Ok(control)
}

#[cfg(unix)]
fn decode_parent_control(message: [u8; PARENT_CONTROL_MESSAGE_BYTES]) -> Option<frame::Payload> {
    match message[0] {
        PARENT_CONTROL_RESIZE => {
            let rows = u16::from_be_bytes([message[1], message[2]]);
            let cols = u16::from_be_bytes([message[3], message[4]]);
            (rows != 0 && cols != 0).then(|| {
                frame::Payload::Resize(Resize {
                    rows: u32::from(rows),
                    cols: u32::from(cols),
                })
            })
        }
        PARENT_CONTROL_INTERRUPT => Some(frame::Payload::Signal(SignalRequest {
            signal: WireSignal::Interrupt as i32,
        })),
        PARENT_CONTROL_TERMINATE => Some(frame::Payload::Signal(SignalRequest {
            signal: WireSignal::Terminate as i32,
        })),
        PARENT_CONTROL_HANGUP => Some(frame::Payload::Signal(SignalRequest {
            signal: WireSignal::Hangup as i32,
        })),
        _ => None,
    }
}

#[cfg(unix)]
fn terminal_size() -> Option<(u16, u16)> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe {
        // SAFETY: fd 0 is live for the helper and points to a writable winsize structure.
        libc::ioctl(0, libc::TIOCGWINSZ, &mut size)
    };
    (result == 0 && size.ws_row != 0 && size.ws_col != 0).then_some((size.ws_row, size.ws_col))
}

#[cfg(unix)]
static SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

#[cfg(unix)]
extern "C" fn relay_signal(signal: libc::c_int) {
    let fd = SIGNAL_WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let value = u8::try_from(signal).unwrap_or(0);
        let _ = unsafe {
            // SAFETY: the descriptor is the write end of a process-lifetime pipe and write is
            // async-signal-safe. The one-byte buffer lives for the duration of the call.
            libc::write(fd, (&raw const value).cast(), 1)
        };
    }
}

#[cfg(unix)]
fn install_signal_relay() -> io::Result<File> {
    let mut descriptors = [-1; 2];
    if unsafe {
        // SAFETY: descriptors points to two valid integers for libc to initialize.
        libc::pipe(descriptors.as_mut_ptr())
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    for descriptor in descriptors {
        let result = unsafe {
            // SAFETY: both descriptors were returned by pipe and F_SETFD accepts FD_CLOEXEC.
            libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC)
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    SIGNAL_WRITE_FD.store(descriptors[1], Ordering::Release);
    let mut action = unsafe {
        // SAFETY: a zeroed sigaction is initialized below before being installed.
        std::mem::zeroed::<libc::sigaction>()
    };
    action.sa_sigaction = relay_signal as *const () as usize;
    action.sa_flags = libc::SA_RESTART;
    unsafe {
        // SAFETY: action owns a valid signal mask and handler function.
        libc::sigemptyset(&raw mut action.sa_mask);
    }
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGWINCH] {
        if unsafe {
            // SAFETY: signal is catchable and action remains valid throughout the call.
            libc::sigaction(signal, &raw const action, std::ptr::null_mut())
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    let reader = unsafe {
        // SAFETY: ownership of the read descriptor is transferred exactly once to File.
        File::from_raw_fd(descriptors[0])
    };
    Ok(reader)
}

#[cfg(unix)]
#[derive(Debug, Error)]
enum BridgeError {
    #[error("protocol I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("invalid protocol frame: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("guest frame size {0} exceeds the protocol limit")]
    FrameTooLarge(usize),
    #[error("the first guest frame was not Hello")]
    ExpectedHello,
    #[error("guest agent version is empty")]
    MissingAgentVersion,
    #[error("guest agent is missing required capabilities: {0}")]
    MissingCapabilities(String),
    #[error("guest sent an invalid output channel {0}")]
    InvalidOutputChannel(i32),
    #[error("guest sent an invalid exit status: {0}")]
    InvalidExit(String),
    #[error("guest sent a payload that is invalid in the running state")]
    UnexpectedGuestPayload,
    #[error("guest agent shut down the session: {0}")]
    AgentShutdown(String),
    #[error("protocol sender lock was poisoned")]
    SenderPoisoned,
    #[error("protocol transfer id space is exhausted")]
    TransferIdsExhausted,
    #[error("guest used unexpected copy transfer id {0}")]
    UnexpectedCopyTransfer(u64),
    #[error("guest exited before every copy-out completed")]
    IncompleteCopyOut,
    #[error("copy transfer exceeded its {0}-byte limit")]
    CopyLimitExceeded(u64),
    #[error("copy transfer size mismatch: expected {expected}, received {actual}")]
    CopySizeMismatch { expected: u64, actual: u64 },
    #[error("copy transfer sha256 digest does not match")]
    CopyDigestMismatch,
    #[error("copy archive contains more than {MAX_COPY_ENTRIES} entries")]
    TooManyCopyEntries,
    #[error("unsafe copy archive: {0}")]
    UnsafeCopyArchive(String),
    #[error("copy-out destination already exists: {}", .0.display())]
    CopyDestinationExists(PathBuf),
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

#[cfg(unix)]
fn validate_copy_arguments(args: &Args) -> Result<ValidatedTransfers, HelperError> {
    if args.copy_in_sources.len() != args.copy_in_destinations.len() {
        return Err(HelperError::InvalidCopy(
            "--copy-in-source and --copy-in-destination counts differ".into(),
        ));
    }
    if args.copy_out_sources.len() != args.copy_out_destinations.len() {
        return Err(HelperError::InvalidCopy(
            "--copy-out-source and --copy-out-destination counts differ".into(),
        ));
    }
    if args.copy_limit_bytes == 0 || args.copy_limit_bytes > MAX_TRANSFER_SIZE {
        return Err(HelperError::InvalidCopy(format!(
            "--copy-limit-bytes must be in 1..={MAX_TRANSFER_SIZE}"
        )));
    }
    let mut guest_destinations = BTreeSet::new();
    let mut copy_in = Vec::with_capacity(args.copy_in_sources.len());
    for (source, destination) in args.copy_in_sources.iter().zip(&args.copy_in_destinations) {
        validate_guest_path(destination)
            .map_err(|error| HelperError::InvalidCopy(error.to_string()))?;
        if !guest_destinations.insert(destination.clone()) {
            return Err(HelperError::InvalidCopy(format!(
                "duplicate guest copy-in destination {destination:?}"
            )));
        }
        let metadata = fs::symlink_metadata(source).map_err(|error| {
            HelperError::InvalidCopy(format!("cannot inspect {}: {error}", source.display()))
        })?;
        if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()) {
            return Err(HelperError::InvalidCopy(format!(
                "copy-in source must be a regular file or directory: {}",
                source.display()
            )));
        }
        copy_in.push(CopyInMapping {
            source: source.clone(),
            destination: destination.clone(),
        });
    }
    let mut host_destinations = BTreeSet::new();
    let mut copy_out = Vec::with_capacity(args.copy_out_sources.len());
    for (source, destination) in args
        .copy_out_sources
        .iter()
        .zip(&args.copy_out_destinations)
    {
        validate_guest_path(source).map_err(|error| HelperError::InvalidCopy(error.to_string()))?;
        if !destination.is_absolute() {
            return Err(HelperError::InvalidCopy(format!(
                "copy-out destination must be absolute: {}",
                destination.display()
            )));
        }
        if !host_destinations.insert(destination.clone()) {
            return Err(HelperError::InvalidCopy(format!(
                "duplicate host copy-out destination {}",
                destination.display()
            )));
        }
        if fs::symlink_metadata(destination).is_ok() {
            return Err(HelperError::InvalidCopy(format!(
                "copy-out destination already exists: {}",
                destination.display()
            )));
        }
        copy_out.push(CopyOutMapping {
            source: source.clone(),
            destination: destination.clone(),
        });
    }
    Ok(ValidatedTransfers { copy_in, copy_out })
}

#[cfg(not(unix))]
fn validate_copy_arguments(args: &Args) -> Result<ValidatedTransfers, HelperError> {
    if !args.copy_in_sources.is_empty() || !args.copy_out_sources.is_empty() {
        return Err(HelperError::Unsupported(
            "copy transfer requires a Unix host",
        ));
    }
    Ok(ValidatedTransfers {
        copy_in: Vec::new(),
        copy_out: Vec::new(),
    })
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
    #[cfg(unix)]
    #[error("invalid copy request: {0}")]
    InvalidCopy(String),
    #[error("libkrun has neither krun_set_root nor krun_add_virtiofs3")]
    MissingRootApi,
    #[error("libkrun does not provide krun_add_disk required for workspace isolation")]
    MissingDiskApi,
    #[error("libkrun does not provide krun_set_root_disk_remount required for block root")]
    MissingRootDiskApi,
    #[error("libkrun does not provide krun_add_net_unixgram required for network access")]
    MissingNetworkApi,
    #[error("libkrun does not provide krun_add_vsock_port required for guest control")]
    MissingVsockPortApi,
    #[error("host platform is unsupported: {0}")]
    Unsupported(&'static str),
    #[error("helper I/O failed: {0}")]
    Io(#[from] io::Error),
    #[cfg(unix)]
    #[error("debugfs failed to {operation} with status {status:?}: {stderr}")]
    Debugfs {
        operation: &'static str,
        status: Option<i32>,
        stderr: String,
    },
    #[cfg(unix)]
    #[error("guest agent verification failed: {0}")]
    GuestAgentVerification(String),
    #[error("host/guest protocol thread panicked")]
    BridgeThread,
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
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    static CONSOLE_TEST_LOCK: Mutex<()> = Mutex::new(());
    static CONSOLE_CALL_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    static NETWORK_TEST_LOCK: Mutex<()> = Mutex::new(());
    static NETWORK_CALL_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn record_disable_implicit_console(context: u32) -> i32 {
        if context == 7 {
            let _ =
                CONSOLE_CALL_SEQUENCE.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
        }
        0
    }

    unsafe extern "C" fn record_add_console(
        context: u32,
        input_fd: i32,
        output_fd: i32,
        err_fd: i32,
    ) -> i32 {
        if (context, input_fd, output_fd, err_fd) == (7, 0, 1, 2) {
            let _ =
                CONSOLE_CALL_SEQUENCE.compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst);
        }
        0
    }

    unsafe extern "C" fn record_add_network(
        context: u32,
        socket: *const c_char,
        fd: i32,
        mac: *mut u8,
        features: u32,
        flags: u32,
    ) -> i32 {
        let socket = unsafe {
            // SAFETY: configure_network_with passes a live NUL-terminated CString.
            CStr::from_ptr(socket)
        };
        let mac = unsafe {
            // SAFETY: configure_network_with passes a live six-byte MAC buffer.
            std::slice::from_raw_parts(mac, NETWORK_MAC.len())
        };
        if context == 9
            && socket == c"/tmp/gvproxy.sock"
            && fd == -1
            && mac == NETWORK_MAC
            && features == NETWORK_FEATURES
            && flags == NETWORK_FLAGS
        {
            let _ =
                NETWORK_CALL_SEQUENCE.compare_exchange(2, 3, Ordering::SeqCst, Ordering::SeqCst);
        }
        0
    }

    unsafe extern "C" fn record_disable_implicit_vsock(context: u32) -> i32 {
        if context == 9 {
            let _ =
                NETWORK_CALL_SEQUENCE.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
        }
        0
    }

    unsafe extern "C" fn record_add_vsock(context: u32, tsi_features: u32) -> i32 {
        if context == 9 && tsi_features == 0 {
            let _ =
                NETWORK_CALL_SEQUENCE.compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst);
        }
        0
    }

    unsafe extern "C" fn record_add_vsock_port(
        context: u32,
        port: u32,
        socket: *const c_char,
    ) -> i32 {
        let socket = unsafe {
            // SAFETY: the test caller passes a live NUL-terminated control socket path.
            CStr::from_ptr(socket)
        };
        if context == 9 && port == CONTROL_VSOCK_PORT && socket == c"/tmp/control.sock" {
            let _ =
                NETWORK_CALL_SEQUENCE.compare_exchange(2, 3, Ordering::SeqCst, Ordering::SeqCst);
        }
        0
    }

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

    static ROOT_DISK_TEST_LOCK: Mutex<()> = Mutex::new(());
    static ROOT_DISK_CALL_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn record_add_root_disk(
        context: u32,
        block_id: *const c_char,
        path: *const c_char,
        read_only: bool,
    ) -> i32 {
        let block_id = unsafe { CStr::from_ptr(block_id) };
        let path = unsafe { CStr::from_ptr(path) };
        if context == 11 && block_id == c"root" && path == c"/tmp/root.ext4" && !read_only {
            let _ =
                ROOT_DISK_CALL_SEQUENCE.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
        }
        0
    }

    unsafe extern "C" fn record_root_remount(
        context: u32,
        device: *const c_char,
        fstype: *const c_char,
        options: *const c_char,
    ) -> i32 {
        let device = unsafe { CStr::from_ptr(device) };
        let fstype = unsafe { CStr::from_ptr(fstype) };
        if context == 11 && device == c"/dev/vda" && fstype == c"ext4" && options.is_null() {
            let _ =
                ROOT_DISK_CALL_SEQUENCE.compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst);
        }
        0
    }

    #[test]
    fn adds_writable_root_disk_before_configuring_the_pivot() {
        let _guard = ROOT_DISK_TEST_LOCK.lock().unwrap();
        ROOT_DISK_CALL_SEQUENCE.store(0, Ordering::SeqCst);

        KrunApi::configure_root_disk_with(
            11,
            Path::new("/tmp/root.ext4"),
            Some(record_add_root_disk),
            Some(record_root_remount),
        )
        .unwrap();

        assert_eq!(ROOT_DISK_CALL_SEQUENCE.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn disables_implicit_console_before_adding_explicit_console() {
        let _guard = CONSOLE_TEST_LOCK.lock().unwrap();
        CONSOLE_CALL_SEQUENCE.store(0, Ordering::SeqCst);

        KrunApi::configure_console_with(
            7,
            0,
            1,
            2,
            Some(record_disable_implicit_console),
            Some(record_add_console),
        )
        .unwrap();

        assert_eq!(CONSOLE_CALL_SEQUENCE.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn keeps_implicit_console_when_disable_api_is_unavailable() {
        let _guard = CONSOLE_TEST_LOCK.lock().unwrap();
        CONSOLE_CALL_SEQUENCE.store(0, Ordering::SeqCst);

        KrunApi::configure_console_with(7, 0, 1, 2, None, Some(record_add_console)).unwrap();

        assert_eq!(CONSOLE_CALL_SEQUENCE.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn configures_gvproxy_without_tsi_features() {
        let _guard = NETWORK_TEST_LOCK.lock().unwrap();
        NETWORK_CALL_SEQUENCE.store(0, Ordering::SeqCst);

        KrunApi::configure_networking_with(
            9,
            None,
            Some(Path::new("/tmp/gvproxy.sock")),
            Some(record_disable_implicit_vsock),
            record_add_vsock,
            None,
            Some(record_add_network),
        )
        .unwrap();

        assert_eq!(NETWORK_CALL_SEQUENCE.load(Ordering::SeqCst), 3);
        assert_eq!(NETWORK_FLAGS & !0b11, 0);
    }

    #[test]
    fn network_off_still_adds_control_vsock_with_tsi_disabled() {
        let _guard = NETWORK_TEST_LOCK.lock().unwrap();
        NETWORK_CALL_SEQUENCE.store(0, Ordering::SeqCst);

        KrunApi::configure_networking_with(
            9,
            None,
            None,
            Some(record_disable_implicit_vsock),
            record_add_vsock,
            None,
            Some(record_add_network),
        )
        .unwrap();

        assert_eq!(NETWORK_CALL_SEQUENCE.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn maps_the_control_port_after_adding_a_tsi_disabled_vsock() {
        let _guard = NETWORK_TEST_LOCK.lock().unwrap();
        NETWORK_CALL_SEQUENCE.store(0, Ordering::SeqCst);

        KrunApi::configure_networking_with(
            9,
            Some(Path::new("/tmp/control.sock")),
            None,
            Some(record_disable_implicit_vsock),
            record_add_vsock,
            Some(record_add_vsock_port),
            None,
        )
        .unwrap();

        assert_eq!(NETWORK_CALL_SEQUENCE.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn network_requires_the_released_libkrun_api() {
        assert!(matches!(
            KrunApi::configure_network_with(9, Path::new("/tmp/gvproxy.sock"), None),
            Err(HelperError::MissingNetworkApi)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn embedded_guest_agent_is_a_static_linux_arm64_elf() {
        assert_eq!(&GUEST_AGENT[..4], b"\x7fELF");
        assert_eq!(GUEST_AGENT[4], 2, "agent must use ELF64");
        assert_eq!(u16::from_le_bytes([GUEST_AGENT[18], GUEST_AGENT[19]]), 183);
    }

    #[cfg(unix)]
    #[test]
    fn trusted_agent_is_written_verified_and_made_executable() {
        let state = tempfile::tempdir().unwrap();
        let staging = state.path().join("staging");
        fs::create_dir(&staging).unwrap();
        let disk = state.path().join("root.ext4");
        fs::write(&disk, []).unwrap();
        let debugfs = state.path().join("debugfs");
        fs::write(
            &debugfs,
            "#!/bin/sh\ncase \"$3\" in\n  'stat '*) printf 'Inode: 12 Type: regular Mode:  0755\\n';;\n  'dump '*) cp agent verified-agent;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&debugfs, fs::Permissions::from_mode(0o700)).unwrap();

        inject_guest_agent(&debugfs, &disk, &staging).unwrap();

        assert_eq!(
            fs::read(staging.join("verified-agent")).unwrap(),
            GUEST_AGENT
        );
    }

    #[cfg(unix)]
    #[test]
    fn host_frames_start_with_exec_sequence_zero() {
        let (host, mut guest) = UnixStream::pair().unwrap();
        let mut sender = HostSender {
            stream: host,
            sequence: FrameSequence::new("session", EXEC_STREAM_ID),
        };
        sender
            .send(frame::Payload::Exec(ExecRequest {
                argv: vec!["/bin/true".into()],
                cwd: String::new(),
                env: Vec::new(),
                tty: false,
                rows: 24,
                cols: 80,
            }))
            .unwrap();

        let frame = read_protocol_frame(&mut guest).unwrap();
        assert_eq!(frame.sequence, 0);
        assert_eq!(frame.session_id, "session");
        assert!(matches!(frame.payload, Some(frame::Payload::Exec(_))));
    }

    #[cfg(unix)]
    #[test]
    fn parent_control_frames_are_strict_and_typed() {
        assert!(matches!(
            decode_parent_control([PARENT_CONTROL_RESIZE, 0, 41, 0, 99]),
            Some(frame::Payload::Resize(Resize { rows: 41, cols: 99 }))
        ));
        assert!(matches!(
            decode_parent_control([PARENT_CONTROL_TERMINATE, 0, 0, 0, 0]),
            Some(frame::Payload::Signal(SignalRequest { signal }))
                if signal == WireSignal::Terminate as i32
        ));
        assert!(decode_parent_control([PARENT_CONTROL_RESIZE, 0, 0, 0, 99]).is_none());
        assert!(decode_parent_control([u8::MAX, 0, 0, 0, 0]).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn parent_control_fifo_is_unlinked_after_open() {
        use std::os::unix::fs::OpenOptionsExt as _;

        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("control.fifo");
        let path_c = path_to_cstring(&path).unwrap();
        // SAFETY: path_c is a live NUL-terminated path inside the private test directory.
        assert_eq!(unsafe { libc::mkfifo(path_c.as_ptr(), 0o600) }, 0);
        let _writer = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();

        let _reader = open_parent_control(&path).unwrap();
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn hello_requires_every_execution_capability() {
        let error = validate_agent_hello(&Hello {
            agent_version: "test".into(),
            capabilities: vec!["exec".into()],
        })
        .unwrap_err();
        assert!(matches!(error, BridgeError::MissingCapabilities(_)));
    }

    #[cfg(unix)]
    #[test]
    fn protocol_exit_status_controls_helper_status() {
        assert_eq!(
            protocol_exit_code(&Exit {
                code: 23,
                signal: None,
            })
            .unwrap(),
            23
        );
        assert_eq!(
            protocol_exit_code(&Exit {
                code: 0,
                signal: Some(15),
            })
            .unwrap(),
            143
        );
        assert!(
            protocol_exit_code(&Exit {
                code: 1,
                signal: Some(15),
            })
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_archive_round_trips_without_following_links() {
        let state = tempfile::tempdir().unwrap();
        let source = state.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("value"), b"hello").unwrap();
        std::os::unix::fs::symlink("value", source.join("link")).unwrap();
        let staging = state.path().join("staging");
        fs::create_dir(&staging).unwrap();
        let mut archive = build_copy_archive(&source, 1024 * 1024, &staging).unwrap();
        assert!(archive.size > 0);
        assert!(archive.digest.starts_with("sha256:"));

        let destination = state.path().join("destination");
        extract_copy_archive(archive.file.as_file_mut(), &destination).unwrap();

        assert_eq!(fs::read(destination.join("value")).unwrap(), b"hello");
        assert_eq!(
            fs::read_link(destination.join("link")).unwrap(),
            Path::new("value")
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_archive_rejects_traversal_and_escaping_links() {
        assert!(valid_archive_symlink(
            Path::new("root/directory/link"),
            Path::new("../value")
        ));
        assert!(!valid_archive_symlink(
            Path::new("root/link"),
            Path::new("../../outside")
        ));
        assert!(!valid_archive_symlink(
            Path::new("root"),
            Path::new("outside")
        ));

        let mut encoded = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut encoded);
            let mut root = tar::Header::new_gnu();
            root.set_entry_type(tar::EntryType::Directory);
            root.set_mode(0o755);
            root.set_size(0);
            root.set_cksum();
            builder.append_data(&mut root, "root", io::empty()).unwrap();
            let mut bad = tar::Header::new_gnu();
            bad.set_entry_type(tar::EntryType::Regular);
            bad.set_mode(0o600);
            bad.set_size(0);
            let name = b"root/../../escape";
            bad.as_mut_bytes()[..name.len()].copy_from_slice(name);
            bad.set_cksum();
            builder.append(&bad, io::empty()).unwrap();
            builder.finish().unwrap();
        }
        let state = tempfile::tempdir().unwrap();
        let mut encoded = encoded.as_slice();
        assert!(extract_copy_archive(&mut encoded, &state.path().join("destination")).is_err());
        assert!(!state.path().join("escape").exists());
    }
}
