# Host/guest protocol

The versioned host/guest protocol crate reserves a virtio-vsock control channel. TSI feature flags are zero. Frames use a four-byte big-endian size prefix followed by protobuf data and are limited to 8 MiB. The current native vertical slice uses libkrun's injected init and explicit console directly; a custom guest agent does not yet consume these frames.

Every frame contains:

- protocol version;
- session ID;
- stream ID;
- monotonically increasing sequence number;
- one typed payload.

Phase 1 payloads define the stable vocabulary:

- `Hello`: agent version and negotiated capabilities;
- `ExecRequest`: argv, cwd, explicit environment, PTY dimensions;
- `Stdin` and `StdinEof`;
- `Resize` and `SignalRequest`;
- `Output`: stdout, stderr, or merged PTY bytes;
- `Exit`: exit code or signal;
- `Shutdown`.

The host rejects missing payloads, missing session IDs, unknown protocol versions, inconsistent lengths, and oversized frames before dispatch. Later versions must add optional fields or negotiate a new protocol version rather than changing existing tag meanings.

MCP does not expose raw protocol frames. It maps these events to bounded calls with output cursors so a long-running command does not require one hour-long tool request.

The stdio MCP transport writes exactly one JSON-RPC response line per request and sends diagnostics only to stderr. It supports both legacy `initialize` clients and direct tool calls used by the 2026-07-28 protocol generation. Tool results include both text content and identical `structuredContent`; binary stream data is base64 encoded.
