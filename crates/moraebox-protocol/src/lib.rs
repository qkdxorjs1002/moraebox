//! Versioned host/guest frames used over virtio-vsock.

#![forbid(unsafe_code)]

use prost::Message;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_SIZE: usize = 8 * 1024 * 1024;
pub const EXEC_STREAM_ID: u64 = 1;

#[derive(Clone, PartialEq, Message)]
pub struct Frame {
    #[prost(uint32, tag = "1")]
    pub protocol_version: u32,
    #[prost(string, tag = "2")]
    pub session_id: String,
    #[prost(uint64, tag = "3")]
    pub stream_id: u64,
    #[prost(uint64, tag = "4")]
    pub sequence: u64,
    #[prost(oneof = "frame::Payload", tags = "10, 11, 12, 13, 14, 15, 16, 17, 18")]
    pub payload: Option<frame::Payload>,
}

pub mod frame {
    use prost::Oneof;

    use super::{
        ExecRequest, Exit, Hello, Output, Resize, Shutdown, SignalRequest, Stdin, StdinEof,
    };

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Payload {
        #[prost(message, tag = "10")]
        Hello(Hello),
        #[prost(message, tag = "11")]
        Exec(ExecRequest),
        #[prost(message, tag = "12")]
        Stdin(Stdin),
        #[prost(message, tag = "13")]
        StdinEof(StdinEof),
        #[prost(message, tag = "14")]
        Resize(Resize),
        #[prost(message, tag = "15")]
        Signal(SignalRequest),
        #[prost(message, tag = "16")]
        Output(Output),
        #[prost(message, tag = "17")]
        Exit(Exit),
        #[prost(message, tag = "18")]
        Shutdown(Shutdown),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct Hello {
    #[prost(string, tag = "1")]
    pub agent_version: String,
    #[prost(string, repeated, tag = "2")]
    pub capabilities: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ExecRequest {
    #[prost(string, repeated, tag = "1")]
    pub argv: Vec<String>,
    #[prost(string, tag = "2")]
    pub cwd: String,
    #[prost(string, repeated, tag = "3")]
    pub env: Vec<String>,
    #[prost(bool, tag = "4")]
    pub tty: bool,
    #[prost(uint32, tag = "5")]
    pub rows: u32,
    #[prost(uint32, tag = "6")]
    pub cols: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct Stdin {
    #[prost(bytes = "vec", tag = "1")]
    pub data: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct StdinEof {}

#[derive(Clone, PartialEq, Message)]
pub struct Resize {
    #[prost(uint32, tag = "1")]
    pub rows: u32,
    #[prost(uint32, tag = "2")]
    pub cols: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct SignalRequest {
    #[prost(enumeration = "WireSignal", tag = "1")]
    pub signal: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum WireSignal {
    Interrupt = 0,
    Terminate = 1,
    Kill = 2,
    Hangup = 3,
}

#[derive(Clone, PartialEq, Message)]
pub struct Output {
    #[prost(enumeration = "WireOutputChannel", tag = "1")]
    pub channel: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum WireOutputChannel {
    Stdout = 0,
    Stderr = 1,
    Tty = 2,
}

#[derive(Clone, PartialEq, Message)]
pub struct Exit {
    #[prost(int32, tag = "1")]
    pub code: i32,
    #[prost(int32, optional, tag = "2")]
    pub signal: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Shutdown {
    #[prost(string, tag = "1")]
    pub reason: String,
}

impl Frame {
    pub fn new(session_id: impl Into<String>, stream_id: u64, payload: frame::Payload) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            session_id: session_id.into(),
            stream_id,
            sequence: 0,
            payload: Some(payload),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerRole {
    Host,
    Guest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState {
    Initial,
    Running,
    InputClosed,
    Finished,
}

/// Validates one direction of a protocol stream before payloads are acted on.
#[derive(Debug, Clone)]
pub struct InboundValidator {
    session_id: String,
    stream_id: u64,
    peer: PeerRole,
    next_sequence: u64,
    state: PeerState,
}

impl InboundValidator {
    pub fn new(session_id: impl Into<String>, stream_id: u64, peer: PeerRole) -> Self {
        Self {
            session_id: session_id.into(),
            stream_id,
            peer,
            next_sequence: 0,
            state: PeerState::Initial,
        }
    }

    pub fn accept(&mut self, frame: &Frame) -> Result<(), ProtocolError> {
        validate_frame(frame)?;
        if frame.session_id != self.session_id {
            return Err(ProtocolError::SessionMismatch);
        }
        if frame.stream_id != self.stream_id {
            return Err(ProtocolError::StreamMismatch {
                expected: self.stream_id,
                actual: frame.stream_id,
            });
        }
        if frame.sequence != self.next_sequence {
            return Err(ProtocolError::SequenceMismatch {
                expected: self.next_sequence,
                actual: frame.sequence,
            });
        }
        let payload = frame
            .payload
            .as_ref()
            .expect("frame validation requires payload");
        let next_state = transition(self.peer, self.state, payload).ok_or_else(|| {
            ProtocolError::UnexpectedPayload {
                peer: self.peer,
                state: self.state,
                payload: payload_name(payload),
            }
        })?;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(ProtocolError::SequenceExhausted)?;
        self.state = next_state;
        Ok(())
    }
}

/// Creates frames for one ordered direction of a protocol stream.
#[derive(Debug, Clone)]
pub struct FrameSequence {
    session_id: String,
    stream_id: u64,
    next_sequence: u64,
}

impl FrameSequence {
    pub fn new(session_id: impl Into<String>, stream_id: u64) -> Self {
        Self {
            session_id: session_id.into(),
            stream_id,
            next_sequence: 0,
        }
    }

    pub fn next(&mut self, payload: frame::Payload) -> Result<Frame, ProtocolError> {
        let sequence = self.next_sequence;
        self.next_sequence = sequence
            .checked_add(1)
            .ok_or(ProtocolError::SequenceExhausted)?;
        let mut frame = Frame::new(&self.session_id, self.stream_id, payload);
        frame.sequence = sequence;
        Ok(frame)
    }
}

pub async fn read_frame<R>(reader: &mut R) -> Result<Frame, ProtocolError>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0_u8; 4];
    reader
        .read_exact(&mut header)
        .await
        .map_err(|source| ProtocolError::ReadHeader { source })?;
    let declared = u32::from_be_bytes(header) as usize;
    validate_frame_size(declared)?;
    let mut body = vec![0_u8; declared];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|source| ProtocolError::ReadBody { declared, source })?;
    let frame = Frame::decode(body.as_slice())?;
    validate_frame(&frame)?;
    Ok(frame)
}

pub async fn write_frame<W>(writer: &mut W, frame: &Frame) -> Result<(), ProtocolError>
where
    W: AsyncWrite + Unpin,
{
    let bytes = encode_frame(frame)?;
    writer
        .write_all(&bytes)
        .await
        .map_err(|source| ProtocolError::Write { source })?;
    writer
        .flush()
        .await
        .map_err(|source| ProtocolError::Write { source })
}

pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>, ProtocolError> {
    validate_frame(frame)?;
    let encoded_len = frame.encoded_len();
    if encoded_len > MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge {
            size: encoded_len,
            maximum: MAX_FRAME_SIZE,
        });
    }
    let length = u32::try_from(encoded_len).map_err(|_| ProtocolError::FrameTooLarge {
        size: encoded_len,
        maximum: MAX_FRAME_SIZE,
    })?;
    let mut bytes = Vec::with_capacity(4 + encoded_len);
    bytes.extend_from_slice(&length.to_be_bytes());
    frame.encode(&mut bytes).expect("Vec writes cannot fail");
    Ok(bytes)
}

pub fn decode_frame(bytes: &[u8]) -> Result<Frame, ProtocolError> {
    if bytes.len() < 4 {
        return Err(ProtocolError::TruncatedHeader);
    }
    let declared = u32::from_be_bytes(bytes[..4].try_into().expect("length checked")) as usize;
    validate_frame_size(declared)?;
    if bytes.len() - 4 != declared {
        return Err(ProtocolError::LengthMismatch {
            declared,
            actual: bytes.len() - 4,
        });
    }
    let frame = Frame::decode(&bytes[4..])?;
    validate_frame(&frame)?;
    Ok(frame)
}

fn validate_frame_size(size: usize) -> Result<(), ProtocolError> {
    if size > MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge {
            size,
            maximum: MAX_FRAME_SIZE,
        });
    }
    Ok(())
}

fn transition(peer: PeerRole, state: PeerState, payload: &frame::Payload) -> Option<PeerState> {
    use frame::Payload::{Exec, Exit, Hello, Output, Resize, Shutdown, Signal, Stdin, StdinEof};

    match (peer, state, payload) {
        (PeerRole::Guest, PeerState::Initial, Hello(_))
        | (PeerRole::Host, PeerState::Initial, Exec(_))
        | (PeerRole::Guest, PeerState::Running, Output(_))
        | (PeerRole::Host, PeerState::Running, Stdin(_) | Resize(_) | Signal(_)) => {
            Some(PeerState::Running)
        }
        (PeerRole::Host, PeerState::Running, StdinEof(_)) => Some(PeerState::InputClosed),
        (PeerRole::Host, PeerState::InputClosed, Resize(_) | Signal(_)) => {
            Some(PeerState::InputClosed)
        }
        (PeerRole::Guest, PeerState::Running, Exit(_) | Shutdown(_))
        | (PeerRole::Host, PeerState::Running | PeerState::InputClosed, Shutdown(_)) => {
            Some(PeerState::Finished)
        }
        _ => None,
    }
}

fn payload_name(payload: &frame::Payload) -> &'static str {
    match payload {
        frame::Payload::Hello(_) => "hello",
        frame::Payload::Exec(_) => "exec",
        frame::Payload::Stdin(_) => "stdin",
        frame::Payload::StdinEof(_) => "stdin_eof",
        frame::Payload::Resize(_) => "resize",
        frame::Payload::Signal(_) => "signal",
        frame::Payload::Output(_) => "output",
        frame::Payload::Exit(_) => "exit",
        frame::Payload::Shutdown(_) => "shutdown",
    }
}

fn validate_frame(frame: &Frame) -> Result<(), ProtocolError> {
    if frame.protocol_version != PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion(frame.protocol_version));
    }
    if frame.session_id.is_empty() {
        return Err(ProtocolError::MissingSessionId);
    }
    if frame.payload.is_none() {
        return Err(ProtocolError::MissingPayload);
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("frame header is truncated")]
    TruncatedHeader,
    #[error("frame length mismatch: declared {declared}, actual {actual}")]
    LengthMismatch { declared: usize, actual: usize },
    #[error("frame size {size} exceeds maximum {maximum}")]
    FrameTooLarge { size: usize, maximum: usize },
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u32),
    #[error("session id is required")]
    MissingSessionId,
    #[error("frame payload is required")]
    MissingPayload,
    #[error("protocol session does not match the negotiated session")]
    SessionMismatch,
    #[error("protocol stream mismatch: expected {expected}, actual {actual}")]
    StreamMismatch { expected: u64, actual: u64 },
    #[error("protocol sequence mismatch: expected {expected}, actual {actual}")]
    SequenceMismatch { expected: u64, actual: u64 },
    #[error("unexpected {payload} payload from {peer:?} while {state:?}")]
    UnexpectedPayload {
        peer: PeerRole,
        state: PeerState,
        payload: &'static str,
    },
    #[error("protocol sequence space is exhausted")]
    SequenceExhausted,
    #[error("failed to read frame header: {source}")]
    ReadHeader { source: std::io::Error },
    #[error("failed to read {declared}-byte frame body: {source}")]
    ReadBody {
        declared: usize,
        source: std::io::Error,
    },
    #[error("failed to write protocol frame: {source}")]
    Write { source: std::io::Error },
    #[error("invalid protobuf frame: {0}")]
    Decode(#[from] prost::DecodeError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_frame() {
        let frame = Frame::new(
            "session",
            7,
            frame::Payload::Stdin(Stdin {
                data: b"hello".to_vec(),
            }),
        );
        let bytes = encode_frame(&frame).unwrap();
        assert_eq!(decode_frame(&bytes).unwrap(), frame);
    }

    #[test]
    fn stdin_eof_matches_the_guest_agent_golden_vector() {
        let frame = Frame::new(
            "session",
            EXEC_STREAM_ID,
            frame::Payload::StdinEof(StdinEof {}),
        );
        let bytes = encode_frame(&frame).unwrap();
        assert_eq!(&bytes[4..], b"\x08\x01\x12\x07session\x18\x01\x6a\x00");
    }

    #[test]
    fn rejects_a_mismatched_length() {
        let mut bytes = encode_frame(&Frame::new(
            "session",
            1,
            frame::Payload::Shutdown(Shutdown {
                reason: "test".into(),
            }),
        ))
        .unwrap();
        bytes.pop();
        assert!(matches!(
            decode_frame(&bytes),
            Err(ProtocolError::LengthMismatch { .. })
        ));
    }

    #[test]
    fn rejects_an_unknown_version() {
        let mut frame = Frame::new("session", 1, frame::Payload::StdinEof(StdinEof {}));
        frame.protocol_version = PROTOCOL_VERSION + 1;
        assert!(matches!(
            encode_frame(&frame),
            Err(ProtocolError::UnsupportedVersion(_))
        ));
    }

    #[tokio::test]
    async fn streams_exactly_one_bounded_frame() {
        let frame = Frame::new(
            "session",
            EXEC_STREAM_ID,
            frame::Payload::StdinEof(StdinEof {}),
        );
        let mut wire = Vec::new();
        write_frame(&mut wire, &frame).await.unwrap();
        let mut wire = wire.as_slice();
        assert_eq!(read_frame(&mut wire).await.unwrap(), frame);
    }

    #[tokio::test]
    async fn rejects_oversized_stream_headers_before_allocating() {
        let declared = u32::try_from(MAX_FRAME_SIZE + 1).unwrap();
        let header = declared.to_be_bytes();
        let mut wire = header.as_slice();
        assert!(matches!(
            read_frame(&mut wire).await,
            Err(ProtocolError::FrameTooLarge { .. })
        ));
    }

    #[tokio::test]
    async fn distinguishes_truncated_stream_headers_and_bodies() {
        let mut header = [0_u8; 2].as_slice();
        assert!(matches!(
            read_frame(&mut header).await,
            Err(ProtocolError::ReadHeader { .. })
        ));

        let mut body = [0, 0, 0, 4, 1, 2].as_slice();
        assert!(matches!(
            read_frame(&mut body).await,
            Err(ProtocolError::ReadBody { declared: 4, .. })
        ));
    }

    #[test]
    fn validates_direction_identity_and_sequence() {
        let mut frames = FrameSequence::new("session", EXEC_STREAM_ID);
        let hello = frames
            .next(frame::Payload::Hello(Hello {
                agent_version: "test".into(),
                capabilities: vec!["exec".into()],
            }))
            .unwrap();
        let output = frames
            .next(frame::Payload::Output(Output {
                channel: WireOutputChannel::Stdout as i32,
                data: b"ok".to_vec(),
            }))
            .unwrap();
        let exit = frames
            .next(frame::Payload::Exit(Exit {
                code: 0,
                signal: None,
            }))
            .unwrap();
        let mut validator = InboundValidator::new("session", EXEC_STREAM_ID, PeerRole::Guest);
        validator.accept(&hello).unwrap();
        validator.accept(&output).unwrap();
        validator.accept(&exit).unwrap();
        assert!(matches!(
            validator.accept(&output),
            Err(ProtocolError::SequenceMismatch { .. })
        ));

        let mut wrong_session = hello.clone();
        wrong_session.session_id = "other".into();
        let mut validator = InboundValidator::new("session", EXEC_STREAM_ID, PeerRole::Guest);
        assert!(matches!(
            validator.accept(&wrong_session),
            Err(ProtocolError::SessionMismatch)
        ));
    }

    #[test]
    fn rejects_payloads_from_the_wrong_peer_or_state() {
        let mut validator = InboundValidator::new("session", EXEC_STREAM_ID, PeerRole::Guest);
        let exec = Frame::new(
            "session",
            EXEC_STREAM_ID,
            frame::Payload::Exec(ExecRequest {
                argv: vec!["/bin/true".into()],
                cwd: String::new(),
                env: Vec::new(),
                tty: false,
                rows: 0,
                cols: 0,
            }),
        );
        assert!(matches!(
            validator.accept(&exec),
            Err(ProtocolError::UnexpectedPayload { .. })
        ));
    }

    #[test]
    fn host_can_signal_and_resize_after_closing_stdin() {
        let mut frames = FrameSequence::new("session", EXEC_STREAM_ID);
        let exec = frames
            .next(frame::Payload::Exec(ExecRequest {
                argv: vec!["/bin/cat".into()],
                cwd: String::new(),
                env: Vec::new(),
                tty: true,
                rows: 24,
                cols: 80,
            }))
            .unwrap();
        let eof = frames.next(frame::Payload::StdinEof(StdinEof {})).unwrap();
        let signal = frames
            .next(frame::Payload::Signal(SignalRequest {
                signal: WireSignal::Terminate as i32,
            }))
            .unwrap();
        let resize = frames
            .next(frame::Payload::Resize(Resize {
                rows: 40,
                cols: 120,
            }))
            .unwrap();
        let mut validator = InboundValidator::new("session", EXEC_STREAM_ID, PeerRole::Host);
        validator.accept(&exec).unwrap();
        validator.accept(&eof).unwrap();
        validator.accept(&signal).unwrap();
        validator.accept(&resize).unwrap();
    }
}
