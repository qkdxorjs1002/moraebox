use super::*;
use std::collections::{BTreeMap, BTreeSet};

pub(super) async fn call_tool(
    server: &McpServer,
    id: Value,
    params: Value,
    cancellation: Option<oneshot::Receiver<()>>,
) -> Value {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return protocol_error(id, -32602, "tool name is required");
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let result: Result<ToolOutput, ToolError> = match name {
        "sandbox_exec" => match sandbox_exec(&server.sdk, arguments, cancellation).await {
            Ok(value) => Ok(value),
            Err(ExecError::Cancelled) => {
                return protocol_error(id, -32800, "request cancelled");
            }
            Err(ExecError::Failed(error)) => Err(error),
        },
        "sandbox_io" => sandbox_io(&server.sdk, arguments)
            .await
            .map(ToolOutput::mirrored),
        "sandbox_session_list" => sandbox_session_list(&server.sdk, arguments)
            .await
            .map(ToolOutput::mirrored),
        "sandbox_session_status" => sandbox_session_status(&server.sdk, arguments)
            .await
            .map(ToolOutput::mirrored),
        "sandbox_stop" => sandbox_stop(&server.sdk, arguments)
            .await
            .map(ToolOutput::mirrored),
        "sandbox_remove" => sandbox_remove(&server.sdk, arguments)
            .await
            .map(ToolOutput::mirrored),
        "sandbox_box_create" => sandbox_box_create(server, arguments)
            .await
            .map(ToolOutput::mirrored),
        "sandbox_box_list" => sandbox_box_list(server, arguments)
            .await
            .map(ToolOutput::mirrored),
        "sandbox_box_get" => sandbox_box_get(server, arguments)
            .await
            .map(ToolOutput::mirrored),
        "sandbox_box_delete" => sandbox_box_delete(server, arguments)
            .await
            .map(ToolOutput::mirrored),
        "sandbox_box_reset" => sandbox_box_reset(server, arguments)
            .await
            .map(ToolOutput::mirrored),
        "sandbox_box_clone" => sandbox_box_clone(server, arguments)
            .await
            .map(ToolOutput::mirrored),
        "sandbox_box_update" => sandbox_box_update(server, arguments)
            .await
            .map(ToolOutput::mirrored),
        "sandbox_box_export" => sandbox_box_export(server, arguments)
            .await
            .map(ToolOutput::mirrored),
        "sandbox_box_import" => sandbox_box_import(server, arguments)
            .await
            .map(ToolOutput::mirrored),
        _ => return protocol_error(id, -32602, "unknown tool"),
    };
    match result {
        Ok(value) => success(id, tool_result(value, false)),
        Err(error) => success(id, tool_result(error.output(), true)),
    }
}

struct ToolOutput {
    structured_content: Value,
    content_text: String,
}

impl ToolOutput {
    fn mirrored(structured_content: Value) -> Self {
        let content_text = structured_content.to_string();
        Self {
            structured_content,
            content_text,
        }
    }

    fn summarized(structured_content: Value, content_text: String) -> Self {
        Self {
            structured_content,
            content_text,
        }
    }
}

#[derive(Debug)]
enum ExecError {
    Cancelled,
    Failed(ToolError),
}

impl From<SdkError> for ExecError {
    fn from(error: SdkError) -> Self {
        match error {
            SdkError::RequestCancelled => Self::Cancelled,
            error => Self::Failed(error.into()),
        }
    }
}

#[derive(Debug, Serialize)]
struct ToolError {
    code: &'static str,
    stage: String,
    retryable: bool,
    message: String,
    remediation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    earliest_cursor: Option<u64>,
}

impl ToolError {
    fn new(
        code: &'static str,
        stage: impl Into<String>,
        retryable: bool,
        message: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            code,
            stage: stage.into(),
            retryable,
            message: message.into(),
            remediation: remediation.into(),
            earliest_cursor: None,
        }
    }

    fn invalid_arguments(message: impl Into<String>) -> Self {
        Self::new(
            "invalid_arguments",
            "request_validation",
            false,
            message,
            "Correct the tool arguments and call the tool again.",
        )
    }

    fn internal(stage: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(
            "internal_error",
            stage,
            false,
            message,
            "Inspect the MCP server diagnostics and retry after correcting the server configuration.",
        )
    }

    fn output(self) -> ToolOutput {
        let content_text = format!(
            "{} during {}: {} Remediation: {}",
            self.code, self.stage, self.message, self.remediation
        );
        ToolOutput::summarized(json!({ "error": self }), content_text)
    }
}

impl From<OutputReadError> for ToolError {
    fn from(error: OutputReadError) -> Self {
        match error {
            OutputReadError::CursorExpired {
                requested,
                earliest,
            } => {
                let mut envelope = Self::new(
                    "cursor_expired",
                    "output_read",
                    true,
                    format!(
                        "output cursor {requested} expired; earliest retained cursor is {earliest}"
                    ),
                    format!("Retry sandbox_io with cursor={earliest}."),
                );
                envelope.earliest_cursor = Some(earliest);
                envelope
            }
            OutputReadError::CursorAhead { requested, next } => Self::new(
                "cursor_ahead",
                "output_read",
                true,
                format!("output cursor {requested} is ahead of next cursor {next}"),
                format!("Retry sandbox_io with cursor={next}, or wait for more output."),
            ),
        }
    }
}

impl From<BackendError> for ToolError {
    fn from(error: BackendError) -> Self {
        match error {
            BackendError::InvalidSpec(message) => Self::invalid_arguments(message),
            BackendError::Unsupported(capability) => Self::new(
                "unsupported_capability",
                "backend_capability",
                false,
                format!("backend does not support {capability}"),
                "Change the arguments or select a backend that supports this capability.",
            ),
            BackendError::Timeout { stage, limit } => Self::new(
                "timeout",
                stage.to_string(),
                true,
                format!("run timed out after {limit:?} during {stage}"),
                "Retry with a larger timeout, or reduce the work performed by the command.",
            ),
            BackendError::ImagePreparation(message) => Self::new(
                "image_prepare_failed",
                RunStage::ImagePull.to_string(),
                true,
                message,
                "Check the image reference, registry credentials, cache permissions, and network, then retry.",
            ),
            error => Self::new(
                "backend_failure",
                "backend",
                true,
                error.to_string(),
                "Check backend readiness and configuration, then retry the operation.",
            ),
        }
    }
}

impl From<SessionError> for ToolError {
    fn from(error: SessionError) -> Self {
        match error {
            SessionError::Backend(error) => error.into(),
            SessionError::Output(error) => error.into(),
            SessionError::Io(error) => Self::new(
                "session_io_failure",
                format!("{}_io", error.stream),
                false,
                error.to_string(),
                "Remove the failed session and start a new sandbox session.",
            ),
            SessionError::SessionClosed => Self::new(
                "session_closed",
                "session_io",
                false,
                "session is no longer available",
                "Start a new sandbox session and use its SessionId.",
            ),
            SessionError::StdinWriteTooLarge { requested, maximum } => Self::new(
                "stdin_too_large",
                "stdin_write",
                false,
                format!(
                    "stdin write is {requested} bytes, exceeding the {maximum}-byte queue limit"
                ),
                format!("Send stdin in chunks no larger than {maximum} bytes."),
            ),
            SessionError::OutputReadTooLarge { requested, maximum } => Self::new(
                "output_read_too_large",
                "output_read",
                false,
                format!(
                    "output read is {requested} bytes, exceeding the {maximum}-byte request limit"
                ),
                format!("Set max_bytes to at most {maximum}."),
            ),
            error => Self::new(
                "session_failure",
                "session_control",
                false,
                error.to_string(),
                "Remove the failed session and start a new sandbox session.",
            ),
        }
    }
}

impl From<BoxStoreError> for ToolError {
    fn from(error: BoxStoreError) -> Self {
        let message = error.to_string();
        match error {
            BoxStoreError::NotFound(_) => Self::new(
                "box_not_found",
                "box_lookup",
                false,
                message,
                "Use sandbox_box_list to select an existing BoxId.",
            ),
            BoxStoreError::Busy { .. } | BoxStoreError::BaseDiskBusy { .. } => Self::new(
                "box_busy",
                "box_lock",
                true,
                message,
                "Wait for the current Box operation to finish, then retry.",
            ),
            BoxStoreError::NeedsRepair(_) => Self::new(
                "box_needs_repair",
                "box_repair",
                false,
                message,
                "Reset or recreate the Box before running it again.",
            ),
            BoxStoreError::Io(_) => Self::new(
                "box_io_failure",
                "box_storage",
                true,
                message,
                "Check storage availability and permissions, then retry.",
            ),
            error => Self::new(
                "box_failure",
                "box_storage",
                false,
                error.to_string(),
                "Inspect the Box metadata and storage, then reset or recreate the Box.",
            ),
        }
    }
}

impl From<SdkError> for ToolError {
    fn from(error: SdkError) -> Self {
        match error {
            SdkError::Session(error) => error.into(),
            SdkError::UnknownSession(session_id) => Self::new(
                "session_not_found",
                "session_lookup",
                false,
                format!("unknown sandbox session {session_id}"),
                "Start a new sandbox session and use its SessionId.",
            ),
            SdkError::SessionLimitExceeded { maximum } => Self::new(
                "session_limit_exceeded",
                "session_start",
                true,
                format!("active sandbox session limit reached (maximum {maximum})"),
                "Stop or remove an existing session, then retry.",
            ),
            SdkError::OutputReadTooLarge { requested, maximum } => Self::new(
                "output_read_too_large",
                "output_read",
                false,
                format!("output read is {requested} bytes, exceeding the {maximum}-byte SDK limit"),
                format!("Set max_bytes to at most {maximum}."),
            ),
            SdkError::OutputReadEmpty => {
                Self::invalid_arguments("output read must request at least one byte")
            }
            SdkError::RequestCancelled => Self::new(
                "request_cancelled",
                "request",
                true,
                "sandbox request was cancelled",
                "Call the tool again if the operation is still required.",
            ),
            SdkError::BoxStore(error) => error.into(),
            SdkError::BoxStoreNotConfigured => Self::new(
                "box_store_unavailable",
                "box_storage",
                false,
                "Box store is not configured for this SDK instance",
                "Configure the MCP server state directory before using Box tools.",
            ),
            error => Self::internal("sdk", error.to_string()),
        }
    }
}

async fn sandbox_exec(
    sdk: &SandboxSdk,
    arguments: Value,
    cancellation: Option<oneshot::Receiver<()>>,
) -> Result<ToolOutput, ExecError> {
    sandbox_exec_with_inline_limit(
        sdk,
        arguments,
        cancellation,
        SANDBOX_EXEC_INLINE_OUTPUT_BYTES,
    )
    .await
}

async fn sandbox_exec_with_inline_limit(
    sdk: &SandboxSdk,
    arguments: Value,
    cancellation: Option<oneshot::Receiver<()>>,
    inline_output_bytes: usize,
) -> Result<ToolOutput, ExecError> {
    let args: ExecArgs = serde_json::from_value(arguments)
        .map_err(|error| ExecError::Failed(ToolError::invalid_arguments(error.to_string())))?;
    if args.argv.is_empty() {
        return Err(ExecError::Failed(ToolError::invalid_arguments(
            "argv must contain an executable",
        )));
    }
    if args.copy_in.len() > 64 || args.copy_out.len() > 64 {
        return Err(ExecError::Failed(ToolError::invalid_arguments(
            "copy_in and copy_out each support at most 64 entries",
        )));
    }
    let mut spec = RunSpec::command(args.argv);
    spec.box_id = args.box_id;
    spec.image_pull_policy = args.pull_policy;
    spec.tty = args.tty;
    spec.network = args.network;
    spec.workspace_mode = args.workspace_mode;
    spec.copy_in = args.copy_in;
    spec.copy_out = args.copy_out;
    spec.copy_limit_bytes = args.copy_limit_bytes;
    if let Some(output_limit) = args.output_limit_bytes {
        if !(1..=MAX_OUTPUT_LIMIT).contains(&output_limit) {
            return Err(ExecError::Failed(ToolError::invalid_arguments(format!(
                "output_limit_bytes must be between 1 and {MAX_OUTPUT_LIMIT}"
            ))));
        }
        spec.output_limit = output_limit;
    }
    if let Some(kill_grace_ms) = args.kill_grace_ms {
        let kill_grace = Duration::from_millis(kill_grace_ms);
        if kill_grace.is_zero() || kill_grace > MAX_KILL_GRACE {
            return Err(ExecError::Failed(ToolError::invalid_arguments(
                "kill_grace_ms must be between 1 and 60000",
            )));
        }
        spec.kill_grace = kill_grace;
    }
    spec.timeout = match (args.unlimited, args.timeout_ms) {
        (true, Some(_)) => {
            return Err(ExecError::Failed(ToolError::invalid_arguments(
                "unlimited=true cannot be combined with timeout_ms",
            )));
        }
        (true, None) => TimeoutPolicy::Unlimited,
        (false, Some(milliseconds)) if milliseconds > 0 => TimeoutPolicy::Limited(milliseconds),
        (false, Some(_)) => {
            return Err(ExecError::Failed(ToolError::invalid_arguments(
                "timeout_ms must be greater than zero",
            )));
        }
        (false, None) => TimeoutPolicy::default(),
    };
    spec.stdin = decode_bounded_stdin(args.stdin_base64)
        .map_err(ExecError::Failed)?
        .unwrap_or_default();
    spec.validate()
        .map_err(|error| ExecError::Failed(ToolError::invalid_arguments(error)))?;
    if args.wait {
        let result = match cancellation {
            Some(cancellation) => {
                sdk.exec_retained_cancellable(spec, inline_output_bytes, cancellation)
                    .await
            }
            None => sdk.exec_retained(spec, inline_output_bytes).await,
        }?;
        let has_more = result.next_cursor < result.output_next_cursor;
        let structured_content = execution_page_json(&result);
        let content_text = execution_content_summary(&result, has_more);
        if !has_more {
            sdk.remove(result.status.session_id).await?;
        }
        Ok(ToolOutput::summarized(structured_content, content_text))
    } else {
        let status = match cancellation {
            Some(cancellation) => sdk.start_cancellable(spec, cancellation).await,
            None => sdk.start(spec).await,
        }?;
        Ok(ToolOutput::mirrored(
            json!({ "status": status, "next_cursor": 0 }),
        ))
    }
}

async fn sandbox_io(sdk: &SandboxSdk, arguments: Value) -> Result<Value, ToolError> {
    let args: IoArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    validate_io_args(&args)?;
    let session_id = SessionId::from_str(&args.session_id)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    let stdin = decode_bounded_stdin(args.stdin_base64)?;
    let resize = args.rows.zip(args.columns);
    let signal = args.signal.as_deref().map(parse_signal).transpose()?;
    let result = sdk
        .io(
            session_id,
            IoRequest {
                cursor: args.cursor,
                max_bytes: args.max_bytes,
                stdin,
                close_stdin: args.close_stdin,
                resize,
                signal,
                wait_timeout: (args.wait_ms > 0).then(|| Duration::from_millis(args.wait_ms)),
            },
        )
        .await?;
    Ok(io_json(&result))
}

fn validate_io_args(args: &IoArgs) -> Result<(), ToolError> {
    if !(1..=MAX_IO_OUTPUT_READ_BYTES).contains(&args.max_bytes) {
        return Err(ToolError::invalid_arguments(format!(
            "max_bytes must be between 1 and {MAX_IO_OUTPUT_READ_BYTES}"
        )));
    }
    if args.wait_ms > MAX_MCP_WAIT_MS {
        return Err(ToolError::invalid_arguments(format!(
            "wait_ms must be between 0 and {MAX_MCP_WAIT_MS}"
        )));
    }
    match (args.rows, args.columns) {
        (None, None) => Ok(()),
        (Some(rows), Some(columns)) if rows > 0 && columns > 0 => Ok(()),
        (Some(_), Some(_)) => Err(ToolError::invalid_arguments(
            "rows and columns must be greater than zero",
        )),
        _ => Err(ToolError::invalid_arguments(
            "rows and columns must be provided together",
        )),
    }
}

async fn sandbox_session_list(sdk: &SandboxSdk, arguments: Value) -> Result<Value, ToolError> {
    let _: EmptyArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    Ok(json!({ "sessions": sdk.list_sessions().await }))
}

async fn sandbox_session_status(sdk: &SandboxSdk, arguments: Value) -> Result<Value, ToolError> {
    let args: StopArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    let session_id = SessionId::from_str(&args.session_id)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    Ok(json!({ "status": sdk.status(session_id).await? }))
}

fn decode_bounded_stdin(input: Option<String>) -> Result<Option<Vec<u8>>, ToolError> {
    let Some(input) = input else {
        return Ok(None);
    };
    if input.len() > MAX_MCP_STDIN_BASE64_CHARS {
        return Err(ToolError::invalid_arguments(format!(
            "stdin_base64 exceeds the {MAX_MCP_STDIN_BYTES}-byte decoded input limit"
        )));
    }
    let decoded = STANDARD
        .decode(input)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    if decoded.len() > MAX_MCP_STDIN_BYTES {
        return Err(ToolError::invalid_arguments(format!(
            "stdin_base64 decodes to {} bytes, exceeding the {MAX_MCP_STDIN_BYTES}-byte limit",
            decoded.len()
        )));
    }
    Ok(Some(decoded))
}

async fn sandbox_stop(sdk: &SandboxSdk, arguments: Value) -> Result<Value, ToolError> {
    let args: StopArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    let session_id = SessionId::from_str(&args.session_id)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    let status = sdk.stop(session_id).await?;
    Ok(json!({ "status": status }))
}

async fn sandbox_remove(sdk: &SandboxSdk, arguments: Value) -> Result<Value, ToolError> {
    let args: StopArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    let session_id = SessionId::from_str(&args.session_id)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    let status = sdk.remove(session_id).await?;
    match status {
        Some(status) => Ok(json!({ "removed": true, "status": status })),
        None => Ok(json!({ "removed": false })),
    }
}

async fn sandbox_box_create(server: &McpServer, arguments: Value) -> Result<Value, ToolError> {
    let args: BoxCreateArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    let reference = args
        .image
        .map_or_else(|| server.boxes.images.default_reference(), Ok)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    let prepared = server
        .boxes
        .images
        .prepare(
            &reference,
            &server.boxes.platform,
            server.boxes.credentials.clone(),
            args.pull_policy,
        )
        .await
        .map_err(|error| {
            ToolError::new(
                "image_prepare_failed",
                "image_pull",
                true,
                error.to_string(),
                "Check the image reference, registry credentials, and network, then retry.",
            )
        })?;
    let disk_size = args
        .disk_size_bytes
        .unwrap_or(server.boxes.default_disk_size);
    if disk_size == 0 {
        return Err(ToolError::invalid_arguments(
            "disk_size_bytes must be greater than zero",
        ));
    }
    let base_spec = BaseDiskSpec::new(
        prepared.manifest_digest.clone(),
        platform_name(&server.boxes.platform),
        disk_size,
    );
    let base_disks = server.boxes.base_disks.clone();
    let rootfs = prepared.rootfs;
    let mke2fs = server.boxes.mke2fs_path.clone();
    let base =
        tokio::task::spawn_blocking(move || base_disks.prepare(&base_spec, &rootfs, &mke2fs))
            .await
            .map_err(|error| {
                ToolError::internal(
                    "base_disk_prepare",
                    format!("base disk task failed: {error}"),
                )
            })??;
    let request = CreateBox::new(
        prepared.manifest_digest,
        platform_name(&server.boxes.platform),
        disk_size,
    )
    .with_labels(args.labels)
    .with_tags(args.tags);
    let request = if let Some(name) = args.name {
        request.with_name(name)
    } else {
        request
    };
    let metadata = server
        .sdk
        .create_box(request, base.disk_path().to_path_buf())
        .await?;
    serde_json::to_value(metadata)
        .map_err(|error| ToolError::internal("response_serialization", error.to_string()))
}

async fn sandbox_box_list(server: &McpServer, arguments: Value) -> Result<Value, ToolError> {
    let args: BoxListArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    let report = server
        .sdk
        .list_boxes_with(BoxQuery {
            name: args.name,
            labels: args.labels,
            tags: args.tags,
            state: args.state,
            sort_by: args.sort_by,
            descending: args.descending,
        })
        .await?;
    serde_json::to_value(report)
        .map_err(|error| ToolError::internal("response_serialization", error.to_string()))
}

async fn sandbox_box_get(server: &McpServer, arguments: Value) -> Result<Value, ToolError> {
    let args: BoxIdArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    let metadata = server.sdk.get_box(args.box_id).await?;
    serde_json::to_value(metadata)
        .map_err(|error| ToolError::internal("response_serialization", error.to_string()))
}

async fn sandbox_box_delete(server: &McpServer, arguments: Value) -> Result<Value, ToolError> {
    let args: ConfirmedBoxArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    require_confirmation(args.confirm)?;
    let metadata = server.sdk.delete_box(args.box_id).await?;
    Ok(json!({ "deleted": metadata.box_id }))
}

async fn sandbox_box_reset(server: &McpServer, arguments: Value) -> Result<Value, ToolError> {
    let args: ConfirmedBoxArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    require_confirmation(args.confirm)?;
    let current = server.sdk.get_box(args.box_id).await?;
    let spec = BaseDiskSpec::new(
        current.manifest_digest,
        current.platform,
        current.virtual_size_bytes,
    );
    let base_disks = server.boxes.base_disks.clone();
    let base = tokio::task::spawn_blocking(move || base_disks.get(&spec))
        .await
        .map_err(|error| {
            ToolError::internal("base_disk_lookup", format!("base disk task failed: {error}"))
        })??
        .ok_or_else(|| {
            ToolError::new(
                "base_disk_not_found",
                "base_disk_lookup",
                false,
                format!(
                "the immutable base disk for Box {} is not cached; recreate the image-backed Box instead",
                args.box_id
                ),
                "Recreate the image-backed Box to restore its immutable base disk.",
            )
        })?;
    let metadata = server
        .sdk
        .reset_box(args.box_id, base.disk_path().to_path_buf())
        .await?;
    serde_json::to_value(metadata)
        .map_err(|error| ToolError::internal("response_serialization", error.to_string()))
}

async fn sandbox_box_clone(server: &McpServer, arguments: Value) -> Result<Value, ToolError> {
    let args: ConfirmedBoxArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    require_confirmation(args.confirm)?;
    let metadata = server.sdk.clone_box(args.box_id).await?;
    serde_json::to_value(metadata)
        .map_err(|error| ToolError::internal("response_serialization", error.to_string()))
}

async fn sandbox_box_update(server: &McpServer, arguments: Value) -> Result<Value, ToolError> {
    let args: BoxUpdateArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    let metadata = server
        .sdk
        .update_box(
            args.box_id,
            UpdateBox {
                name: args.name,
                clear_name: args.clear_name,
                set_labels: args.set_labels,
                remove_labels: args.remove_labels,
                add_tags: args.add_tags,
                remove_tags: args.remove_tags,
            },
        )
        .await?;
    serde_json::to_value(metadata)
        .map_err(|error| ToolError::internal("response_serialization", error.to_string()))
}

async fn sandbox_box_export(server: &McpServer, arguments: Value) -> Result<Value, ToolError> {
    let args: BoxExportArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    let report = server.sdk.export_box(args.box_id, args.destination).await?;
    serde_json::to_value(report)
        .map_err(|error| ToolError::internal("response_serialization", error.to_string()))
}

async fn sandbox_box_import(server: &McpServer, arguments: Value) -> Result<Value, ToolError> {
    let args: BoxImportArgs = serde_json::from_value(arguments)
        .map_err(|error| ToolError::invalid_arguments(error.to_string()))?;
    let metadata = server.sdk.import_box(args.source).await?;
    serde_json::to_value(metadata)
        .map_err(|error| ToolError::internal("response_serialization", error.to_string()))
}

fn require_confirmation(confirmed: bool) -> Result<(), ToolError> {
    if confirmed {
        Ok(())
    } else {
        Err(ToolError::invalid_arguments(
            "confirm must be true for this Box operation",
        ))
    }
}

fn execution_page_json(result: &ExecutionPageResult) -> Value {
    let has_more = result.next_cursor < result.output_next_cursor;
    json!({
        "status": result.status,
        "output": chunks_json(&result.output),
        "next_cursor": result.next_cursor,
        "output_next_cursor": result.output_next_cursor,
        "has_more": has_more,
        "continuation_cursor": has_more.then_some(result.next_cursor),
        "truncated": result.truncated
    })
}

fn execution_content_summary(result: &ExecutionPageResult, has_more: bool) -> String {
    let inline_bytes = result
        .output
        .iter()
        .map(|chunk| chunk.data.len())
        .sum::<usize>();
    if has_more {
        format!(
            "sandbox_exec completed; {inline_bytes} output bytes are in structuredContent; continue session {} from cursor {} with sandbox_io",
            result.status.session_id, result.next_cursor
        )
    } else {
        format!(
            "sandbox_exec completed; {inline_bytes} output bytes are in structuredContent; no continuation is required"
        )
    }
}

fn io_json(result: &IoResult) -> Value {
    json!({
        "status": result.status,
        "output": chunks_json(&result.output),
        "next_cursor": result.next_cursor,
        "truncated": result.truncated,
        "wait_timed_out": result.wait_timed_out
    })
}

fn chunks_json(chunks: &[OutputChunk]) -> Vec<Value> {
    chunks
        .iter()
        .map(|chunk| {
            json!({
                "cursor": chunk.cursor,
                "channel": chunk.channel,
                "text": String::from_utf8_lossy(&chunk.data)
            })
        })
        .collect()
}

fn parse_signal(value: &str) -> Result<Signal, ToolError> {
    match value.to_ascii_uppercase().as_str() {
        "INT" | "SIGINT" => Ok(Signal::Interrupt),
        "TERM" | "SIGTERM" => Ok(Signal::Terminate),
        "KILL" | "SIGKILL" => Ok(Signal::Kill),
        "HUP" | "SIGHUP" => Ok(Signal::Hangup),
        _ => Err(ToolError::invalid_arguments(format!(
            "unsupported signal {value}"
        ))),
    }
}

fn tool_result(output: ToolOutput, is_error: bool) -> Value {
    let mut content_item = serde_json::Map::with_capacity(2);
    content_item.insert("type".into(), Value::String("text".into()));
    content_item.insert("text".into(), Value::String(output.content_text));
    let mut result = serde_json::Map::with_capacity(3);
    result.insert(
        "content".into(),
        Value::Array(vec![Value::Object(content_item)]),
    );
    result.insert("structuredContent".into(), output.structured_content);
    result.insert("isError".into(), Value::Bool(is_error));
    Value::Object(result)
}

pub(super) fn success(id: Value, result: Value) -> Value {
    let response = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    drop((id, result));
    response
}

pub(super) fn protocol_error(id: Value, code: i32, message: &str) -> Value {
    let response =
        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } });
    drop(id);
    response
}

fn session_status_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session_id": { "type": "string", "format": "uuid" },
            "backend": { "type": "string" },
            "resolved_image_digest": {
                "anyOf": [{ "type": "string" }, { "type": "null" }],
                "description": "Actual materialized OCI manifest digest for image-backed sessions."
            },
            "state": {
                "type": "string",
                "enum": ["new", "preparing", "starting", "ready", "running", "stopping", "failed", "timed_out", "dead"]
            },
            "termination_reason": {
                "anyOf": [
                    { "type": "string", "enum": ["exited", "cancelled", "timed_out", "failed"] },
                    { "type": "null" }
                ]
            },
            "exit_code": { "anyOf": [{ "type": "integer" }, { "type": "null" }] },
            "signal": { "anyOf": [{ "type": "integer" }, { "type": "null" }] },
            "timed_out": { "type": "boolean" },
            "elapsed_micros": { "type": "integer", "minimum": 0 }
        },
        "required": [
            "session_id", "backend", "resolved_image_digest", "state", "termination_reason",
            "exit_code", "signal", "timed_out", "elapsed_micros"
        ],
        "additionalProperties": false
    })
}

fn output_chunks_schema() -> Value {
    json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": {
                "cursor": { "type": "integer", "minimum": 0 },
                "channel": { "type": "string", "enum": ["stdout", "stderr"] },
                "text": { "type": "string" }
            },
            "required": ["cursor", "channel", "text"],
            "additionalProperties": false
        }
    })
}

fn execution_output_schema() -> Value {
    json!({
        "oneOf": [
            {
                "type": "object",
                "properties": {
                    "status": session_status_schema(),
                    "output": output_chunks_schema(),
                    "next_cursor": { "type": "integer", "minimum": 0 },
                    "output_next_cursor": { "type": "integer", "minimum": 0 },
                    "has_more": { "type": "boolean" },
                    "continuation_cursor": {
                        "anyOf": [{ "type": "integer", "minimum": 0 }, { "type": "null" }]
                    },
                    "truncated": { "type": "boolean" }
                },
                "required": [
                    "status", "output", "next_cursor", "output_next_cursor", "has_more",
                    "continuation_cursor", "truncated"
                ],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "status": session_status_schema(),
                    "next_cursor": { "type": "integer", "minimum": 0 }
                },
                "required": ["status", "next_cursor"],
                "additionalProperties": false
            }
        ]
    })
}

fn io_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "status": session_status_schema(),
            "output": output_chunks_schema(),
            "next_cursor": { "type": "integer", "minimum": 0 },
            "truncated": { "type": "boolean" },
            "wait_timed_out": { "type": "boolean" }
        },
        "required": ["status", "output", "next_cursor", "truncated", "wait_timed_out"],
        "additionalProperties": false
    })
}

fn status_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "status": session_status_schema() },
        "required": ["status"],
        "additionalProperties": false
    })
}

fn session_list_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "sessions": { "type": "array", "items": session_status_schema() }
        },
        "required": ["sessions"],
        "additionalProperties": false
    })
}

fn remove_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "removed": { "type": "boolean" },
            "status": session_status_schema()
        },
        "required": ["removed"],
        "additionalProperties": false
    })
}

fn box_metadata_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "schema_version": { "type": "integer", "minimum": 1 },
            "box_id": { "type": "string", "format": "uuid" },
            "state": { "type": "string", "enum": ["ready", "dirty", "needs_repair"] },
            "manifest_digest": { "type": "string" },
            "platform": { "type": "string" },
            "disk_format": { "type": "string", "enum": ["raw_ext4"] },
            "virtual_size_bytes": { "type": "integer", "minimum": 1 },
            "generation": { "type": "integer", "minimum": 0 },
            "created_at_unix_ms": { "type": "integer", "minimum": 0 },
            "updated_at_unix_ms": { "type": "integer", "minimum": 0 },
            "owner_uid": { "anyOf": [{ "type": "integer", "minimum": 0 }, { "type": "null" }] },
            "name": { "anyOf": [{ "type": "string" }, { "type": "null" }] },
            "labels": { "type": "object", "additionalProperties": { "type": "string" } },
            "tags": { "type": "array", "items": { "type": "string" }, "uniqueItems": true },
            "last_used_at_unix_ms": { "anyOf": [{ "type": "integer", "minimum": 0 }, { "type": "null" }] },
            "physical_size_bytes": { "type": "integer", "minimum": 0 }
        },
        "required": [
            "schema_version", "box_id", "state", "manifest_digest", "platform", "disk_format",
            "virtual_size_bytes", "generation", "created_at_unix_ms", "updated_at_unix_ms",
            "owner_uid", "name", "labels", "tags", "last_used_at_unix_ms",
            "physical_size_bytes"
        ],
        "additionalProperties": false
    })
}

fn box_list_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "boxes": { "type": "array", "items": box_metadata_schema() },
            "errors": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "entry_name": { "type": "string" },
                        "box_id": {
                            "anyOf": [
                                { "type": "string", "format": "uuid" },
                                { "type": "null" }
                            ]
                        },
                        "code": {
                            "type": "string",
                            "enum": [
                                "invalid_name", "invalid_metadata", "unsupported_schema",
                                "unsafe_file_type", "missing_data", "busy", "io", "corrupt_store"
                            ]
                        },
                        "message": { "type": "string" }
                    },
                    "required": ["entry_name", "box_id", "code", "message"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["boxes", "errors"],
        "additionalProperties": false
    })
}

fn deleted_box_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "deleted": { "type": "string", "format": "uuid" } },
        "required": ["deleted"],
        "additionalProperties": false
    })
}

fn box_bundle_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "box_id": { "type": "string", "format": "uuid" },
            "path": { "type": "string" },
            "size_bytes": { "type": "integer", "minimum": 1 },
            "sha256": { "type": "string", "pattern": "^sha256:[0-9a-f]{64}$" }
        },
        "required": ["box_id", "path", "size_bytes", "sha256"],
        "additionalProperties": false
    })
}

fn error_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "error": {
                "type": "object",
                "properties": {
                    "code": { "type": "string" },
                    "stage": { "type": "string" },
                    "retryable": { "type": "boolean" },
                    "message": { "type": "string" },
                    "remediation": { "type": "string" },
                    "earliest_cursor": { "type": "integer", "minimum": 0 }
                },
                "required": ["code", "stage", "retryable", "message", "remediation"],
                "additionalProperties": false
            }
        },
        "required": ["error"],
        "additionalProperties": false
    })
}

fn tool_output_schema(success_schema: Value) -> Value {
    let mut schema = serde_json::Map::with_capacity(1);
    schema.insert(
        "oneOf".into(),
        Value::Array(vec![success_schema, error_output_schema()]),
    );
    Value::Object(schema)
}

#[allow(clippy::too_many_lines)]
pub(super) fn tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "sandbox_exec",
                "title": "Execute in configured runtime",
                "description": "Start a command in the configured runtime. Every call receives a new SessionId and, with libkrun, a new microVM. Image-backed runs prepare their image lazily within timeout_ms; pull_policy=missing uses the cache first, always refreshes from the registry, and never permits only an existing cache entry. Failures use code image_prepare_failed and stage image_pull, and status.resolved_image_digest reports the actual materialized manifest. Pass box_id only when the command should reuse that Box's persistent root filesystem. Prefer this for untrusted code, dependency installation, isolated experiments, reproducible Linux checks, or long-running sessions. Network access is disabled by default; set network=true to opt in for one native VM run. output_limit_bytes bounds retained output and kill_grace_ms bounds TERM-to-force cleanup; defaults remain 64 MiB and 5000 ms. At most 32 sessions run concurrently. wait=true returns at most 1 MiB inline; when has_more is true, call sandbox_io with the returned SessionId and continuation_cursor within five minutes. Cancelled wait=true runs are cleaned. wait=false starts a session owned by this stdio connection until sandbox_remove or disconnect; completed status and output expire after five minutes. Output chunks contain UTF-8 text; invalid byte sequences are replaced with U+FFFD.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "argv": { "type": "array", "items": { "type": "string" }, "minItems": 1 },
                        "stdin_base64": { "type": "string", "maxLength": MAX_MCP_STDIN_BASE64_CHARS },
                        "box_id": { "type": "string", "format": "uuid", "description": "Persistent Box root filesystem to reuse; the microVM and SessionId remain new." },
                        "pull_policy": { "type": "string", "enum": ["missing", "always", "never"], "default": "missing", "description": "Image acquisition policy for image-backed sessions." },
                        "timeout_ms": { "type": "integer", "minimum": 1 },
                        "unlimited": { "type": "boolean", "default": false },
                        "output_limit_bytes": { "type": "integer", "minimum": 1, "maximum": MAX_OUTPUT_LIMIT, "default": DEFAULT_OUTPUT_LIMIT },
                        "kill_grace_ms": { "type": "integer", "minimum": 1, "maximum": 60_000, "default": DEFAULT_KILL_GRACE.as_millis() },
                        "network": { "type": "boolean", "default": false },
                        "tty": { "type": "boolean", "default": false },
                        "workspace_mode": { "type": "string", "enum": ["read_only", "overlay"], "default": "read_only", "description": "Use overlay only when the MCP server was configured with a workspace disk." },
                        "copy_in": { "type": "array", "maxItems": 64, "items": { "type": "object", "properties": { "source": { "type": "string", "description": "Host file or directory." }, "destination": { "type": "string", "description": "Normalized absolute guest destination." } }, "required": ["source", "destination"], "additionalProperties": false }, "default": [] },
                        "copy_out": { "type": "array", "maxItems": 64, "items": { "type": "object", "properties": { "source": { "type": "string", "description": "Normalized absolute guest source." }, "destination": { "type": "string", "description": "Absolute create-new host destination." } }, "required": ["source", "destination"], "additionalProperties": false }, "default": [] },
                        "copy_limit_bytes": { "type": "integer", "minimum": 1, "maximum": MAX_COPY_LIMIT, "default": DEFAULT_COPY_LIMIT },
                        "wait": { "type": "boolean", "default": true }
                    },
                    "required": ["argv"],
                    "additionalProperties": false
                },
                "outputSchema": tool_output_schema(execution_output_schema()),
                "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": true }
            },
            {
                "name": "sandbox_io",
                "title": "Sandbox session I/O",
                "description": "Write stdin, close it, signal or resize, and read bounded UTF-8 text output from a cursor. Set wait_ms up to 30000 to long-poll until output arrives, the session ends, or the wait expires; wait_timed_out distinguishes expiry. Invalid byte sequences are replaced with U+FFFD.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": { "type": "string" },
                        "cursor": { "type": "integer", "minimum": 0, "default": 0 },
                        "max_bytes": { "type": "integer", "minimum": 1, "maximum": MAX_IO_OUTPUT_READ_BYTES, "default": 1_048_576 },
                        "wait_ms": { "type": "integer", "minimum": 0, "maximum": MAX_MCP_WAIT_MS, "default": 0 },
                        "stdin_base64": { "type": "string", "maxLength": MAX_MCP_STDIN_BASE64_CHARS },
                        "close_stdin": { "type": "boolean", "default": false },
                        "rows": { "type": "integer", "minimum": 1, "maximum": 65_535 },
                        "columns": { "type": "integer", "minimum": 1, "maximum": 65_535 },
                        "signal": { "type": "string", "enum": ["INT", "TERM", "KILL", "HUP"] }
                    },
                    "required": ["session_id"],
                    "additionalProperties": false
                },
                "outputSchema": tool_output_schema(io_output_schema()),
                "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false }
            },
            {
                "name": "sandbox_session_list",
                "title": "List sandbox sessions",
                "description": "List current connection-owned sessions in stable SessionId order without waiting or starting a sandbox.",
                "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
                "outputSchema": tool_output_schema(session_list_output_schema()),
                "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
            },
            {
                "name": "sandbox_session_status",
                "title": "Get sandbox session status",
                "description": "Read the current status of one retained session without waiting for it to finish.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "session_id": { "type": "string", "format": "uuid" } },
                    "required": ["session_id"],
                    "additionalProperties": false
                },
                "outputSchema": tool_output_schema(status_output_schema()),
                "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
            },
            {
                "name": "sandbox_stop",
                "title": "Stop sandbox",
                "description": "Terminate a running sandbox with the configured grace period.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "session_id": { "type": "string" } },
                    "required": ["session_id"],
                    "additionalProperties": false
                },
                "outputSchema": tool_output_schema(status_output_schema()),
                "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": false }
            },
            {
                "name": "sandbox_remove",
                "title": "Remove sandbox session",
                "description": "Stop a running session if needed, wait for cleanup, and immediately remove its retained status and output. Repeating the call returns removed=false.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "session_id": { "type": "string" } },
                    "required": ["session_id"],
                    "additionalProperties": false
                },
                "outputSchema": tool_output_schema(remove_output_schema()),
                "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": false }
            },
            {
                "name": "sandbox_box_create",
                "title": "Create persistent Box",
                "description": "Create an independent persistent root filesystem from an OCI image. pull_policy=missing uses the cache first, always refreshes from the registry, and never permits only an existing cache entry. The returned manifest_digest is the actual materialized manifest.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "image": { "type": "string", "description": "OCI image reference; uses the configured default when omitted." },
                        "pull_policy": { "type": "string", "enum": ["missing", "always", "never"], "default": "missing", "description": "Image acquisition policy." },
                        "disk_size_bytes": { "type": "integer", "minimum": 1, "description": "Virtual root disk size; uses the server default when omitted." },
                        "name": { "type": "string", "description": "Optional unique display name." },
                        "labels": { "type": "object", "additionalProperties": { "type": "string" } },
                        "tags": { "type": "array", "items": { "type": "string" }, "uniqueItems": true }
                    },
                    "additionalProperties": false
                },
                "outputSchema": tool_output_schema(box_metadata_schema()),
                "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": true }
            },
            {
                "name": "sandbox_box_list",
                "title": "List persistent Boxes",
                "description": "Filter and stably sort persistent Box metadata without starting a microVM.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "labels": {
                            "type": "object",
                            "additionalProperties": { "anyOf": [{ "type": "string" }, { "type": "null" }] }
                        },
                        "tags": { "type": "array", "items": { "type": "string" }, "uniqueItems": true },
                        "state": { "type": "string", "enum": ["ready", "dirty", "needs_repair"] },
                        "sort_by": { "type": "string", "enum": ["id", "name", "created", "updated", "last_used", "physical_size", "virtual_size"], "default": "id" },
                        "descending": { "type": "boolean", "default": false }
                    },
                    "additionalProperties": false
                },
                "outputSchema": tool_output_schema(box_list_output_schema()),
                "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
            },
            {
                "name": "sandbox_box_get",
                "title": "Get persistent Box",
                "description": "Read metadata for one persistent Box.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "box_id": { "type": "string", "format": "uuid" } },
                    "required": ["box_id"],
                    "additionalProperties": false
                },
                "outputSchema": tool_output_schema(box_metadata_schema()),
                "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
            },
            {
                "name": "sandbox_box_delete",
                "title": "Delete persistent Box",
                "description": "Permanently delete one idle Box and its full root filesystem. confirm must be true.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "box_id": { "type": "string", "format": "uuid" },
                        "confirm": { "type": "boolean", "description": "Explicitly authorize permanent deletion." }
                    },
                    "required": ["box_id", "confirm"],
                    "additionalProperties": false
                },
                "outputSchema": tool_output_schema(deleted_box_output_schema()),
                "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false }
            },
            {
                "name": "sandbox_box_reset",
                "title": "Reset persistent Box",
                "description": "Replace every change in one idle Box with its cached immutable base disk. confirm must be true.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "box_id": { "type": "string", "format": "uuid" },
                        "confirm": { "type": "boolean", "description": "Explicitly authorize discarding all Box changes." }
                    },
                    "required": ["box_id", "confirm"],
                    "additionalProperties": false
                },
                "outputSchema": tool_output_schema(box_metadata_schema()),
                "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false }
            },
            {
                "name": "sandbox_box_clone",
                "title": "Clone persistent Box",
                "description": "Create a new independent Box from the current disk of an idle Box. confirm must be true because this allocates durable state.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "box_id": { "type": "string", "format": "uuid" },
                        "confirm": { "type": "boolean", "description": "Explicitly authorize creation of durable cloned state." }
                    },
                    "required": ["box_id", "confirm"],
                    "additionalProperties": false
                },
                "outputSchema": tool_output_schema(box_metadata_schema()),
                "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false }
            },
            {
                "name": "sandbox_box_update",
                "title": "Update persistent Box metadata",
                "description": "Rename an idle Box or atomically add and remove labels and tags. Names are unique case-insensitively.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "box_id": { "type": "string", "format": "uuid" },
                        "name": { "type": "string" },
                        "clear_name": { "type": "boolean", "default": false },
                        "set_labels": { "type": "object", "additionalProperties": { "type": "string" } },
                        "remove_labels": { "type": "array", "items": { "type": "string" }, "uniqueItems": true },
                        "add_tags": { "type": "array", "items": { "type": "string" }, "uniqueItems": true },
                        "remove_tags": { "type": "array", "items": { "type": "string" }, "uniqueItems": true }
                    },
                    "required": ["box_id"],
                    "additionalProperties": false
                },
                "outputSchema": tool_output_schema(box_metadata_schema()),
                "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
            },
            {
                "name": "sandbox_box_export",
                "title": "Export persistent Box",
                "description": "Create a new SHA-256-verified sparse tar bundle from one idle ready Box. Existing destination files are never overwritten.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "box_id": { "type": "string", "format": "uuid" },
                        "destination": { "type": "string" }
                    },
                    "required": ["box_id", "destination"],
                    "additionalProperties": false
                },
                "outputSchema": tool_output_schema(box_bundle_output_schema()),
                "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false }
            },
            {
                "name": "sandbox_box_import",
                "title": "Import persistent Box",
                "description": "Verify a versioned Box tar bundle and atomically restore it under a new BoxId without overwriting existing Boxes.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "source": { "type": "string" } },
                    "required": ["source"],
                    "additionalProperties": false
                },
                "outputSchema": tool_output_schema(box_metadata_schema()),
                "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false }
            }
        ]
    })
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
struct ExecArgs {
    argv: Vec<String>,
    stdin_base64: Option<String>,
    box_id: Option<BoxId>,
    pull_policy: ImagePullPolicy,
    timeout_ms: Option<u64>,
    unlimited: bool,
    output_limit_bytes: Option<usize>,
    kill_grace_ms: Option<u64>,
    network: bool,
    tty: bool,
    workspace_mode: WorkspaceMode,
    copy_in: Vec<CopyInSpec>,
    copy_out: Vec<CopyOutSpec>,
    copy_limit_bytes: u64,
    wait: bool,
}

impl Default for ExecArgs {
    fn default() -> Self {
        Self {
            argv: Vec::new(),
            stdin_base64: None,
            box_id: None,
            pull_policy: ImagePullPolicy::Missing,
            timeout_ms: None,
            unlimited: false,
            output_limit_bytes: None,
            kill_grace_ms: None,
            network: false,
            tty: false,
            workspace_mode: WorkspaceMode::ReadOnly,
            copy_in: Vec::new(),
            copy_out: Vec::new(),
            copy_limit_bytes: DEFAULT_COPY_LIMIT,
            wait: true,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct IoArgs {
    session_id: String,
    cursor: u64,
    max_bytes: usize,
    wait_ms: u64,
    stdin_base64: Option<String>,
    close_stdin: bool,
    rows: Option<u16>,
    columns: Option<u16>,
    signal: Option<String>,
}

impl Default for IoArgs {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            cursor: 0,
            max_bytes: 1024 * 1024,
            wait_ms: 0,
            stdin_base64: None,
            close_stdin: false,
            rows: None,
            columns: None,
            signal: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StopArgs {
    session_id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct EmptyArgs {}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BoxCreateArgs {
    image: Option<String>,
    pull_policy: ImagePullPolicy,
    disk_size_bytes: Option<u64>,
    name: Option<String>,
    labels: BTreeMap<String, String>,
    tags: BTreeSet<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BoxListArgs {
    name: Option<String>,
    labels: BTreeMap<String, Option<String>>,
    tags: BTreeSet<String>,
    state: Option<BoxState>,
    sort_by: BoxSortBy,
    descending: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoxUpdateArgs {
    box_id: BoxId,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    clear_name: bool,
    #[serde(default)]
    set_labels: BTreeMap<String, String>,
    #[serde(default)]
    remove_labels: BTreeSet<String>,
    #[serde(default)]
    add_tags: BTreeSet<String>,
    #[serde(default)]
    remove_tags: BTreeSet<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoxExportArgs {
    box_id: BoxId,
    destination: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoxImportArgs {
    source: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoxIdArgs {
    box_id: BoxId,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfirmedBoxArgs {
    box_id: BoxId,
    confirm: bool,
}

fn platform_name(platform: &Platform) -> String {
    match &platform.variant {
        Some(variant) => format!("{}/{}/{}", platform.os, platform.architecture, variant),
        None => format!("{}/{}", platform.os, platform.architecture),
    }
}

pub(super) fn parse_disk_size(input: &str) -> Result<u64, String> {
    let input = input.trim();
    let split = input
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(input.len());
    let (number, suffix) = input.split_at(split);
    let value = number
        .parse::<u64>()
        .map_err(|_| "disk size must start with a positive integer".to_owned())?;
    if value == 0 {
        return Err("disk size must be greater than zero".into());
    }
    let multiplier = match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kib" => 1024,
        "mib" => 1024 * 1024,
        "gib" => 1024 * 1024 * 1024,
        "kb" => 1000,
        "mb" => 1000 * 1000,
        "gb" => 1000 * 1000 * 1000,
        _ => return Err("disk size suffix must be B, KiB, MiB, GiB, KB, MB, or GB".into()),
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| "disk size is too large".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{ConnectionState, handle_request, initialize_protocol_version};

    #[test]
    fn connection_state_supports_stateless_and_legacy_modes() {
        let tools = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
        let initialize = json!({
            "jsonrpc":"2.0","id":2,"method":"initialize",
            "params":{"protocolVersion":"2025-11-25"}
        });

        let mut stateless = ConnectionState::default();
        stateless.accept_request(&tools).unwrap();
        assert_eq!(stateless, ConnectionState::Stateless);
        assert_eq!(stateless.accept_request(&initialize).unwrap_err().0, -32600);

        let mut legacy = ConnectionState::default();
        legacy.accept_request(&initialize).unwrap();
        assert_eq!(legacy, ConnectionState::AwaitingInitialized);
        assert_eq!(legacy.accept_request(&tools).unwrap_err().0, -32002);
        legacy.initialized();
        legacy.accept_request(&tools).unwrap();
        assert_eq!(legacy, ConnectionState::LegacyReady);
    }

    #[test]
    fn initialize_protocol_version_is_required_and_bounded() {
        for version in SUPPORTED_PROTOCOL_VERSIONS {
            let request = json!({"params":{"protocolVersion":version}});
            assert_eq!(initialize_protocol_version(&request).unwrap(), version);
        }
        assert!(initialize_protocol_version(&json!({"params":{}})).is_err());
        assert!(
            initialize_protocol_version(&json!({
                "params":{"protocolVersion":"2099-01-01"}
            }))
            .unwrap_err()
            .contains("unsupported protocol version")
        );
    }

    #[test]
    fn tools_advertise_output_contracts_and_side_effects() {
        let list = tools_list();
        let tools = list
            .get("tools")
            .and_then(Value::as_array)
            .expect("tools list");
        assert_eq!(tools.len(), 15);
        assert!(tools.iter().all(|tool| tool.get("outputSchema").is_some()));
        assert!(tools.iter().all(|tool| {
            tool.pointer("/outputSchema/oneOf/1/properties/error")
                .is_some()
        }));

        let advertised = |name: &str| {
            tools
                .iter()
                .find(|tool| tool.get("name") == Some(&json!(name)))
                .unwrap_or_else(|| panic!("missing advertised tool {name}"))
        };
        for name in [
            "sandbox_exec",
            "sandbox_io",
            "sandbox_stop",
            "sandbox_remove",
        ] {
            let tool = advertised(name);
            assert_eq!(
                tool.pointer("/annotations/readOnlyHint"),
                Some(&json!(false)),
                "{name} must disclose mutation"
            );
            assert_eq!(
                tool.pointer("/annotations/destructiveHint"),
                Some(&json!(true)),
                "{name} must disclose destructive inputs"
            );
        }
        for name in [
            "sandbox_session_list",
            "sandbox_session_status",
            "sandbox_box_list",
            "sandbox_box_get",
        ] {
            assert_eq!(
                advertised(name).pointer("/annotations/readOnlyHint"),
                Some(&json!(true)),
                "{name} must be read-only"
            );
        }
        for name in ["sandbox_box_delete", "sandbox_box_reset"] {
            assert_eq!(
                advertised(name).pointer("/annotations/destructiveHint"),
                Some(&json!(true)),
                "{name} must disclose destructive Box changes"
            );
        }

        assert!(
            advertised("sandbox_exec")
                .pointer("/outputSchema/oneOf/0/oneOf")
                .is_some(),
            "sandbox_exec must describe wait=true and wait=false results"
        );
        assert_eq!(
            advertised("sandbox_io")
                .pointer("/outputSchema/oneOf/0/properties/wait_timed_out/type"),
            Some(&json!("boolean"))
        );
        assert_eq!(
            advertised("sandbox_box_create")
                .pointer("/outputSchema/oneOf/0/properties/owner_uid/anyOf/1/type"),
            Some(&json!("null"))
        );
    }

    #[tokio::test]
    async fn initialize_list_and_call_work() {
        let server = test_server(SandboxSdk::new(Arc::new(ProcessBackend)));
        let initialized = handle_request(
            &server,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}),
        )
        .await;
        assert_eq!(
            initialized.pointer("/result/protocolVersion"),
            Some(&json!("2025-11-25"))
        );
        let instructions = initialized
            .pointer("/result/instructions")
            .and_then(Value::as_str)
            .unwrap();
        for expected in [
            "untrusted code",
            "dependency installation",
            "long-running sessions",
            "Only the libkrun backend provides VM isolation",
            "process backend is for deterministic development and is not isolated",
            "Host workspace files are not attached automatically",
            "new microVM and SessionId for every run",
        ] {
            assert!(instructions.contains(expected), "missing {expected:?}");
        }
        let list = handle_request(
            &server,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .await;
        assert_eq!(
            list.pointer("/result/tools/0/name"),
            Some(&json!("sandbox_exec"))
        );
        assert_eq!(
            list.pointer("/result/tools/0/title"),
            Some(&json!("Execute in configured runtime"))
        );
        let exec_description = list
            .pointer("/result/tools/0/description")
            .and_then(Value::as_str)
            .unwrap();
        assert!(exec_description.contains("with libkrun"));
        assert!(exec_description.contains("reproducible Linux checks"));
        assert!(exec_description.contains("Network access is disabled by default"));
        assert!(exec_description.contains("continuation_cursor"));
        assert_eq!(
            list.pointer("/result/tools/0/inputSchema/properties/network/default"),
            Some(&json!(false))
        );
        assert_eq!(
            list.pointer("/result/tools/0/inputSchema/properties/box_id/format"),
            Some(&json!("uuid"))
        );
        assert_eq!(
            list.pointer("/result/tools/0/inputSchema/additionalProperties"),
            Some(&json!(false))
        );
        assert_eq!(
            list.pointer("/result/tools/1/inputSchema/properties/rows/maximum"),
            Some(&json!(u16::MAX))
        );
        assert_eq!(
            list.pointer("/result/tools/0/annotations/openWorldHint"),
            Some(&json!(true))
        );
        assert!(
            list.pointer("/result/tools")
                .and_then(Value::as_array)
                .is_some_and(|tools| tools
                    .iter()
                    .any(|tool| tool.get("name") == Some(&json!("sandbox_remove"))))
        );
        let call = handle_request(
            &server,
            json!({
                "jsonrpc":"2.0","id":3,"method":"tools/call",
                "params":{"name":"sandbox_exec","arguments":{"argv":successful_command()}}
            }),
        )
        .await;
        assert_eq!(
            call.pointer("/result/structuredContent/status/exit_code"),
            Some(&json!(0))
        );
        assert_exec_pull_contract(&list, &call);
        assert_eq!(
            call.pointer("/result/structuredContent/output/0/text"),
            Some(&json!("mcp"))
        );
        assert!(
            call.pointer("/result/structuredContent/output/0/data_base64")
                .is_none()
        );
        let content_text = call
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .unwrap();
        assert!(content_text.starts_with("sandbox_exec completed"));
        assert!(!content_text.contains("mcp"));
    }

    #[tokio::test]
    async fn exec_schema_advertises_bounded_resource_controls() {
        let server = test_server(SandboxSdk::new(Arc::new(ProcessBackend)));
        let list = handle_request(
            &server,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        )
        .await;

        assert_eq!(
            list.pointer("/result/tools/0/inputSchema/properties/output_limit_bytes/maximum"),
            Some(&json!(MAX_OUTPUT_LIMIT))
        );
        assert_eq!(
            list.pointer("/result/tools/0/inputSchema/properties/kill_grace_ms/maximum"),
            Some(&json!(60_000))
        );
        assert_eq!(
            list.pointer("/result/tools/0/inputSchema/properties/pull_policy/enum"),
            Some(&json!(["missing", "always", "never"]))
        );
        assert_eq!(
            list.pointer("/result/tools/0/inputSchema/properties/copy_limit_bytes/maximum"),
            Some(&json!(MAX_COPY_LIMIT))
        );
        assert_eq!(
            list.pointer("/result/tools/0/inputSchema/properties/workspace_mode/enum"),
            Some(&json!(["read_only", "overlay"]))
        );
    }

    fn assert_exec_pull_contract(list: &Value, call: &Value) {
        assert_eq!(call.pointer("/result/isError"), Some(&json!(false)));
        assert_eq!(
            list.pointer("/result/tools/0/inputSchema/properties/pull_policy/default"),
            Some(&json!("missing"))
        );
        assert_eq!(
            list.pointer(
                "/result/tools/0/outputSchema/oneOf/0/oneOf/0/properties/status/properties/resolved_image_digest/anyOf/1/type"
            ),
            Some(&json!("null"))
        );
        assert_eq!(
            call.pointer("/result/structuredContent/status/resolved_image_digest"),
            Some(&Value::Null)
        );
    }

    #[tokio::test]
    async fn session_wait_and_query_tools_are_advertised() {
        let server = test_server(SandboxSdk::new(Arc::new(ProcessBackend)));
        let list = handle_request(
            &server,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        )
        .await;

        assert_eq!(
            list.pointer("/result/tools/1/inputSchema/properties/wait_ms/maximum"),
            Some(&json!(MAX_MCP_WAIT_MS))
        );
        for expected in ["sandbox_session_list", "sandbox_session_status"] {
            assert!(
                list.pointer("/result/tools")
                    .and_then(Value::as_array)
                    .is_some_and(|tools| tools
                        .iter()
                        .any(|tool| tool.get("name") == Some(&json!(expected)))),
                "missing {expected}"
            );
        }
    }

    #[tokio::test]
    async fn waiting_exec_exposes_a_real_continuation_without_duplicating_output() {
        let sdk = SandboxSdk::new(Arc::new(ProcessBackend));
        let result =
            sandbox_exec_with_inline_limit(&sdk, json!({ "argv": successful_command() }), None, 2)
                .await
                .unwrap();

        assert_eq!(
            result.structured_content.get("has_more"),
            Some(&json!(true))
        );
        assert_eq!(
            result.structured_content.get("continuation_cursor"),
            Some(&json!(2))
        );
        assert_eq!(
            result.structured_content.get("output_next_cursor"),
            Some(&json!(3))
        );
        assert!(!result.content_text.contains("mcp"));
        let session_id = result
            .structured_content
            .pointer("/status/session_id")
            .and_then(Value::as_str)
            .unwrap()
            .parse::<SessionId>()
            .unwrap();
        let continuation = sdk
            .io(
                session_id,
                IoRequest {
                    cursor: 2,
                    ..IoRequest::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(continuation.next_cursor, 3);
        assert_eq!(continuation.output[0].data, b"p");
        sdk.remove(session_id).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn waiting_exec_removes_a_session_when_inline_output_is_complete() {
        let sdk = SandboxSdk::new(Arc::new(ProcessBackend));
        let result =
            sandbox_exec_with_inline_limit(&sdk, json!({ "argv": successful_command() }), None, 3)
                .await
                .unwrap();
        let session_id = result
            .structured_content
            .pointer("/status/session_id")
            .and_then(Value::as_str)
            .unwrap()
            .parse::<SessionId>()
            .unwrap();

        assert_eq!(
            result.structured_content.get("has_more"),
            Some(&json!(false))
        );
        assert_eq!(
            result.structured_content.get("continuation_cursor"),
            Some(&Value::Null)
        );
        assert!(matches!(
            sdk.wait(session_id).await,
            Err(SdkError::UnknownSession(id)) if id == session_id
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exec_applies_output_limit_and_accepts_bounded_kill_grace() {
        let sdk = SandboxSdk::new(Arc::new(ProcessBackend));
        let result = sandbox_exec_with_inline_limit(
            &sdk,
            json!({
                "argv": successful_command(),
                "output_limit_bytes": 2,
                "kill_grace_ms": 250
            }),
            None,
            16,
        )
        .await
        .unwrap();

        assert_eq!(
            result.structured_content.get("truncated"),
            Some(&json!(true))
        );
        assert_eq!(
            result.structured_content.pointer("/output/0/text"),
            Some(&json!("cp"))
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn tool_handlers_reject_schema_bypasses_before_execution() {
        let server = test_server(SandboxSdk::new(Arc::new(ProcessBackend)));
        let cases = [
            (
                "sandbox_exec",
                json!({ "argv": successful_command(), "unknown": true }),
                "unknown field",
            ),
            (
                "sandbox_exec",
                json!({
                    "argv": successful_command(),
                    "unlimited": true,
                    "timeout_ms": 10
                }),
                "cannot be combined",
            ),
            (
                "sandbox_exec",
                json!({ "argv": successful_command(), "output_limit_bytes": 0 }),
                "output_limit_bytes must be between",
            ),
            (
                "sandbox_exec",
                json!({
                    "argv": successful_command(),
                    "output_limit_bytes": MAX_OUTPUT_LIMIT + 1
                }),
                "output_limit_bytes must be between",
            ),
            (
                "sandbox_exec",
                json!({ "argv": successful_command(), "kill_grace_ms": 0 }),
                "kill_grace_ms must be between",
            ),
            (
                "sandbox_exec",
                json!({ "argv": successful_command(), "kill_grace_ms": 60_001 }),
                "kill_grace_ms must be between",
            ),
            (
                "sandbox_exec",
                json!({ "argv": successful_command(), "copy_limit_bytes": 0 }),
                "copy_limit_bytes must be between",
            ),
            (
                "sandbox_exec",
                json!({
                    "argv": successful_command(),
                    "copy_out": [{ "source": "/workspace/../host", "destination": "/tmp/result" }]
                }),
                "copy-out requires",
            ),
            (
                "sandbox_io",
                json!({ "session_id": SessionId::new(), "max_bytes": 0 }),
                "max_bytes must be between",
            ),
            (
                "sandbox_io",
                json!({ "session_id": SessionId::new(), "wait_ms": MAX_MCP_WAIT_MS + 1 }),
                "wait_ms must be between",
            ),
            (
                "sandbox_io",
                json!({ "session_id": SessionId::new(), "rows": 24 }),
                "provided together",
            ),
            (
                "sandbox_io",
                json!({ "session_id": SessionId::new(), "rows": 0, "columns": 80 }),
                "greater than zero",
            ),
            (
                "sandbox_io",
                json!({ "session_id": SessionId::new(), "rows": 65_536, "columns": 80 }),
                "invalid value",
            ),
        ];
        for (index, (name, arguments, expected)) in cases.into_iter().enumerate() {
            let response = handle_request(
                &server,
                json!({
                    "jsonrpc": "2.0",
                    "id": index,
                    "method": "tools/call",
                    "params": { "name": name, "arguments": arguments }
                }),
            )
            .await;
            assert_eq!(response.pointer("/result/isError"), Some(&json!(true)));
            let envelope = response
                .pointer("/result/structuredContent/error")
                .and_then(Value::as_object)
                .unwrap();
            assert_eq!(envelope.get("code"), Some(&json!("invalid_arguments")));
            assert_eq!(envelope.get("stage"), Some(&json!("request_validation")));
            assert_eq!(envelope.get("retryable"), Some(&json!(false)));
            assert!(
                envelope
                    .get("remediation")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty())
            );
            let message = envelope.get("message").and_then(Value::as_str).unwrap();
            assert!(
                message.contains(expected),
                "expected {expected:?} in {message:?}"
            );
        }
    }

    #[test]
    fn stdin_decoder_rejects_encoded_and_decoded_oversize_inputs() {
        let oversized = STANDARD.encode(vec![0_u8; MAX_MCP_STDIN_BYTES + 1]);
        let error = decode_bounded_stdin(Some(oversized)).unwrap_err();
        assert!(
            error.message.contains("decoded input limit") || error.message.contains("decodes to")
        );
        assert_eq!(error.code, "invalid_arguments");

        let encoded_oversize = "A".repeat(MAX_MCP_STDIN_BASE64_CHARS + 1);
        assert!(
            decode_bounded_stdin(Some(encoded_oversize))
                .unwrap_err()
                .message
                .contains("decoded input limit")
        );
    }

    #[test]
    fn cursor_expiry_error_includes_recovery_cursor() {
        let output = ToolError::from(SdkError::Session(SessionError::Output(
            OutputReadError::CursorExpired {
                requested: 4,
                earliest: 12,
            },
        )))
        .output();

        assert_eq!(
            output.structured_content.pointer("/error/code"),
            Some(&json!("cursor_expired"))
        );
        assert_eq!(
            output.structured_content.pointer("/error/stage"),
            Some(&json!("output_read"))
        );
        assert_eq!(
            output.structured_content.pointer("/error/retryable"),
            Some(&json!(true))
        );
        assert_eq!(
            output.structured_content.pointer("/error/earliest_cursor"),
            Some(&json!(12))
        );
        assert!(
            output
                .structured_content
                .pointer("/error/remediation")
                .and_then(Value::as_str)
                .is_some_and(|value| value.contains("cursor=12"))
        );
    }

    #[test]
    fn output_chunks_are_rendered_as_lossy_utf8_text() {
        let output = chunks_json(&[OutputChunk {
            cursor: 7,
            channel: moraebox_core::OutputChannel::Stdout,
            data: vec![b'o', b'k', 0xff],
        }]);

        assert_eq!(output[0].get("cursor"), Some(&json!(7)));
        assert_eq!(output[0].get("channel"), Some(&json!("stdout")));
        assert_eq!(output[0].get("text"), Some(&json!("ok\u{fffd}")));
        assert!(output[0].get("data_base64").is_none());
    }

    #[tokio::test]
    async fn process_backend_rejects_vm_network_opt_in() {
        let server = test_server(SandboxSdk::new(Arc::new(ProcessBackend)));
        let call = handle_request(
            &server,
            json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{
                    "name":"sandbox_exec",
                    "arguments":{"argv":successful_command(),"network":true}
                }
            }),
        )
        .await;

        assert_eq!(call.pointer("/result/isError"), Some(&json!(true)));
        assert!(
            call.pointer("/result/structuredContent/error/message")
                .and_then(Value::as_str)
                .is_some_and(|error| error.contains("without VM isolation"))
        );
        assert_eq!(
            call.pointer("/result/structuredContent/error/code"),
            Some(&json!("unsupported_capability"))
        );
    }

    #[tokio::test]
    async fn process_backend_rejects_box_execution() {
        let server = test_server(SandboxSdk::new(Arc::new(ProcessBackend)));
        let call = handle_request(
            &server,
            json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{
                    "name":"sandbox_exec",
                    "arguments":{"argv":successful_command(),"box_id":BoxId::new()}
                }
            }),
        )
        .await;

        assert_eq!(call.pointer("/result/isError"), Some(&json!(true)));
        assert!(
            call.pointer("/result/structuredContent/error/message")
                .and_then(Value::as_str)
                .is_some_and(|error| error.contains("Box persistence"))
        );
        assert_eq!(
            call.pointer("/result/structuredContent/error/code"),
            Some(&json!("unsupported_capability"))
        );
    }

    #[tokio::test]
    async fn box_list_get_and_confirmed_delete_work() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.ext4");
        let file = std::fs::File::create(&source).unwrap();
        file.set_len(1024 * 1024).unwrap();
        let store = BoxStore::new(temporary.path().join("state"));
        let metadata = store
            .create(
                &CreateBox::new("sha256:test", "linux/arm64", 1024 * 1024),
                &source,
            )
            .unwrap();
        std::fs::create_dir(store.boxes_directory().join("broken-entry")).unwrap();
        let server = test_server(SandboxSdk::new(Arc::new(ProcessBackend)).with_box_store(store));

        let list = call(&server, "sandbox_box_list", json!({})).await;
        assert_eq!(
            list.pointer("/result/structuredContent/boxes/0/box_id"),
            Some(&json!(metadata.box_id))
        );
        assert_eq!(
            list.pointer("/result/structuredContent/errors/0/code"),
            Some(&json!("invalid_name"))
        );
        let bundle = temporary.path().join("box.tar");
        let exported = call(
            &server,
            "sandbox_box_export",
            json!({"box_id": metadata.box_id, "destination": bundle}),
        )
        .await;
        assert_eq!(exported.pointer("/result/isError"), Some(&json!(false)));
        let imported = call(
            &server,
            "sandbox_box_import",
            json!({"source": temporary.path().join("box.tar")}),
        )
        .await;
        assert_eq!(imported.pointer("/result/isError"), Some(&json!(false)));
        assert_ne!(
            imported.pointer("/result/structuredContent/box_id"),
            Some(&json!(metadata.box_id))
        );
        let updated = call(
            &server,
            "sandbox_box_update",
            json!({
                "box_id": metadata.box_id,
                "name": "dev-box",
                "set_labels": {"team": "core"},
                "add_tags": ["warm"]
            }),
        )
        .await;
        assert_eq!(
            updated.pointer("/result/structuredContent/name"),
            Some(&json!("dev-box"))
        );
        let filtered = call(
            &server,
            "sandbox_box_list",
            json!({
                "labels": {"team": "core"},
                "tags": ["warm"],
                "sort_by": "name"
            }),
        )
        .await;
        assert_eq!(
            filtered.pointer("/result/structuredContent/boxes/0/box_id"),
            Some(&json!(metadata.box_id))
        );
        let get = call(
            &server,
            "sandbox_box_get",
            json!({"box_id": metadata.box_id}),
        )
        .await;
        assert_eq!(
            get.pointer("/result/structuredContent/box_id"),
            Some(&json!(metadata.box_id))
        );
        let unconfirmed = call(
            &server,
            "sandbox_box_delete",
            json!({"box_id": metadata.box_id, "confirm": false}),
        )
        .await;
        assert_eq!(unconfirmed.pointer("/result/isError"), Some(&json!(true)));
        let deleted = call(
            &server,
            "sandbox_box_delete",
            json!({"box_id": metadata.box_id, "confirm": true}),
        )
        .await;
        assert_eq!(deleted.pointer("/result/isError"), Some(&json!(false)));
    }

    #[test]
    fn bare_invocation_shows_help_unless_runtime_is_configured() {
        let error = parse_args_from(["morae-mcp"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);

        let raw_args = [OsString::from("morae-mcp")];
        let mut configured = Args::try_parse_from(["morae-mcp"]).unwrap();
        assert_eq!(configured.server.backend, "libkrun");
        assert!(configured.server.cache_dir.is_none());
        assert!(configured.server.state_dir.is_none());
        assert!(should_show_bare_help(&raw_args, &configured));
        configured.server.rootfs = Some("rootfs".into());
        assert!(!should_show_bare_help(&raw_args, &configured));

        let image = parse_args_from(["morae-mcp", "--image", "python:3.12"]).unwrap();
        assert_eq!(image.server.image.as_deref(), Some("python:3.12"));
        assert!(
            parse_args_from(["morae-mcp", "--rootfs", "rootfs", "--image", "python:3.12"]).is_err()
        );

        let process = parse_args_from(["morae-mcp", "--backend", "process"]).unwrap();
        assert_eq!(process.server.backend, "process");

        let explicit = parse_args_from([
            "morae-mcp",
            "--cache-dir",
            "custom-cache",
            "--state-dir",
            "custom-state",
            "--image",
            "python:3.12",
        ])
        .unwrap();
        assert_eq!(explicit.server.cache_dir, Some("custom-cache".into()));
        assert_eq!(explicit.server.state_dir, Some("custom-state".into()));
    }

    #[tokio::test]
    async fn process_server_rejects_a_guest_rootfs() {
        let result = create_server(ServerArgs {
            backend: "process".into(),
            helper: None,
            libkrun: None,
            gvproxy: None,
            rootfs: Some("ignored-rootfs".into()),
            image: None,
            workspace: None,
            cache_dir: Some(".moraebox/cache".into()),
            state_dir: Some(".moraebox/state".into()),
            registry_username: None,
            registry_password: None,
            lib_dir: None,
            cpus: 2,
            memory_mib: 512,
            mke2fs: None,
            e2fsck: None,
            debugfs: None,
            disk_size: 8 * 1024 * 1024 * 1024,
        });

        let Err(error) = result else {
            panic!("process server unexpectedly accepted a guest rootfs");
        };
        assert_eq!(
            error.to_string(),
            "--rootfs and --image require --backend libkrun"
        );
    }

    #[tokio::test]
    async fn image_preparation_is_lazy_and_reports_tool_errors() {
        let temporary = tempfile::tempdir().unwrap();
        let cache_dir = temporary.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join("default-image.json"), b"{").unwrap();
        let runtime_stub = temporary.path().join("runtime-stub");
        std::fs::write(&runtime_stub, b"stub").unwrap();
        let server = create_server(ServerArgs {
            backend: "libkrun".into(),
            helper: Some(runtime_stub.clone()),
            libkrun: Some(runtime_stub),
            gvproxy: None,
            rootfs: None,
            image: None,
            workspace: None,
            cache_dir: Some(cache_dir.clone()),
            state_dir: Some(temporary.path().join("state")),
            registry_username: None,
            registry_password: None,
            lib_dir: None,
            cpus: 2,
            memory_mib: 512,
            mke2fs: None,
            e2fsck: None,
            debugfs: None,
            disk_size: 8 * 1024 * 1024 * 1024,
        })
        .expect("server creation must not resolve the image");

        let initialized = handle_request(
            &server,
            json!({
                "jsonrpc":"2.0", "id":1, "method":"initialize",
                "params":{"protocolVersion":PROTOCOL_VERSION}
            }),
        )
        .await;
        assert_eq!(
            initialized.pointer("/result/protocolVersion"),
            Some(&json!(PROTOCOL_VERSION))
        );

        let failed = call(
            &server,
            "sandbox_exec",
            json!({"argv": successful_command()}),
        )
        .await;
        assert_eq!(failed.pointer("/result/isError"), Some(&json!(true)));
        assert_eq!(
            failed.pointer("/result/structuredContent/error/code"),
            Some(&json!("image_prepare_failed"))
        );
        assert_eq!(
            failed.pointer("/result/structuredContent/error/stage"),
            Some(&json!("image_pull"))
        );

        let persistent = call(
            &server,
            "sandbox_exec",
            json!({"argv": successful_command(), "box_id": BoxId::new()}),
        )
        .await;
        assert_ne!(
            persistent.pointer("/result/structuredContent/error/code"),
            Some(&json!("image_prepare_failed")),
            "persistent Box execution must bypass default image preparation"
        );
    }

    fn test_server(sdk: SandboxSdk) -> McpServer {
        let root =
            std::env::temp_dir().join(format!("moraebox-mcp-test-services-{}", std::process::id()));
        McpServer {
            sdk,
            boxes: BoxServices {
                images: ImageCache::new(&root),
                base_disks: BaseDiskStore::new(&root),
                platform: Platform::host_linux(),
                credentials: None,
                mke2fs_path: DiskToolPaths::discover(None, None).mke2fs_command(),
                default_disk_size: 8 * 1024 * 1024 * 1024,
            },
        }
    }

    async fn call(server: &McpServer, name: &str, arguments: Value) -> Value {
        handle_request(
            server,
            json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":name,"arguments":arguments}
            }),
        )
        .await
    }

    #[cfg(unix)]
    fn successful_command() -> Vec<String> {
        ["/usr/bin/printf", "mcp"].map(String::from).into()
    }

    #[cfg(windows)]
    fn successful_command() -> Vec<String> {
        vec![
            std::path::PathBuf::from(
                std::env::var_os("SystemRoot").expect("Windows must define SystemRoot"),
            )
            .join("System32")
            .join("cmd.exe")
            .to_string_lossy()
            .into_owned(),
            "/D".into(),
            "/C".into(),
            "exit /b 0".into(),
        ]
    }
}
