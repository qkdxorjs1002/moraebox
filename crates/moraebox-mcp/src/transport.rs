use super::{
    Arc, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter, HashMap,
    InflightRequests, JoinSet, MAX_CONCURRENT_REQUESTS, McpServer, RESPONSE_QUEUE_CAPACITY,
    SERVER_INSTRUCTIONS, SUPPORTED_PROTOCOL_VERSIONS, Semaphore, StdMutex, Value, io, json, mpsc,
    oneshot,
};
use crate::errors::McpServeError;
use crate::tools::{call_tool, protocol_error, success, tools_list};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConnectionState {
    #[default]
    Undecided,
    AwaitingInitialized,
    LegacyReady,
    Stateless,
}

impl ConnectionState {
    pub(super) fn accept_request(&mut self, request: &Value) -> Result<(), (i32, String)> {
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        match method {
            "initialize" => {
                initialize_protocol_version(request).map_err(|message| (-32602, message))?;
                if *self != Self::Undecided {
                    return Err((
                        -32600,
                        "initialize is only valid before selecting a protocol mode".into(),
                    ));
                }
                *self = Self::AwaitingInitialized;
            }
            "tools/list" | "tools/call" => match self {
                Self::Undecided => *self = Self::Stateless,
                Self::AwaitingInitialized => {
                    return Err((
                        -32002,
                        "server initialization is not complete; send notifications/initialized"
                            .into(),
                    ));
                }
                Self::LegacyReady | Self::Stateless => {}
            },
            _ => {}
        }
        Ok(())
    }

    pub(super) fn initialized(&mut self) {
        if *self == Self::AwaitingInitialized {
            *self = Self::LegacyReady;
        }
    }
}

pub(super) fn initialize_protocol_version(request: &Value) -> Result<&str, String> {
    let version = request
        .pointer("/params/protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| "initialize params.protocolVersion is required".to_owned())?;
    if SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
        Ok(version)
    } else {
        Err(format!(
            "unsupported protocol version {version}; supported versions: {}",
            SUPPORTED_PROTOCOL_VERSIONS.join(", ")
        ))
    }
}

pub(super) async fn serve(server: McpServer) -> Result<(), McpServeError> {
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let (responses, response_receiver) = mpsc::channel(RESPONSE_QUEUE_CAPACITY);
    let writer = tokio::spawn(write_responses(response_receiver));
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS));
    let inflight: InflightRequests = Arc::new(StdMutex::new(HashMap::new()));
    let mut requests = JoinSet::new();
    let mut connection_state = ConnectionState::default();
    let input_error = loop {
        let line = match input.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break None,
            Err(error) => break Some(error),
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                if responses
                    .send(protocol_error(Value::Null, -32700, &error.to_string()))
                    .await
                    .is_err()
                {
                    break Some(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "MCP response writer stopped",
                    ));
                }
                continue;
            }
        };
        let should_dispatch =
            match should_dispatch_request(&request, &mut connection_state, &inflight, &responses)
                .await
            {
                Ok(should_dispatch) => should_dispatch,
                Err(error) => break Some(error),
            };
        if !should_dispatch {
            continue;
        }
        if let Err(error) = dispatch_request(
            &server,
            request,
            &responses,
            &permits,
            &inflight,
            &mut requests,
        )
        .await
        {
            break Some(error);
        }
    };

    cancel_all_requests(&inflight);
    drop(responses);

    let mut request_error = None;
    while let Some(result) = requests.join_next().await {
        if let Err(error) = result
            && request_error.is_none()
        {
            request_error = Some(error);
        }
    }
    let cleanup_error = server.sdk.shutdown().await.err();
    let writer_result = writer.await.map_err(McpServeError::WriterTask)?;
    if let Some(error) = input_error {
        return Err(error.into());
    }
    if let Some(error) = request_error {
        return Err(McpServeError::RequestTask(error));
    }
    if let Some(error) = cleanup_error {
        return Err(McpServeError::SessionCleanup(error));
    }
    writer_result?;
    Ok(())
}

async fn should_dispatch_request(
    request: &Value,
    connection_state: &mut ConnectionState,
    inflight: &InflightRequests,
    responses: &mpsc::Sender<Value>,
) -> io::Result<bool> {
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    if method == "notifications/cancelled" {
        if let Some(request_id) = request.pointer("/params/requestId") {
            cancel_request(inflight, request_id);
        }
        return Ok(false);
    }
    if request.get("id").is_none() {
        if method == "notifications/initialized" {
            connection_state.initialized();
        }
        return Ok(false);
    }
    if let Err((code, message)) = connection_state.accept_request(request) {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        responses
            .send(protocol_error(id, code, &message))
            .await
            .map_err(|_| response_writer_stopped())?;
        return Ok(false);
    }
    Ok(true)
}

async fn dispatch_request(
    server: &McpServer,
    request: Value,
    responses: &mpsc::Sender<Value>,
    permits: &Arc<Semaphore>,
    inflight: &InflightRequests,
    requests: &mut JoinSet<()>,
) -> io::Result<()> {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let key = request_key(&id);
    let (cancel, cancellation) = oneshot::channel();
    if !register_request(inflight, &key, cancel) {
        responses
            .send(protocol_error(id, -32600, "duplicate in-flight request id"))
            .await
            .map_err(|_| response_writer_stopped())?;
        return Ok(());
    }
    let server = server.clone();
    let responses = responses.clone();
    let inflight = Arc::clone(inflight);
    let permits = Arc::clone(permits);
    requests.spawn(async move {
        let mut cancellation = cancellation;
        let permit = tokio::select! {
            permit = permits.acquire_owned() => {
                permit.expect("request semaphore remains open")
            }
            _ = &mut cancellation => {
                inflight
                    .lock()
                    .expect("in-flight request lock is not poisoned")
                    .remove(&key);
                let _ = responses
                    .send(protocol_error(id, -32800, "request cancelled"))
                    .await;
                return;
            }
        };
        let _permit = permit;
        let response = handle_cancellable_request(&server, request, cancellation).await;
        inflight
            .lock()
            .expect("in-flight request lock is not poisoned")
            .remove(&key);
        let _ = responses.send(response).await;
    });
    Ok(())
}

fn register_request(
    inflight: &InflightRequests,
    key: &str,
    cancellation: oneshot::Sender<()>,
) -> bool {
    let mut inflight = inflight
        .lock()
        .expect("in-flight request lock is not poisoned");
    if inflight.contains_key(key) {
        false
    } else {
        inflight.insert(key.to_owned(), Some(cancellation));
        true
    }
}

fn cancel_request(inflight: &InflightRequests, request_id: &Value) {
    let cancellation = inflight
        .lock()
        .expect("in-flight request lock is not poisoned")
        .get_mut(&request_key(request_id))
        .and_then(Option::take);
    if let Some(cancellation) = cancellation {
        let _ = cancellation.send(());
    }
}

fn cancel_all_requests(inflight: &InflightRequests) {
    let cancellations = std::mem::take(
        &mut *inflight
            .lock()
            .expect("in-flight request lock is not poisoned"),
    );
    for cancellation in cancellations.into_values().flatten() {
        let _ = cancellation.send(());
    }
}

fn response_writer_stopped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "MCP response writer stopped")
}

fn request_key(id: &Value) -> String {
    serde_json::to_string(id).expect("JSON-RPC request id is serializable")
}

async fn write_responses(mut responses: mpsc::Receiver<Value>) -> io::Result<()> {
    let mut output = BufWriter::new(tokio::io::stdout());
    while let Some(response) = responses.recv().await {
        write_response(&mut output, &response).await?;
    }
    Ok(())
}

async fn write_response<W>(output: &mut W, response: &Value) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut encoded = serde_json::to_vec(response).map_err(io::Error::other)?;
    encoded.push(b'\n');
    output.write_all(&encoded).await?;
    output.flush().await?;
    Ok(())
}

#[cfg(test)]
pub(super) async fn handle_request(server: &McpServer, request: Value) -> Value {
    handle_request_inner(server, request, None).await
}

async fn handle_cancellable_request(
    server: &McpServer,
    request: Value,
    cancellation: oneshot::Receiver<()>,
) -> Value {
    handle_request_inner(server, request, Some(cancellation)).await
}

async fn handle_request_inner(
    server: &McpServer,
    request: Value,
    cancellation: Option<oneshot::Receiver<()>>,
) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    match method {
        "initialize" => match initialize_protocol_version(&request) {
            Ok(version) => success(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "moraebox", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": SERVER_INSTRUCTIONS
                }),
            ),
            Err(message) => protocol_error(id, -32602, &message),
        },
        "ping" => success(id, json!({})),
        "tools/list" => success(id, tools_list()),
        "tools/call" => {
            call_tool(
                server,
                id,
                request.get("params").cloned().unwrap_or_default(),
                cancellation,
            )
            .await
        }
        _ => protocol_error(id, -32601, "method not found"),
    }
}
