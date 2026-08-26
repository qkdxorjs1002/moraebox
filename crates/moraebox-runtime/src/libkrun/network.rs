use std::{
    fmt::Write as _,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    os::{fd::AsRawFd as _, unix::net::UnixDatagram as StdUnixDatagram},
    path::{Path, PathBuf},
    process::Stdio,
};

use moraebox_core::{NetworkMode, PublishProtocol, PublishRequest, RunSpec};
use nix::{
    fcntl::{FcntlArg, FdFlag, fcntl},
    sys::socket::{setsockopt, sockopt},
};
use serde::Serialize;
use tempfile::TempDir;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::{UnixDatagram, UnixStream},
    process::{Child, ChildStderr, Command},
    sync::oneshot,
    task::JoinHandle,
    time::{Instant, sleep},
};

use super::{
    LibkrunConfig, NETWORK_PROXY_POLL_INTERVAL, NETWORK_PROXY_START_TIMEOUT,
    NETWORK_PROXY_STDERR_FINISH_TIMEOUT, NETWORK_PROXY_STDERR_LIMIT,
    policy::{
        Cidr, DomainPattern, FrameDirection, PolicyConfig, PolicyDecision, PolicyEngine,
        PolicyLimits, PolicyMode,
    },
};
use crate::BackendError;

const GVPROXY_GATEWAY: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 1);
const GVPROXY_GUEST: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 2);
const VFKIT_MAGIC: &[u8] = b"VFKT";
const MAX_ETHERNET_FRAME: usize = 65_535;
const SOCKET_RECEIVE_BUFFER: usize = 7 * 1024 * 1024;
const MAX_SERVICES_RESPONSE: u64 = 64 * 1024;
const AUTO_PORT_ATTEMPTS: usize = 8;

#[derive(Debug)]
pub(super) struct NetworkProxy {
    child: Option<Child>,
    stderr: Option<BoundedStderr>,
    relay: Option<PolicyRelay>,
    state: Option<TempDir>,
    published_ports: Vec<PublishRequest>,
    socket_path: Option<PathBuf>,
    helper_socket: Option<StdUnixDatagram>,
}

impl NetworkProxy {
    pub(super) async fn start_for_spec(
        config: &LibkrunConfig,
        spec: &RunSpec,
    ) -> Result<Self, BackendError> {
        Self::start_configured(config, Some(spec), |_| {}).await
    }

    #[cfg(test)]
    pub(super) async fn start_observed(
        config: &LibkrunConfig,
        observe_spawn: impl FnOnce(Option<u32>),
    ) -> Result<Self, BackendError> {
        Self::start_configured(config, None, observe_spawn).await
    }

    #[expect(
        clippy::too_many_lines,
        reason = "network startup keeps gvproxy, relay, forwarding, and cleanup ownership ordered"
    )]
    async fn start_configured(
        config: &LibkrunConfig,
        spec: Option<&RunSpec>,
        observe_spawn: impl FnOnce(Option<u32>),
    ) -> Result<Self, BackendError> {
        let executable = config.network_proxy_path()?;
        let state = create_network_state(config)?;
        let upstream_path = state.path().join("gvproxy.sock");
        let upstream_uri = vfkit_socket_uri(&upstream_path)?;
        let services_path = spec
            .filter(|spec| !spec.publish.is_empty())
            .map(|_| state.path().join("services.sock"));
        let policy = spec.map(policy_config_for_spec).transpose()?.flatten();

        let mut command = Command::new(executable);
        command
            .arg("--listen-vfkit")
            .arg(upstream_uri)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .env_clear()
            .kill_on_drop(true);
        if let Some(path) = &services_path {
            command.arg("--services").arg(unix_socket_uri(path)?);
        }
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
            relay: None,
            state: Some(state),
            published_ports: Vec::new(),
            socket_path: Some(upstream_path.clone()),
            helper_socket: None,
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
            let vfkit_probe = probe_vfkit_endpoint(&upstream_path);
            let services_probe = services_path
                .as_ref()
                .map_or(Ok(()), |path| probe_services_endpoint(path));
            let probe_error = match vfkit_probe.and(services_probe) {
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

        if let Some(policy) = policy {
            let relay_path = proxy
                .state
                .as_ref()
                .expect("network proxy owns its state during startup")
                .path()
                .join("relay.sock");
            match PolicyRelay::start(&relay_path, &upstream_path, policy) {
                Ok((relay, helper_socket)) => {
                    proxy.relay = Some(relay);
                    proxy.socket_path = None;
                    proxy.helper_socket = Some(helper_socket);
                }
                Err(error) => {
                    let mut message = format!("failed to start network policy relay: {error}");
                    if let Err(cleanup) = proxy.stop().await {
                        let _ = write!(message, "; cleanup failed: {cleanup}");
                    }
                    return Err(BackendError::Control(message));
                }
            }
        }

        if let (Some(path), Some(spec)) = (services_path.as_deref(), spec) {
            match expose_ports(path, &spec.publish).await {
                Ok(published) => proxy.published_ports = published,
                Err(error) => {
                    let mut message = format!("failed to publish preview port: {error}");
                    if let Err(cleanup) = proxy.stop().await {
                        let _ = write!(message, "; cleanup failed: {cleanup}");
                    }
                    return Err(BackendError::Control(message));
                }
            }
        }

        Ok(proxy)
    }

    pub(super) fn published_ports(&self) -> &[PublishRequest] {
        &self.published_ports
    }

    pub(super) fn append_helper_network_argument(&self, command: &mut Command) {
        if let Some(socket) = &self.helper_socket {
            command
                .arg("--network-fd")
                .arg(socket.as_raw_fd().to_string());
        } else if let Some(path) = &self.socket_path {
            command.arg("--network-socket").arg(path);
        }
    }

    pub(super) fn helper_spawned(&mut self) {
        self.helper_socket.take();
    }

    pub(super) async fn stop(mut self) -> io::Result<()> {
        cleanup_network_proxy(
            self.child.take(),
            self.stderr.take(),
            self.relay.take(),
            self.state.take(),
        )
        .await
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
        let relay = self.relay.take();
        let state = self.state.take();
        if child.is_none() && stderr.is_none() && relay.is_none() && state.is_none() {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = cleanup_network_proxy(child, stderr, relay, state).await;
            });
        } else {
            if let Some(mut child) = child {
                let _ = child.start_kill();
            }
            if let Some(stderr) = stderr {
                stderr.abort();
            }
            if let Some(relay) = relay {
                relay.abort();
            }
            drop(state);
        }
    }
}

async fn cleanup_network_proxy(
    child: Option<Child>,
    stderr: Option<BoundedStderr>,
    relay: Option<PolicyRelay>,
    state: Option<TempDir>,
) -> io::Result<()> {
    let relay_result = if let Some(relay) = relay {
        relay.stop().await
    } else {
        Ok(())
    };
    let child_result = if let Some(mut child) = child {
        stop_network_child(&mut child).await
    } else {
        Ok(())
    };
    if let Some(stderr) = stderr {
        stderr.abort();
    }
    let state_result = state.map_or(Ok(()), TempDir::close);
    relay_result?;
    child_result?;
    state_result
}

async fn stop_network_child(child: &mut Child) -> io::Result<()> {
    if child.try_wait()?.is_none() {
        let kill_result = child.start_kill();
        if child.try_wait()?.is_none() {
            kill_result?;
        }
    }
    child.wait().await.map(|_| ())
}

#[derive(Debug)]
struct PolicyRelay {
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<io::Result<()>>>,
}

impl PolicyRelay {
    fn start(
        relay_path: &Path,
        upstream_path: &Path,
        config: PolicyConfig,
    ) -> io::Result<(Self, StdUnixDatagram)> {
        let (guest, helper) = StdUnixDatagram::pair()?;
        prepare_helper_socket(&helper)?;
        prepare_datagram_socket(&guest)?;
        guest.set_nonblocking(true)?;

        let upstream = StdUnixDatagram::bind(relay_path)?;
        prepare_datagram_socket(&upstream)?;
        upstream.connect(upstream_path)?;
        upstream.send(VFKIT_MAGIC)?;
        upstream.set_nonblocking(true)?;

        let (shutdown, shutdown_receiver) = oneshot::channel();
        let task = tokio::spawn(run_policy_relay(
            UnixDatagram::from_std(guest)?,
            UnixDatagram::from_std(upstream)?,
            PolicyEngine::new(config),
            shutdown_receiver,
        ));
        Ok((
            Self {
                shutdown: Some(shutdown),
                task: Some(task),
            },
            helper,
        ))
    }

    async fn stop(mut self) -> io::Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        let result = task
            .await
            .map_err(|error| io::Error::other(format!("network policy relay failed: {error}")))?;
        match result {
            // gvproxy may unlink its connected datagram endpoint as soon as the VM disconnects.
            // The relay is already fail-closed at that point, and the gvproxy child is reaped
            // separately, so endpoint disappearance during ordered cleanup is not a run failure.
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }

    fn abort(mut self) {
        self.shutdown.take();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for PolicyRelay {
    fn drop(&mut self) {
        self.shutdown.take();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn run_policy_relay(
    guest: UnixDatagram,
    upstream: UnixDatagram,
    mut policy: PolicyEngine,
    mut shutdown: oneshot::Receiver<()>,
) -> io::Result<()> {
    let started = Instant::now();
    let mut guest_frame = vec![0_u8; MAX_ETHERNET_FRAME];
    let mut network_frame = vec![0_u8; MAX_ETHERNET_FRAME];
    loop {
        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            received = guest.recv(&mut guest_frame) => {
                let length = received?;
                let frame = &guest_frame[..length];
                let decision = policy.evaluate_ethernet(
                    FrameDirection::GuestToNetwork,
                    frame,
                    started.elapsed(),
                );
                if matches!(decision, PolicyDecision::Allow(_)) {
                    upstream.send(frame).await?;
                }
            }
            received = upstream.recv(&mut network_frame) => {
                let length = received?;
                let frame = &network_frame[..length];
                let decision = policy.evaluate_ethernet(
                    FrameDirection::NetworkToGuest,
                    frame,
                    started.elapsed(),
                );
                if matches!(decision, PolicyDecision::Allow(_)) {
                    guest.send(frame).await?;
                }
            }
        }
    }
}

fn prepare_helper_socket(socket: &StdUnixDatagram) -> io::Result<()> {
    prepare_datagram_socket(socket)?;
    let raw_flags = fcntl(socket, FcntlArg::F_GETFD).map_err(io::Error::from)?;
    let mut flags = FdFlag::from_bits_truncate(raw_flags);
    flags.remove(FdFlag::FD_CLOEXEC);
    fcntl(socket, FcntlArg::F_SETFD(flags)).map_err(io::Error::from)?;
    Ok(())
}

fn prepare_datagram_socket(socket: &StdUnixDatagram) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    setsockopt(socket, sockopt::SndBuf, &MAX_ETHERNET_FRAME).map_err(io::Error::from)?;
    #[cfg(not(target_os = "macos"))]
    setsockopt(socket, sockopt::SndBuf, &SOCKET_RECEIVE_BUFFER).map_err(io::Error::from)?;
    setsockopt(socket, sockopt::RcvBuf, &SOCKET_RECEIVE_BUFFER).map_err(io::Error::from)?;
    Ok(())
}

fn policy_config_for_spec(spec: &RunSpec) -> Result<Option<PolicyConfig>, BackendError> {
    match spec.effective_network_mode() {
        NetworkMode::Disabled | NetworkMode::Unrestricted => Ok(None),
        NetworkMode::Allowlist => {
            let policy = spec.network_policy.as_ref().ok_or_else(|| {
                BackendError::Control("allowlist mode requires a typed network policy".into())
            })?;
            let cidrs = policy
                .allow_cidrs
                .iter()
                .map(|cidr| {
                    cidr.parse::<Cidr>().map_err(|error| {
                        BackendError::Control(format!("invalid network CIDR {cidr}: {error}"))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let domains = policy
                .allow_domains
                .iter()
                .map(|domain| {
                    domain.parse::<DomainPattern>().map_err(|error| {
                        BackendError::Control(format!(
                            "invalid network domain pattern {domain}: {error}"
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let config = PolicyConfig::new(
                PolicyMode::Allowlist,
                cidrs,
                domains,
                [IpAddr::V4(GVPROXY_GATEWAY)],
                [GVPROXY_GATEWAY],
                [IpAddr::V4(GVPROXY_GATEWAY)],
                PolicyLimits::default(),
            )
            .and_then(|config| config.with_guest_ipv4_addresses([GVPROXY_GUEST]))
            .and_then(|config| {
                config
                    .with_published_tcp_ports(spec.publish.iter().map(|request| request.guest_port))
            })
            .map_err(|error| BackendError::Control(format!("invalid network policy: {error}")))?;
            Ok(Some(config))
        }
    }
}

#[derive(Serialize)]
struct ExposeRequest {
    local: String,
    remote: String,
    protocol: &'static str,
}

async fn expose_ports(
    services_path: &Path,
    requests: &[PublishRequest],
) -> io::Result<Vec<PublishRequest>> {
    let mut published = Vec::with_capacity(requests.len());
    for request in requests {
        let attempts = if request.host_port == 0 {
            AUTO_PORT_ATTEMPTS
        } else {
            1
        };
        let mut last_error = None;
        for _ in 0..attempts {
            let host_port = if request.host_port == 0 {
                allocate_loopback_port(request.host_address)?
            } else {
                request.host_port
            };
            let exposed = PublishRequest {
                protocol: request.protocol,
                host_address: request.host_address,
                host_port,
                guest_port: request.guest_port,
            };
            match expose_port(services_path, &exposed).await {
                Ok(()) => {
                    published.push(exposed);
                    last_error = None;
                    break;
                }
                Err(error) => last_error = Some(error),
            }
        }
        if let Some(error) = last_error {
            return Err(error);
        }
    }
    Ok(published)
}

fn allocate_loopback_port(address: IpAddr) -> io::Result<u16> {
    let listener = TcpListener::bind(SocketAddr::new(address, 0))?;
    listener.local_addr().map(|address| address.port())
}

async fn expose_port(services_path: &Path, request: &PublishRequest) -> io::Result<()> {
    debug_assert_eq!(request.protocol, PublishProtocol::Tcp);
    let body = serde_json::to_vec(&ExposeRequest {
        local: SocketAddr::new(request.host_address, request.host_port).to_string(),
        remote: SocketAddr::new(IpAddr::V4(GVPROXY_GUEST), request.guest_port).to_string(),
        protocol: "tcp",
    })
    .map_err(io::Error::other)?;
    services_post(services_path, "/services/forwarder/expose", &body).await
}

async fn services_post(path: &Path, endpoint: &str, body: &[u8]) -> io::Result<()> {
    let mut stream = UnixStream::connect(path).await?;
    let header = format!(
        "POST {endpoint} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await?;
    let mut response = Vec::new();
    stream
        .take(MAX_SERVICES_RESPONSE + 1)
        .read_to_end(&mut response)
        .await?;
    if response.len() as u64 > MAX_SERVICES_RESPONSE {
        return Err(io::Error::other(
            "gvproxy services response exceeded 64 KiB",
        ));
    }
    let response = String::from_utf8_lossy(&response);
    let status = response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse::<u16>().ok())
        .ok_or_else(|| io::Error::other("gvproxy services returned an invalid HTTP response"))?;
    if (200..300).contains(&status) {
        Ok(())
    } else {
        let diagnostics = response
            .split_once("\r\n\r\n")
            .map_or("", |(_, body)| body.trim());
        Err(io::Error::other(format!(
            "gvproxy services returned HTTP {status}: {diagnostics}"
        )))
    }
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

#[cfg(unix)]
fn probe_services_endpoint(path: &Path) -> io::Result<()> {
    std::os::unix::net::UnixStream::connect(path).map(|_| ())
}

#[cfg(not(unix))]
fn probe_services_endpoint(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Unix stream endpoints are unavailable on this platform",
    ))
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
    unix_socket_uri_with_scheme(path, "unixgram")
}

fn unix_socket_uri(path: &Path) -> Result<String, BackendError> {
    unix_socket_uri_with_scheme(path, "unix")
}

fn unix_socket_uri_with_scheme(path: &Path, scheme: &str) -> Result<String, BackendError> {
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
    Ok(format!("{scheme}://{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{net::UnixListener, time::timeout};

    #[tokio::test]
    async fn services_client_publishes_multiple_auto_ports() {
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("services.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let body = read_http_body(&mut stream).await;
                bodies.push(serde_json::from_slice::<serde_json::Value>(&body).unwrap());
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
            }
            bodies
        });
        let requests = [
            PublishRequest {
                protocol: PublishProtocol::Tcp,
                host_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                host_port: 0,
                guest_port: 3000,
            },
            PublishRequest {
                protocol: PublishProtocol::Tcp,
                host_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                host_port: 0,
                guest_port: 8080,
            },
        ];
        let published = expose_ports(&path, &requests).await.unwrap();
        assert_eq!(published.len(), 2);
        assert!(published.iter().all(|request| request.host_port != 0));
        assert_ne!(published[0].host_port, published[1].host_port);
        let bodies = server.await.unwrap();
        assert_eq!(bodies[0]["remote"], "192.168.127.2:3000");
        assert_eq!(bodies[1]["remote"], "192.168.127.2:8080");
        assert_eq!(bodies[0]["protocol"], "tcp");
    }

    #[tokio::test]
    async fn policy_relay_forwards_only_allowed_and_correlated_frames() {
        let state = tempfile::tempdir().unwrap();
        let upstream_path = state.path().join("upstream.sock");
        let relay_path = state.path().join("relay.sock");
        let upstream = UnixDatagram::bind(&upstream_path).unwrap();
        let config = PolicyConfig::new(
            PolicyMode::Unrestricted,
            [],
            [],
            [],
            [],
            [],
            PolicyLimits::default(),
        )
        .unwrap();
        let (relay, guest) = PolicyRelay::start(&relay_path, &upstream_path, config).unwrap();
        let fd_flags = fcntl(&guest, FcntlArg::F_GETFD).unwrap();
        assert!(!FdFlag::from_bits_truncate(fd_flags).contains(FdFlag::FD_CLOEXEC));
        guest.set_nonblocking(true).unwrap();
        let guest = UnixDatagram::from_std(guest).unwrap();

        let mut buffer = vec![0_u8; MAX_ETHERNET_FRAME];
        let (length, relay_peer) = timeout(
            std::time::Duration::from_secs(1),
            upstream.recv_from(&mut buffer),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&buffer[..length], VFKIT_MAGIC);
        let relay_peer = relay_peer.as_pathname().unwrap().to_path_buf();

        let outbound = udp_frame(
            Ipv4Addr::new(192, 168, 127, 2),
            Ipv4Addr::new(203, 0, 113, 7),
            40_000,
            1234,
        );
        guest.send(&outbound).await.unwrap();
        let (length, _) = timeout(
            std::time::Duration::from_secs(1),
            upstream.recv_from(&mut buffer),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&buffer[..length], outbound);

        let reverse = udp_frame(
            Ipv4Addr::new(203, 0, 113, 7),
            Ipv4Addr::new(192, 168, 127, 2),
            1234,
            40_000,
        );
        upstream.send_to(&reverse, &relay_peer).await.unwrap();
        let length = timeout(std::time::Duration::from_secs(1), guest.recv(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buffer[..length], reverse);

        let unsolicited = udp_frame(
            Ipv4Addr::new(203, 0, 113, 8),
            Ipv4Addr::new(192, 168, 127, 2),
            1234,
            40_001,
        );
        upstream.send_to(&unsolicited, &relay_peer).await.unwrap();
        assert!(
            timeout(
                std::time::Duration::from_millis(50),
                guest.recv(&mut buffer),
            )
            .await
            .is_err()
        );
        relay.stop().await.unwrap();
    }

    #[tokio::test]
    async fn policy_relay_cleanup_tolerates_an_unlinked_upstream_endpoint() {
        let task = tokio::spawn(async {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "upstream endpoint was removed",
            ))
        });
        let relay = PolicyRelay {
            shutdown: None,
            task: Some(task),
        };

        relay.stop().await.unwrap();
    }

    async fn read_http_body(stream: &mut UnixStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        let header_end = loop {
            let length = stream.read(&mut chunk).await.unwrap();
            assert_ne!(length, 0);
            request.extend_from_slice(&chunk[..length]);
            if let Some(position) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap();
        while request.len() - header_end < content_length {
            let length = stream.read(&mut chunk).await.unwrap();
            assert_ne!(length, 0);
            request.extend_from_slice(&chunk[..length]);
        }
        request[header_end..header_end + content_length].to_vec()
    }

    fn udp_frame(
        source: Ipv4Addr,
        destination: Ipv4Addr,
        source_port: u16,
        destination_port: u16,
    ) -> Vec<u8> {
        let mut udp = Vec::new();
        udp.extend_from_slice(&source_port.to_be_bytes());
        udp.extend_from_slice(&destination_port.to_be_bytes());
        udp.extend_from_slice(&12_u16.to_be_bytes());
        udp.extend_from_slice(&[0, 0]);
        udp.extend_from_slice(b"data");
        let mut ip = vec![0_u8; 20];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&u16::try_from(20 + udp.len()).unwrap().to_be_bytes());
        ip[8] = 64;
        ip[9] = 17;
        ip[12..16].copy_from_slice(&source.octets());
        ip[16..20].copy_from_slice(&destination.octets());
        let checksum = ipv4_checksum(&ip);
        ip[10..12].copy_from_slice(&checksum.to_be_bytes());
        ip.extend_from_slice(&udp);
        let mut ethernet = vec![0_u8; 14];
        ethernet[12..14].copy_from_slice(&0x0800_u16.to_be_bytes());
        ethernet.extend_from_slice(&ip);
        ethernet
    }

    fn ipv4_checksum(bytes: &[u8]) -> u16 {
        let mut sum = bytes.chunks(2).fold(0_u32, |sum, chunk| {
            let word = if chunk.len() == 2 {
                u16::from_be_bytes([chunk[0], chunk[1]])
            } else {
                u16::from(chunk[0]) << 8
            };
            sum + u32::from(word)
        });
        while sum >> 16 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        !u16::try_from(sum).expect("folded IPv4 checksum fits in 16 bits")
    }
}
