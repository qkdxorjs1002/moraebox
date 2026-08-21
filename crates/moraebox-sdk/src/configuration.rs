use std::path::{Path, PathBuf};

use moraebox_box::{BaseDiskStore, BoxStore, BoxStoreError, EphemeralDiskStore};
use moraebox_image::{ImageCache, Platform, PreparedImage, WorkspaceError, digest_tree};
use moraebox_runtime::{
    BoxRootSource, BoxRuntimeConfig, DiskToolPaths, LibkrunConfig, NativeRuntimePaths,
};
use thiserror::Error;

#[derive(Clone)]
pub struct ManagedStorage {
    cache_dir: PathBuf,
    state_dir: PathBuf,
    images: ImageCache,
    boxes: BoxStore,
    base_disks: BaseDiskStore,
    ephemeral_disks: EphemeralDiskStore,
}

impl ManagedStorage {
    pub fn open(
        cache_dir: impl Into<PathBuf>,
        state_dir: impl Into<PathBuf>,
    ) -> Result<Self, BoxStoreError> {
        let cache_dir = cache_dir.into();
        let state_dir = state_dir.into();
        let storage = Self {
            images: ImageCache::new(&cache_dir),
            boxes: BoxStore::new(&state_dir),
            base_disks: BaseDiskStore::new(&cache_dir),
            ephemeral_disks: EphemeralDiskStore::new(cache_dir.join("runtime")),
            cache_dir,
            state_dir,
        };
        let _ = storage.boxes.garbage_collect()?;
        let _ = storage.base_disks.garbage_collect()?;
        let _ = storage.ephemeral_disks.garbage_collect()?;
        Ok(storage)
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn images(&self) -> &ImageCache {
        &self.images
    }

    pub fn boxes(&self) -> &BoxStore {
        &self.boxes
    }

    pub fn base_disks(&self) -> &BaseDiskStore {
        &self.base_disks
    }

    pub fn ephemeral_disks(&self) -> &EphemeralDiskStore {
        &self.ephemeral_disks
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeRuntimeOverrides {
    pub helper: Option<PathBuf>,
    pub libkrun: Option<PathBuf>,
    pub library_search_path: Option<PathBuf>,
    pub gvproxy: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct NativeSandboxConfig {
    native_paths: NativeRuntimePaths,
    disk_tools: DiskToolPaths,
    disk_size: u64,
    vcpus: u8,
    memory_mib: u32,
}

impl NativeSandboxConfig {
    pub fn discover(
        overrides: NativeRuntimeOverrides,
        disk_tools: DiskToolPaths,
        disk_size: u64,
        vcpus: u8,
        memory_mib: u32,
    ) -> Self {
        Self::from_resolved(
            NativeRuntimePaths::discover_with_gvproxy(
                overrides.helper,
                overrides.libkrun,
                overrides.library_search_path,
                overrides.gvproxy,
            ),
            disk_tools,
            disk_size,
            vcpus,
            memory_mib,
        )
    }

    pub fn from_resolved(
        native_paths: NativeRuntimePaths,
        disk_tools: DiskToolPaths,
        disk_size: u64,
        vcpus: u8,
        memory_mib: u32,
    ) -> Self {
        Self {
            native_paths,
            disk_tools,
            disk_size,
            vcpus,
            memory_mib,
        }
    }

    pub fn disk_tools(&self) -> &DiskToolPaths {
        &self.disk_tools
    }

    pub fn prepared_image_source(
        &self,
        prepared: PreparedImage,
        platform: &Platform,
    ) -> BoxRootSource {
        self.root_source(
            prepared.rootfs,
            prepared.manifest_digest,
            platform_name(platform),
        )
    }

    pub fn rootfs_source(
        &self,
        rootfs: impl Into<PathBuf>,
        platform: &Platform,
    ) -> Result<BoxRootSource, NativeConfigurationError> {
        let rootfs = rootfs.into();
        let digest = digest_tree(&rootfs)?.to_string();
        Ok(self.root_source(rootfs, digest, platform_name(platform)))
    }

    pub fn libkrun_config(
        &self,
        root_path: Option<PathBuf>,
        storage: &ManagedStorage,
        workspace_disk: Option<PathBuf>,
    ) -> Result<LibkrunConfig, NativeConfigurationError> {
        let helper = self
            .native_paths
            .helper
            .clone()
            .ok_or(NativeConfigurationError::MissingHelper)?;
        let libkrun = self
            .native_paths
            .libkrun
            .clone()
            .ok_or(NativeConfigurationError::MissingLibkrun)?;
        let mut config = LibkrunConfig::new(helper, libkrun, root_path.unwrap_or_default());
        config
            .libkrunfw_path
            .clone_from(&self.native_paths.libkrunfw);
        config
            .library_search_path
            .clone_from(&self.native_paths.library_search_path);
        config.gvproxy_path.clone_from(&self.native_paths.gvproxy);
        config.debugfs_path = self.disk_tools.debugfs_command();
        config.network_runtime_dir = storage.cache_dir().join("network");
        config.workspace_disk = workspace_disk;
        config.vcpus = self.vcpus;
        config.memory_mib = self.memory_mib;
        Ok(config)
    }

    pub fn box_runtime(
        &self,
        storage: &ManagedStorage,
        source: Option<BoxRootSource>,
    ) -> BoxRuntimeConfig {
        BoxRuntimeConfig {
            boxes: storage.boxes().clone(),
            base_disks: storage.base_disks().clone(),
            ephemeral_disks: storage.ephemeral_disks().clone(),
            source,
            e2fsck_path: self.disk_tools.e2fsck_command(),
        }
    }

    fn root_source(
        &self,
        rootfs_path: PathBuf,
        manifest_digest: String,
        platform: String,
    ) -> BoxRootSource {
        BoxRootSource {
            rootfs_path,
            manifest_digest,
            platform,
            virtual_size_bytes: self.disk_size,
            mke2fs_path: self.disk_tools.mke2fs_command(),
        }
    }
}

#[derive(Debug, Error)]
pub enum NativeConfigurationError {
    #[error("libkrun backend requires --helper, MORAE_HELPER_PATH, or a sibling morae-vmm-helper")]
    MissingHelper,
    #[error(
        "libkrun backend requires --libkrun, MORAE_LIBKRUN_PATH, or a supported Homebrew libkrun"
    )]
    MissingLibkrun,
    #[error("failed to inspect the root filesystem: {0}")]
    Rootfs(#[from] WorkspaceError),
}

fn platform_name(platform: &Platform) -> String {
    match &platform.variant {
        Some(variant) => format!("{}/{}/{}", platform.os, platform.architecture, variant),
        None => format!("{}/{}", platform.os, platform.architecture),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved_paths() -> NativeRuntimePaths {
        NativeRuntimePaths {
            helper: Some("/native/morae-vmm-helper".into()),
            libkrun: Some("/native/lib/libkrun.dylib".into()),
            libkrunfw: Some("/native/lib/libkrunfw.dylib".into()),
            gvproxy: Some("/native/gvproxy".into()),
            library_search_path: Some("/native/lib".into()),
        }
    }

    fn disk_tools() -> DiskToolPaths {
        DiskToolPaths {
            mke2fs: Some("/native/mke2fs".into()),
            e2fsck: Some("/native/e2fsck".into()),
            debugfs: Some("/native/debugfs".into()),
        }
    }

    #[test]
    fn managed_storage_uses_one_cache_and_state_layout() {
        let temporary = tempfile::tempdir().unwrap();
        let cache = temporary.path().join("cache");
        let state = temporary.path().join("state");
        let storage = ManagedStorage::open(&cache, &state).unwrap();

        assert_eq!(storage.cache_dir(), cache);
        assert_eq!(storage.state_dir(), state);
        assert_eq!(storage.boxes().state_root(), state);
    }

    #[test]
    fn native_config_applies_resolved_paths_compute_and_storage_defaults() {
        let temporary = tempfile::tempdir().unwrap();
        let storage = ManagedStorage::open(
            temporary.path().join("cache"),
            temporary.path().join("state"),
        )
        .unwrap();
        let builder = NativeSandboxConfig::from_resolved(
            resolved_paths(),
            disk_tools(),
            9 * 1024 * 1024,
            4,
            1024,
        );
        let config = builder
            .libkrun_config(Some("/rootfs".into()), &storage, Some("/workspace".into()))
            .unwrap();

        assert_eq!(
            config.libkrunfw_path,
            Some("/native/lib/libkrunfw.dylib".into())
        );
        assert_eq!(
            config.network_runtime_dir,
            storage.cache_dir().join("network")
        );
        assert_eq!(config.workspace_disk, Some("/workspace".into()));
        assert_eq!(config.debugfs_path, PathBuf::from("/native/debugfs"));
        assert_eq!((config.vcpus, config.memory_mib), (4, 1024));
    }

    #[test]
    fn prepared_and_rootfs_sources_share_disk_and_platform_settings() {
        let temporary = tempfile::tempdir().unwrap();
        let rootfs = temporary.path().join("rootfs");
        std::fs::create_dir(&rootfs).unwrap();
        std::fs::write(rootfs.join("payload"), b"payload").unwrap();
        let builder = NativeSandboxConfig::from_resolved(
            resolved_paths(),
            disk_tools(),
            9 * 1024 * 1024,
            2,
            512,
        );
        let platform = Platform::host_linux();
        let from_rootfs = builder.rootfs_source(&rootfs, &platform).unwrap();
        let from_image = builder.prepared_image_source(
            PreparedImage {
                reference: "example.test/image:latest".into(),
                manifest_digest: "sha256:prepared".into(),
                rootfs,
            },
            &platform,
        );

        for source in [&from_rootfs, &from_image] {
            assert_eq!(source.virtual_size_bytes, 9 * 1024 * 1024);
            assert_eq!(source.mke2fs_path, PathBuf::from("/native/mke2fs"));
            assert!(source.platform.starts_with("linux/"));
        }
        assert_eq!(from_image.manifest_digest, "sha256:prepared");
    }
}
