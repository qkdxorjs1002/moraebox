//! OCI acquisition, digest-addressed storage, safe layer application, and host snapshots.

#![forbid(unsafe_code)]

mod cache;
mod cas;
mod durability;
mod layer;
mod lock;
mod reference;
mod registry;
mod workspace;

pub use cache::{
    BUILTIN_DEFAULT_IMAGE, CacheLock, CacheReconcileReport, CacheUsage, CachedImage, CleanReport,
    ImageCache, ImageCacheError, ImageProgressStage, PreparedImage, PruneReport, RemoveReport,
    RootfsMetadataIssue, RootfsMetadataIssueKind,
};
pub use cas::{Cas, Digest};
pub use layer::{LayerCompression, LayerLimits, apply_layer};
pub use reference::{ImageReference, RegistryReference};
pub use registry::{
    Credentials, ImageManifest, ImagePullLimits, Platform, PulledImage, RegistryClient,
    RegistryError,
};
pub use workspace::{WorkspaceError, WorkspaceSnapshot, WorkspaceStage, digest_tree};
