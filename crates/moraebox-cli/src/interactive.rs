use super::{
    Arc, AsyncWriteExt, Backend, CliErrorSource, IsTerminal, OutputChannel, OutputReadError, Read,
    RunBudget, RunSpec, SessionError, SessionHandle, SessionManager, SessionState, SessionStatus,
    Signal, io,
};

const INTERACTIVE_READ_BYTES: usize = 64 * 1024;

pub(super) async fn run_interactive<B>(
    backend: B,
    mut spec: RunSpec,
    budget: RunBudget,
) -> Result<i32, CliErrorSource>
where
    B: Backend + 'static,
{
    let host_terminal = spec.tty && io::stdin().is_terminal();
    if host_terminal {
        if let Some((rows, columns)) = terminal_window_size()? {
            spec.tty_rows = rows;
            spec.tty_columns = columns;
        }
    }
    let _terminal = RawTerminalGuard::enter(host_terminal)?;
    let mut input = InteractiveInput::new()?;
    let mut signals = HostSignals::new()?;
    let session = SessionManager::new(Arc::new(backend))
        .start_with_budget(spec, budget)
        .await?;
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    let mut cursor = 0_u64;
    let mut input_open = true;

    let input_session = session.clone();
    let input_future = forward_interactive_input(&mut input, input_session);
    tokio::pin!(input_future);
    let wait_future = session.wait();
    tokio::pin!(wait_future);

    let status = loop {
        drain_interactive_output(&session, &mut cursor, &mut stdout, &mut stderr).await?;
        let output_future = session.wait_for_output(cursor);
        tokio::pin!(output_future);

        tokio::select! {
            status = &mut wait_future => break status?,
            input_result = &mut input_future, if input_open => {
                match input_result {
                    Ok(()) => input_open = false,
                    Err(_) if session.status().state == SessionState::Dead => {
                        input_open = false;
                    }
                    Err(error) => return Err(error),
                }
            }
            output_result = &mut output_future => {
                output_result?;
            }
            signal_result = signals.recv() => {
                let request = match signal_result? {
                    HostEvent::Signal(signal) => session.signal(signal).await,
                    HostEvent::Resize(rows, columns) => session.resize(rows, columns).await,
                };
                let error = request.err().filter(|_| session.status().state != SessionState::Dead);
                if let Some(error) = error {
                    return Err(error.into());
                }
            }
        }
    };

    drain_interactive_output(&session, &mut cursor, &mut stdout, &mut stderr).await?;
    stdout.flush().await?;
    stderr.flush().await?;
    Ok(session_exit_code(&status))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(unix),
    expect(dead_code, reason = "window-change events are available only on Unix")
)]
enum HostEvent {
    Signal(Signal),
    Resize(u16, u16),
}

#[cfg(unix)]
fn terminal_window_size() -> io::Result<Option<(u16, u16)>> {
    let window = rustix::termios::tcgetwinsize(io::stdin()).map_err(io::Error::from)?;
    Ok((window.ws_row != 0 && window.ws_col != 0).then_some((window.ws_row, window.ws_col)))
}

#[cfg(not(unix))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the unsupported stub mirrors the fallible Unix terminal interface"
)]
fn terminal_window_size() -> io::Result<Option<(u16, u16)>> {
    Ok(None)
}

async fn forward_interactive_input(
    input: &mut InteractiveInput,
    session: SessionHandle,
) -> Result<(), CliErrorSource> {
    let mut buffer = vec![0_u8; 16 * 1024];
    loop {
        let count = input.read(&mut buffer).await?;
        if count == 0 {
            session.close_stdin().await?;
            return Ok(());
        }
        session.write(buffer[..count].to_vec()).await?;
    }
}

async fn drain_interactive_output(
    session: &SessionHandle,
    cursor: &mut u64,
    stdout: &mut tokio::io::Stdout,
    stderr: &mut tokio::io::Stderr,
) -> Result<(), CliErrorSource> {
    loop {
        let output = match session.read_output(*cursor, INTERACTIVE_READ_BYTES).await {
            Ok(output) => output,
            Err(SessionError::Output(OutputReadError::CursorExpired { earliest, .. })) => {
                let warning =
                    format!("morae: interactive output before cursor {earliest} was dropped\n");
                stderr.write_all(warning.as_bytes()).await?;
                *cursor = earliest;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if output.chunks.is_empty() {
            return Ok(());
        }
        for chunk in output.chunks {
            match chunk.channel {
                OutputChannel::Stdout | OutputChannel::Tty => {
                    stdout.write_all(&chunk.data).await?;
                }
                OutputChannel::Stderr => stderr.write_all(&chunk.data).await?,
            }
        }
        *cursor = output.next_cursor;
        stdout.flush().await?;
        stderr.flush().await?;
    }
}

fn session_exit_code(status: &SessionStatus) -> i32 {
    if status.timed_out {
        124
    } else if let Some(code) = status.exit_code {
        code
    } else if let Some(signal) = status.signal {
        128 + signal
    } else {
        125
    }
}

#[cfg(unix)]
struct RawTerminalGuard {
    original: nix::sys::termios::Termios,
}

#[cfg(unix)]
impl RawTerminalGuard {
    fn enter(enabled: bool) -> io::Result<Option<Self>> {
        use nix::sys::termios::{SetArg, cfmakeraw, tcgetattr, tcsetattr};

        if !enabled {
            return Ok(None);
        }
        let stdin = io::stdin();
        let original = tcgetattr(&stdin).map_err(io::Error::from)?;
        let mut raw = original.clone();
        cfmakeraw(&mut raw);
        tcsetattr(&stdin, SetArg::TCSANOW, &raw).map_err(io::Error::from)?;
        Ok(Some(Self { original }))
    }
}

#[cfg(unix)]
impl Drop for RawTerminalGuard {
    fn drop(&mut self) {
        use nix::sys::termios::{SetArg, tcsetattr};

        let _ = tcsetattr(io::stdin(), SetArg::TCSANOW, &self.original);
    }
}

#[cfg(not(unix))]
struct RawTerminalGuard;

#[cfg(not(unix))]
impl RawTerminalGuard {
    #[allow(
        clippy::unnecessary_wraps,
        reason = "the unsupported stub mirrors the fallible Unix terminal interface"
    )]
    fn enter(_enabled: bool) -> io::Result<Option<Self>> {
        Ok(None)
    }
}

struct InteractiveInput {
    receiver: tokio::sync::mpsc::Receiver<io::Result<Vec<u8>>>,
}

impl InteractiveInput {
    fn new() -> io::Result<Self> {
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        // Do not set O_NONBLOCK on the process stdin descriptor. A terminal
        // commonly supplies stdin, stdout, and stderr as duped descriptors for
        // one open file description, so changing stdin flags also changes the
        // output descriptors and turns transient terminal backpressure into an
        // EAGAIN failure. A detached reader keeps the async control loop
        // cancellable without mutating any host descriptor flags.
        std::thread::Builder::new()
            .name("morae-stdin".into())
            .spawn(move || {
                let mut stdin = io::stdin();
                loop {
                    let mut buffer = vec![0_u8; 16 * 1024];
                    let result = Read::read(&mut stdin, &mut buffer).map(|count| {
                        buffer.truncate(count);
                        buffer
                    });
                    let reached_eof = matches!(&result, Ok(bytes) if bytes.is_empty());
                    if sender.blocking_send(result).is_err() || reached_eof {
                        return;
                    }
                }
            })?;
        Ok(Self { receiver })
    }

    async fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let bytes = self.receiver.recv().await.ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "host stdin reader stopped")
        })??;
        if bytes.len() > buffer.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "host stdin chunk exceeds the interactive input buffer",
            ));
        }
        let count = bytes.len();
        buffer[..count].copy_from_slice(&bytes);
        Ok(count)
    }
}

#[cfg(unix)]
struct HostSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    window_change: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl HostSignals {
    fn new() -> io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};

        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
            window_change: signal(SignalKind::window_change())?,
        })
    }

    async fn recv(&mut self) -> io::Result<HostEvent> {
        loop {
            let event = tokio::select! {
                value = self.interrupt.recv() => {
                    signal_event(value, HostEvent::Signal(Signal::Interrupt))?
                }
                value = self.terminate.recv() => {
                    signal_event(value, HostEvent::Signal(Signal::Terminate))?
                }
                value = self.window_change.recv() => {
                    signal_event(value, HostEvent::Resize(0, 0))?
                }
            };
            if matches!(event, HostEvent::Resize(..)) {
                if let Some((rows, columns)) = terminal_window_size()? {
                    return Ok(HostEvent::Resize(rows, columns));
                }
            } else {
                return Ok(event);
            }
        }
    }
}

#[cfg(unix)]
fn signal_event(received: Option<()>, event: HostEvent) -> io::Result<HostEvent> {
    received
        .map(|()| event)
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "host signal stream closed"))
}

#[cfg(windows)]
struct HostSignals {
    interrupt: tokio::signal::windows::CtrlC,
}

#[cfg(windows)]
impl HostSignals {
    fn new() -> io::Result<Self> {
        Ok(Self {
            interrupt: tokio::signal::windows::ctrl_c()?,
        })
    }

    async fn recv(&mut self) -> io::Result<HostEvent> {
        self.interrupt
            .recv()
            .await
            .map(|()| HostEvent::Signal(Signal::Interrupt))
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "host signal stream closed"))
    }
}

#[cfg(not(any(unix, windows)))]
struct HostSignals;

#[cfg(not(any(unix, windows)))]
impl HostSignals {
    fn new() -> io::Result<Self> {
        Ok(Self)
    }

    async fn recv(&mut self) -> io::Result<HostEvent> {
        tokio::signal::ctrl_c().await?;
        Ok(HostEvent::Signal(Signal::Interrupt))
    }
}
