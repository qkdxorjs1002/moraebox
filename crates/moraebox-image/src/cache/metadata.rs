use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::ROOTFS_METADATA_SCHEMA_VERSION;
use crate::{Digest, Platform};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ImageRecord {
    pub(super) schema_version: u32,
    pub(super) reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) source_manifest_digest: Option<String>,
    pub(super) manifest_digest: String,
    pub(super) platform: Platform,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct DefaultImage {
    pub(super) schema_version: u32,
    pub(super) reference: String,
}

#[derive(Debug)]
pub(super) struct StoredRecord {
    pub(super) path: PathBuf,
    pub(super) value: ImageRecord,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct StorageUsage {
    pub(super) logical_bytes: u64,
    pub(super) allocated_bytes: u64,
    pub(super) entries: u64,
}

impl StorageUsage {
    pub(super) fn add(&mut self, other: Self) {
        self.logical_bytes = self.logical_bytes.saturating_add(other.logical_bytes);
        self.allocated_bytes = self.allocated_bytes.saturating_add(other.allocated_bytes);
        self.entries = self.entries.saturating_add(other.entries);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct RootfsMetadata {
    pub(super) schema_version: u32,
    pub(super) manifest_digest: String,
    pub(super) logical_bytes: u64,
    pub(super) allocated_bytes: u64,
    pub(super) entries: u64,
}

impl RootfsMetadata {
    pub(super) fn new(digest: &Digest, usage: StorageUsage) -> Self {
        Self {
            schema_version: ROOTFS_METADATA_SCHEMA_VERSION,
            manifest_digest: digest.to_string(),
            logical_bytes: usage.logical_bytes,
            allocated_bytes: usage.allocated_bytes,
            entries: usage.entries,
        }
    }

    pub(super) fn usage(&self) -> StorageUsage {
        StorageUsage {
            logical_bytes: self.logical_bytes,
            allocated_bytes: self.allocated_bytes,
            entries: self.entries,
        }
    }
}

#[derive(Debug)]
pub(super) enum RootfsMetadataState {
    Missing,
    Invalid,
    Valid(RootfsMetadata),
}

impl RootfsMetadataState {
    pub(super) fn usage(&self) -> Option<StorageUsage> {
        match self {
            Self::Valid(metadata) => Some(metadata.usage()),
            Self::Missing | Self::Invalid => None,
        }
    }
}

#[derive(Debug)]
pub(super) struct RootfsEntry {
    pub(super) digest: Digest,
    pub(super) path: PathBuf,
    pub(super) ready: bool,
    pub(super) valid_digest_name: bool,
    pub(super) metadata: RootfsMetadataState,
}
