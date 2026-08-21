use std::{collections::VecDeque, ops::Range, sync::Arc};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputChannel {
    Stdout,
    Stderr,
    Tty,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputChunk {
    pub cursor: u64,
    pub channel: OutputChannel,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputRead {
    pub chunks: Vec<OutputChunk>,
    pub next_cursor: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct OutputReadSnapshot {
    chunks: Vec<SharedOutputChunk>,
    next_cursor: u64,
    truncated: bool,
}

impl OutputReadSnapshot {
    #[must_use]
    pub fn materialize(self) -> OutputRead {
        OutputRead {
            chunks: self
                .chunks
                .into_iter()
                .map(|chunk| OutputChunk {
                    cursor: chunk.cursor,
                    channel: chunk.channel,
                    data: chunk.data[chunk.range].to_vec(),
                })
                .collect(),
            next_cursor: self.next_cursor,
            truncated: self.truncated,
        }
    }
}

#[derive(Debug, Clone)]
struct SharedOutputChunk {
    cursor: u64,
    channel: OutputChannel,
    data: Arc<[u8]>,
    range: Range<usize>,
}

#[derive(Debug, Clone)]
struct StoredOutputChunk {
    cursor: u64,
    channel: OutputChannel,
    data: Arc<[u8]>,
    start: usize,
}

impl StoredOutputChunk {
    fn len(&self) -> usize {
        self.data.len() - self.start
    }

    fn end_cursor(&self) -> u64 {
        self.cursor + self.len() as u64
    }
}

#[derive(Debug, Clone)]
pub struct OutputBuffer {
    capacity: usize,
    retained_bytes: usize,
    earliest_cursor: u64,
    next_cursor: u64,
    truncated: bool,
    chunks: VecDeque<StoredOutputChunk>,
}

impl OutputBuffer {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "output buffer capacity must be non-zero");
        Self {
            capacity,
            retained_bytes: 0,
            earliest_cursor: 0,
            next_cursor: 0,
            truncated: false,
            chunks: VecDeque::new(),
        }
    }

    pub fn push(&mut self, channel: OutputChannel, data: impl AsRef<[u8]>) -> u64 {
        let data = data.as_ref();
        let cursor = self.next_cursor;
        self.next_cursor += data.len() as u64;
        if data.is_empty() {
            return cursor;
        }
        self.retained_bytes += data.len();
        self.chunks.push_back(StoredOutputChunk {
            cursor,
            channel,
            data: Arc::from(data),
            start: 0,
        });
        self.trim();
        cursor
    }

    pub fn next_cursor(&self) -> u64 {
        self.next_cursor
    }

    pub fn earliest_cursor(&self) -> u64 {
        self.earliest_cursor
    }

    pub fn read(&self, cursor: u64, max_bytes: usize) -> Result<OutputRead, OutputReadError> {
        Ok(self.snapshot(cursor, max_bytes)?.materialize())
    }

    pub fn snapshot(
        &self,
        cursor: u64,
        max_bytes: usize,
    ) -> Result<OutputReadSnapshot, OutputReadError> {
        if cursor < self.earliest_cursor {
            return Err(OutputReadError::CursorExpired {
                requested: cursor,
                earliest: self.earliest_cursor,
            });
        }
        if cursor > self.next_cursor {
            return Err(OutputReadError::CursorAhead {
                requested: cursor,
                next: self.next_cursor,
            });
        }
        let mut remaining = max_bytes;
        let mut chunks = Vec::new();
        let mut next = cursor;
        for chunk in &self.chunks {
            if remaining == 0 {
                break;
            }
            if chunk.end_cursor() <= cursor {
                continue;
            }
            let offset = usize::try_from(cursor.saturating_sub(chunk.cursor))
                .expect("cursor offset cannot exceed the in-memory chunk length");
            let start = chunk.start + offset;
            let take = remaining.min(chunk.data.len() - start);
            chunks.push(SharedOutputChunk {
                cursor: chunk.cursor + offset as u64,
                channel: chunk.channel,
                data: Arc::clone(&chunk.data),
                range: start..start + take,
            });
            next = chunk.cursor + (offset + take) as u64;
            remaining -= take;
        }
        Ok(OutputReadSnapshot {
            chunks,
            next_cursor: next,
            truncated: self.truncated,
        })
    }

    fn trim(&mut self) {
        while self.retained_bytes > self.capacity {
            let excess = self.retained_bytes - self.capacity;
            let Some(front) = self.chunks.front_mut() else {
                break;
            };
            if excess >= front.len() {
                let removed = self.chunks.pop_front().expect("front exists");
                self.retained_bytes -= removed.len();
                self.earliest_cursor = removed.end_cursor();
            } else {
                front.start += excess;
                front.cursor += excess as u64;
                self.retained_bytes -= excess;
                self.earliest_cursor = front.cursor;
            }
            self.truncated = true;
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum OutputReadError {
    #[error("output cursor {requested} expired; earliest retained cursor is {earliest}")]
    CursorExpired { requested: u64, earliest: u64 },
    #[error("output cursor {requested} is ahead of next cursor {next}")]
    CursorAhead { requested: u64, next: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_channel_order_and_cursor() {
        let mut output = OutputBuffer::new(16);
        output.push(OutputChannel::Stdout, b"abc");
        output.push(OutputChannel::Stderr, b"de");
        let read = output.read(0, 16).unwrap();
        assert_eq!(read.next_cursor, 5);
        assert_eq!(read.chunks.len(), 2);
        assert_eq!(read.chunks[1].channel, OutputChannel::Stderr);
    }

    #[test]
    fn trims_to_an_exact_byte_capacity() {
        let mut output = OutputBuffer::new(4);
        output.push(OutputChannel::Stdout, b"abcdef");
        assert_eq!(output.earliest_cursor(), 2);
        assert!(matches!(
            output.read(0, 4),
            Err(OutputReadError::CursorExpired { .. })
        ));
        let read = output.read(2, 4).unwrap();
        assert_eq!(read.chunks[0].data, b"cdef");
        assert!(read.truncated);
    }

    #[test]
    fn snapshot_remains_readable_after_the_buffer_evicts_its_chunks() {
        let mut output = OutputBuffer::new(6);
        output.push(OutputChannel::Stdout, b"abcdef");
        let snapshot = output.snapshot(0, 6).unwrap();

        output.push(OutputChannel::Stderr, b"ghijkl");

        let read = snapshot.materialize();
        assert_eq!(read.chunks[0].data, b"abcdef");
        assert_eq!(read.next_cursor, 6);
        assert!(!read.truncated);
        assert_eq!(output.earliest_cursor(), 6);
    }
}
