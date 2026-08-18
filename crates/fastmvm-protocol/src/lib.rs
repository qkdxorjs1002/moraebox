//! Versioned host/guest frames used over virtio-vsock.

#![forbid(unsafe_code)]

use prost::Message;
use thiserror::Error;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_SIZE: usize = 8 * 1024 * 1024;

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
    if declared > MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge {
            size: declared,
            maximum: MAX_FRAME_SIZE,
        });
    }
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
}
