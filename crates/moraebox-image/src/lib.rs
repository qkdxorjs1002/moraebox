//! OCI acquisition, digest-addressed storage, safe layer application, and host snapshots.

#![forbid(unsafe_code)]

mod cas;
mod layer;
mod reference;
mod registry;
mod workspace;

pub use cas::{Cas, Digest};
pub use layer::{LayerCompression, apply_layer};
pub use reference::{ImageReference, RegistryReference};
pub use registry::{
    Credentials, ImageManifest, Platform, PulledImage, RegistryClient, RegistryError,
};
pub use workspace::{WorkspaceError, WorkspaceSnapshot, digest_tree};
