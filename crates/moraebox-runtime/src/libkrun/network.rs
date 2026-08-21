use std::{
    fmt::Write as _,
    io,
    path::{Path, PathBuf},
    process::Stdio,
};

use tempfile::TempDir;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, ChildStderr, Command},
    task::JoinHandle,
    time::{Instant, sleep},
};

use super::{
    LibkrunConfig, NETWORK_PROXY_POLL_INTERVAL, NETWORK_PROXY_START_TIMEOUT,
    NETWORK_PROXY_STDERR_FINISH_TIMEOUT, NETWORK_PROXY_STDERR_LIMIT,
};
use crate::BackendError;

#[derive(Debug)]
pub(super) struct NetworkProxy {
    child: Option<Child>,
    stderr: Option<BoundedStderr>,
    state: Option<TempDir>,
    pub(super) socket_path: PathBuf,
}

impl NetworkProxy {
    pub(super) async fn start(config: &LibkrunConfig) -> Result<Self, BackendError> {
        Self::start_observed(config, |_| {}).await
    }

    pub(super) async fn start_observed(
        config: &LibkrunConfig,
        observe_spawn: impl FnOnce(Option<u32>),
    ) -> Result<Self, BackendError> {
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
            .stderr(Stdio::piped())
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
        observe_spawn(child.id());
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| BackendError::Control("gvproxy stderr pipe was not created".into()))?;
        let mut proxy = Self {
            child: Some(child),
            stderr: Some(BoundedStderr::spawn(stderr)),
            state: Some(state),
            socket_path,
        };
        let started = Instant::now();
        loop {
            if let Some(status) = proxy.child_mut()?.try_wait()? {
                let diagnostics = proxy.finish_stderr().await.map_err(BackendError::Io)?;
                let mut message = format!(
                    "gvproxy exited before its vfkit endpoint was ready: {status}{}",
                    stderr_diagnostics(&diagnostics)
                );
                if let Err(error) = proxy.stop().await {
                    let _ = write!(message, "; cleanup failed: {error}");
                }
                return Err(BackendError::Control(message));
            }
            let probe_error = match probe_vfkit_endpoint(&proxy.socket_path) {
                Ok(()) => break,
                Err(error) => error.to_string(),
            };
            if started.elapsed() >= NETWORK_PROXY_START_TIMEOUT {
                let diagnostics = proxy.finish_stderr().await.map_err(BackendError::Io)?;
                let mut message = format!(
                    "gvproxy vfkit endpoint was not connectable within 5 seconds; last socket error: {probe_error}{}",
                    stderr_diagnostics(&diagnostics)
                );
                if let Err(error) = proxy.stop().await {
                    let _ = write!(message, "; cleanup failed: {error}");
                }
                return Err(BackendError::Control(message));
            }
            sleep(NETWORK_PROXY_POLL_INTERVAL).await;
        }

        Ok(proxy)
    }

    pub(super) async fn stop(mut self) -> io::Result<()> {
        cleanup_network_proxy(self.child.take(), self.stderr.take(), self.state.take()).await
    }

    fn child_mut(&mut self) -> io::Result<&mut Child> {
        self.child
            .as_mut()
            .ok_or_else(|| io::Error::other("gvproxy child is no longer owned"))
    }

    async fn finish_stderr(&mut self) -> io::Result<Vec<u8>> {
        let Some(stderr) = self.stderr.take() else {
            return Ok(Vec::new());
        };
        stderr.finish().await
    }
}

impl Drop for NetworkProxy {
    fn drop(&mut self) {
        let child = self.child.take();
        let stderr = self.stderr.take();
        let state = self.state.take();
        if child.is_none() && stderr.is_none() && state.is_none() {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = cleanup_network_proxy(child, stderr, state).await;
            });
        } else {
            if let Some(mut child) = child {
                let _ = child.start_kill();
            }
            if let Some(stderr) = stderr {
                stderr.abort();
            }
            drop(state);
        }
    }
}

async fn cleanup_network_proxy(
    child: Option<Child>,
    stderr: Option<BoundedStderr>,
    state: Option<TempDir>,
) -> io::Result<()> {
    let child_result = if let Some(mut child) = child {
        stop_network_child(&mut child).await
    } else {
        Ok(())
    };
    if let Some(stderr) = stderr {
        stderr.abort();
    }
    let state_result = state.map_or(Ok(()), TempDir::close);
    child_result?;
    state_result
}

async fn stop_network_child(child: &mut Child) -> io::Result<()> {
    if child.try_wait()?.is_none()
        && let Err(error) = child.start_kill()
        && child.try_wait()?.is_none()
    {
        return Err(error);
    }
    child.wait().await.map(|_| ())
}
#[derive(Debug)]
struct BoundedStderr {
    task: JoinHandle<io::Result<Vec<u8>>>,
}

impl BoundedStderr {
    fn spawn(stderr: ChildStderr) -> Self {
        Self {
            task: tokio::spawn(read_bounded_tail(stderr, NETWORK_PROXY_STDERR_LIMIT)),
        }
    }

    async fn finish(mut self) -> io::Result<Vec<u8>> {
        if let Ok(result) =
            tokio::time::timeout(NETWORK_PROXY_STDERR_FINISH_TIMEOUT, &mut self.task).await
        {
            result
                .map_err(|error| io::Error::other(format!("gvproxy stderr task failed: {error}")))?
        } else {
            self.task.abort();
            Ok(Vec::new())
        }
    }

    fn abort(self) {
        self.task.abort();
    }
}

async fn read_bounded_tail(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> io::Result<Vec<u8>> {
    let mut retained = Vec::with_capacity(limit);
    let mut chunk = [0_u8; 4096];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Ok(retained);
        }
        append_bounded_tail(&mut retained, &chunk[..read], limit);
    }
}

pub(crate) fn append_bounded_tail(retained: &mut Vec<u8>, bytes: &[u8], limit: usize) {
    if limit == 0 {
        retained.clear();
        return;
    }
    if bytes.len() >= limit {
        retained.clear();
        retained.extend_from_slice(&bytes[bytes.len() - limit..]);
        return;
    }
    let overflow = retained
        .len()
        .saturating_add(bytes.len())
        .saturating_sub(limit);
    if overflow > 0 {
        retained.drain(..overflow);
    }
    retained.extend_from_slice(bytes);
}

pub(crate) fn stderr_diagnostics(stderr: &[u8]) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!(
            "; stderr (last {} bytes): {}",
            stderr.len(),
            String::from_utf8_lossy(stderr).trim()
        )
    }
}

#[cfg(unix)]
pub(crate) fn probe_vfkit_endpoint(path: &Path) -> io::Result<()> {
    let socket = std::os::unix::net::UnixDatagram::unbound()?;
    socket.connect(path)
}

#[cfg(not(unix))]
pub(crate) fn probe_vfkit_endpoint(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Unix datagram endpoints are unavailable on this platform",
    ))
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

pub(super) fn vfkit_socket_uri(path: &Path) -> Result<String, BackendError> {
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
