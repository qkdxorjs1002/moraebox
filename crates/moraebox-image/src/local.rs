use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write as _},
    path::{Component, Path, PathBuf},
    str::FromStr as _,
};

use futures_util::stream;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use tokio::io::AsyncReadExt as _;

use crate::{
    Cas, Digest, ImageManifest, ImagePullLimits, ImageReference, Platform, PulledImage,
    cas::{CasError, PutStreamError},
    registry::{Descriptor, RegistryError, platform_matches, validate_manifest_limits},
};

const OCI_LAYOUT_VERSION: &str = "1.0.0";
const MAX_LOCAL_ARCHIVE_ENTRIES: usize = 1_000_000;
const STREAM_BUFFER_SIZE: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ManifestEnvelope {
    Index(ImageIndex),
    Manifest(ImageManifest),
}

#[derive(Debug, Deserialize)]
struct ImageIndex {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    manifests: Vec<Descriptor>,
}

#[derive(Debug, Deserialize)]
struct OciLayoutVersion {
    #[serde(rename = "imageLayoutVersion")]
    image_layout_version: String,
}

#[derive(Debug, Deserialize)]
struct ImageConfigPlatform {
    architecture: String,
    os: String,
    #[serde(default)]
    variant: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DockerRootfsConfig {
    rootfs: DockerRootfs,
}

#[derive(Debug, Deserialize)]
struct DockerRootfs {
    diff_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DockerManifestEntry {
    config: String,
    #[serde(default)]
    repo_tags: Vec<String>,
    layers: Vec<String>,
}

struct DockerStaging {
    _directory: tempfile::TempDir,
    manifest_bytes: Vec<u8>,
    config: StagedBlob,
    layers: Vec<StagedBlob>,
}

struct StagedBlob {
    path: PathBuf,
    size: u64,
    digest: Digest,
}

pub(crate) async fn import_local_image(
    reference: &ImageReference,
    platform: &Platform,
    cas: &Cas,
) -> Result<PulledImage, LocalImageError> {
    let limits = ImagePullLimits::default();
    match reference {
        ImageReference::OciLayout(path) => {
            import_oci_layout(path, reference, platform, cas, limits).await
        }
        ImageReference::DockerArchive { path, selector } => {
            import_docker_archive(path, selector, reference, platform, cas, limits).await
        }
        ImageReference::Registry(_) => Err(LocalImageError::UnsupportedReference),
    }
}

async fn import_oci_layout(
    root: &Path,
    reference: &ImageReference,
    platform: &Platform,
    cas: &Cas,
    limits: ImagePullLimits,
) -> Result<PulledImage, LocalImageError> {
    ensure_real_directory(root)?;
    let blobs = root.join("blobs");
    let sha256 = blobs.join("sha256");
    ensure_real_directory(&blobs)?;
    ensure_real_directory(&sha256)?;
    let layout_bytes = read_bounded_regular_file(
        &root.join("oci-layout"),
        limits.max_manifest_bytes,
        "OCI layout version",
    )?;
    let layout: OciLayoutVersion = serde_json::from_slice(&layout_bytes)?;
    if layout.image_layout_version != OCI_LAYOUT_VERSION {
        return Err(LocalImageError::UnsupportedLayoutVersion(
            layout.image_layout_version,
        ));
    }
    let index_bytes = read_bounded_regular_file(
        &root.join("index.json"),
        limits.max_manifest_bytes,
        "OCI index",
    )?;
    let source_manifest_digest = Digest::from_bytes(&index_bytes);
    cas.put_verified(&source_manifest_digest, &index_bytes)
        .await?;
    let envelope: ManifestEnvelope = serde_json::from_slice(&index_bytes)?;
    let (manifest_digest, manifest) = match envelope {
        ManifestEnvelope::Index(index) => {
            if index.schema_version != 2 {
                return Err(LocalImageError::UnsupportedSchema(index.schema_version));
            }
            let mut descriptor_digests = BTreeSet::new();
            for descriptor in &index.manifests {
                if !descriptor_digests.insert(&descriptor.digest) {
                    return Err(LocalImageError::DuplicateDescriptor(
                        descriptor.digest.clone(),
                    ));
                }
            }
            let descriptor = select_platform_descriptor(&index.manifests, platform)?;
            let digest = import_oci_descriptor(
                &sha256,
                descriptor,
                limits.max_manifest_bytes,
                "selected manifest",
                cas,
            )
            .await?;
            let bytes = cas.read(&digest).await?;
            (digest, serde_json::from_slice(&bytes)?)
        }
        ManifestEnvelope::Manifest(manifest) => {
            let digest = source_manifest_digest.clone();
            (digest, manifest)
        }
    };
    validate_local_manifest(&manifest, &limits)?;
    let config_digest = import_oci_descriptor(
        &sha256,
        &manifest.config,
        limits.max_config_bytes,
        "configuration blob",
        cas,
    )
    .await?;
    validate_config_platform(&cas.read(&config_digest).await?, platform)?;
    let mut layer_digests = Vec::with_capacity(manifest.layers.len());
    let mut seen_layers = BTreeSet::new();
    for layer in &manifest.layers {
        if !seen_layers.insert(&layer.digest) {
            return Err(LocalImageError::DuplicateDescriptor(layer.digest.clone()));
        }
        layer_digests.push(
            import_oci_descriptor(&sha256, layer, limits.max_layer_bytes, "layer blob", cas)
                .await?,
        );
    }
    Ok(PulledImage {
        reference: reference.to_string(),
        source_manifest_digest,
        manifest_digest,
        manifest,
        config_digest,
        layer_digests,
        limits,
    })
}

async fn import_docker_archive(
    archive: &Path,
    selector: &str,
    reference: &ImageReference,
    platform: &Platform,
    cas: &Cas,
    limits: ImagePullLimits,
) -> Result<PulledImage, LocalImageError> {
    let archive = archive.to_path_buf();
    let selector = selector.to_owned();
    let staging_root = cas.root().join("tmp");
    fs::create_dir_all(&staging_root)?;
    let staging = tokio::task::spawn_blocking(move || {
        stage_docker_archive(&archive, &selector, &staging_root, &limits)
    })
    .await
    .map_err(|error| LocalImageError::Task(error.to_string()))??;

    let source_manifest_digest = Digest::from_bytes(&staging.manifest_bytes);
    cas.put_verified(&source_manifest_digest, &staging.manifest_bytes)
        .await?;
    let config_digest = import_staged_blob(&staging.config, limits.max_config_bytes, cas).await?;
    let config_bytes = cas.read(&config_digest).await?;
    validate_config_platform(&config_bytes, platform)?;
    let layer_digests = import_staged_layers(&staging.layers, &limits, cas).await?;
    validate_docker_diff_ids(&config_bytes, &layer_digests)?;
    let config = Descriptor {
        media_type: "application/vnd.docker.container.image.v1+json".into(),
        digest: config_digest.to_string(),
        size: staging.config.size,
        platform: None,
    };
    let layers = staging
        .layers
        .iter()
        .zip(&layer_digests)
        .map(|(layer, digest)| Descriptor {
            media_type: "application/vnd.docker.image.rootfs.diff.tar".into(),
            digest: digest.to_string(),
            size: layer.size,
            platform: None,
        })
        .collect::<Vec<_>>();
    let manifest = ImageManifest {
        schema_version: 2,
        media_type: Some("application/vnd.docker.distribution.manifest.v2+json".into()),
        config,
        layers,
    };
    validate_local_manifest(&manifest, &limits)?;
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    let manifest_digest = Digest::from_bytes(&manifest_bytes);
    cas.put_verified(&manifest_digest, &manifest_bytes).await?;
    Ok(PulledImage {
        reference: reference.to_string(),
        source_manifest_digest,
        manifest_digest,
        manifest,
        config_digest,
        layer_digests,
        limits,
    })
}

fn select_platform_descriptor<'a>(
    manifests: &'a [Descriptor],
    platform: &Platform,
) -> Result<&'a Descriptor, LocalImageError> {
    let matches = manifests
        .iter()
        .filter(|descriptor| platform_matches(descriptor.platform.as_ref(), platform))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [descriptor] => Ok(*descriptor),
        [] if manifests.len() == 1 && manifests[0].platform.is_none() => Ok(&manifests[0]),
        [] => Err(LocalImageError::PlatformNotFound(platform.clone())),
        _ => Err(LocalImageError::AmbiguousPlatform(platform.clone())),
    }
}

async fn import_oci_descriptor(
    blob_root: &Path,
    descriptor: &Descriptor,
    maximum: u64,
    resource: &'static str,
    cas: &Cas,
) -> Result<Digest, LocalImageError> {
    if descriptor.size > maximum {
        return Err(LocalImageError::SizeLimit {
            resource,
            actual: descriptor.size,
            maximum,
        });
    }
    let digest = Digest::from_str(&descriptor.digest)?;
    let path = blob_root.join(digest.hex());
    import_regular_file(&path, &digest, descriptor.size, maximum, cas).await
}

async fn import_staged_layers(
    layers: &[StagedBlob],
    limits: &ImagePullLimits,
    cas: &Cas,
) -> Result<Vec<Digest>, LocalImageError> {
    let mut compressed = 0_u64;
    let mut digests = Vec::with_capacity(layers.len());
    for layer in layers {
        compressed = compressed
            .checked_add(layer.size)
            .ok_or(LocalImageError::CompressedLimit(
                limits.max_compressed_bytes,
            ))?;
        if compressed > limits.max_compressed_bytes {
            return Err(LocalImageError::CompressedLimit(
                limits.max_compressed_bytes,
            ));
        }
        digests.push(import_staged_blob(layer, limits.max_layer_bytes, cas).await?);
    }
    Ok(digests)
}

async fn import_staged_blob(
    blob: &StagedBlob,
    maximum: u64,
    cas: &Cas,
) -> Result<Digest, LocalImageError> {
    import_regular_file(&blob.path, &blob.digest, blob.size, maximum, cas).await
}

async fn import_regular_file(
    path: &Path,
    expected: &Digest,
    expected_size: u64,
    maximum: u64,
    cas: &Cas,
) -> Result<Digest, LocalImageError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(LocalImageError::UnsafePath(path.into()));
    }
    if metadata.len() != expected_size {
        return Err(LocalImageError::DescriptorSize {
            digest: expected.to_string(),
            expected: expected_size,
            actual: metadata.len(),
        });
    }
    let file = tokio::fs::File::from_std(open_regular_file(path)?);
    let chunks = stream::try_unfold(file, |mut file| async move {
        let mut buffer = vec![0_u8; STREAM_BUFFER_SIZE];
        let count = file.read(&mut buffer).await?;
        if count == 0 {
            Ok::<_, io::Error>(None)
        } else {
            buffer.truncate(count);
            Ok(Some((buffer, file)))
        }
    });
    cas.put_stream(Some(expected), Some(expected_size), maximum, chunks)
        .await
        .map_err(|error| LocalImageError::from_put_stream(error, maximum, expected))
}

fn validate_local_manifest(
    manifest: &ImageManifest,
    limits: &ImagePullLimits,
) -> Result<(), LocalImageError> {
    if manifest.schema_version != 2 {
        return Err(LocalImageError::UnsupportedSchema(manifest.schema_version));
    }
    validate_manifest_limits(manifest, limits)?;
    Ok(())
}

fn validate_config_platform(bytes: &[u8], expected: &Platform) -> Result<(), LocalImageError> {
    let config: ImageConfigPlatform = serde_json::from_slice(bytes)?;
    let actual = Platform {
        os: config.os,
        architecture: config.architecture,
        variant: config.variant,
    };
    if actual.os != expected.os
        || actual.architecture != expected.architecture
        || expected
            .variant
            .as_ref()
            .is_some_and(|variant| actual.variant.as_ref() != Some(variant))
    {
        return Err(LocalImageError::ConfigPlatformMismatch {
            expected: Box::new(expected.clone()),
            actual: Box::new(actual),
        });
    }
    Ok(())
}

fn validate_docker_diff_ids(
    config: &[u8],
    layer_digests: &[Digest],
) -> Result<(), LocalImageError> {
    let config: DockerRootfsConfig = serde_json::from_slice(config)?;
    if config.rootfs.diff_ids.len() != layer_digests.len() {
        return Err(LocalImageError::DockerDiffIdCount {
            expected: config.rootfs.diff_ids.len(),
            actual: layer_digests.len(),
        });
    }
    for (index, (expected, actual)) in config.rootfs.diff_ids.iter().zip(layer_digests).enumerate()
    {
        let expected = Digest::from_str(expected)?;
        if expected != *actual {
            return Err(LocalImageError::DockerDiffIdMismatch {
                index,
                expected,
                actual: actual.clone(),
            });
        }
    }
    Ok(())
}

fn stage_docker_archive(
    archive_path: &Path,
    selector: &str,
    staging_root: &Path,
    limits: &ImagePullLimits,
) -> Result<DockerStaging, LocalImageError> {
    let mut archive = open_regular_file(archive_path)?;
    let metadata = archive.metadata()?;
    let maximum_archive = limits
        .max_compressed_bytes
        .saturating_add(limits.max_manifest_bytes)
        .saturating_add(limits.max_config_bytes);
    if metadata.len() > maximum_archive {
        return Err(LocalImageError::SizeLimit {
            resource: "Docker archive",
            actual: metadata.len(),
            maximum: maximum_archive,
        });
    }
    let manifest_bytes = scan_docker_manifest(&mut archive, limits.max_manifest_bytes)?;
    let manifests: Vec<DockerManifestEntry> = serde_json::from_slice(&manifest_bytes)?;
    let mut selected = manifests
        .iter()
        .filter(|entry| entry.repo_tags.iter().any(|tag| tag == selector));
    let entry = selected
        .next()
        .ok_or_else(|| LocalImageError::DockerSelectorNotFound(selector.into()))?;
    if selected.next().is_some() {
        return Err(LocalImageError::DuplicateDockerSelector(selector.into()));
    }
    if entry.layers.len() > limits.max_layers {
        return Err(LocalImageError::LayerCount {
            actual: entry.layers.len(),
            maximum: limits.max_layers,
        });
    }
    let config_name = validated_archive_path(Path::new(&entry.config))?;
    let mut selected_names = BTreeSet::new();
    if !selected_names.insert(config_name.clone()) {
        return Err(LocalImageError::DuplicateArchivePath(config_name));
    }
    let layer_names = entry
        .layers
        .iter()
        .map(|name| validated_archive_path(Path::new(name)))
        .collect::<Result<Vec<_>, _>>()?;
    for name in &layer_names {
        if !selected_names.insert(name.clone()) {
            return Err(LocalImageError::DuplicateArchivePath(name.clone()));
        }
    }
    let directory = tempfile::Builder::new()
        .prefix("local-docker-")
        .tempdir_in(staging_root)?;
    let staged = extract_selected_docker_entries(
        &mut archive,
        &config_name,
        &layer_names,
        directory.path(),
        limits,
    )?;
    Ok(DockerStaging {
        _directory: directory,
        manifest_bytes,
        config: staged.0,
        layers: staged.1,
    })
}

fn scan_docker_manifest(archive_file: &mut File, maximum: u64) -> Result<Vec<u8>, LocalImageError> {
    archive_file.seek(SeekFrom::Start(0))?;
    let mut archive = tar::Archive::new(archive_file);
    let mut seen = BTreeSet::new();
    let mut manifest = None;
    let mut count = 0_usize;
    for entry in archive.entries()? {
        count += 1;
        if count > MAX_LOCAL_ARCHIVE_ENTRIES {
            return Err(LocalImageError::TooManyArchiveEntries);
        }
        let mut entry = entry?;
        let path = validated_archive_path(&entry.path()?)?;
        if !seen.insert(path.clone()) {
            return Err(LocalImageError::DuplicateArchivePath(path));
        }
        validate_docker_entry_type(entry.header().entry_type(), &path)?;
        if path == Path::new("manifest.json") {
            let size = entry.header().size()?;
            if size > maximum {
                return Err(LocalImageError::SizeLimit {
                    resource: "Docker manifest",
                    actual: size,
                    maximum,
                });
            }
            let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
            entry.read_to_end(&mut bytes)?;
            manifest = Some(bytes);
        }
    }
    manifest.ok_or(LocalImageError::MissingDockerManifest)
}

fn extract_selected_docker_entries(
    archive_file: &mut File,
    config_name: &Path,
    layer_names: &[PathBuf],
    staging: &Path,
    limits: &ImagePullLimits,
) -> Result<(StagedBlob, Vec<StagedBlob>), LocalImageError> {
    let layer_indexes = layer_names
        .iter()
        .enumerate()
        .map(|(index, path)| (path.clone(), index))
        .collect::<BTreeMap<_, _>>();
    let mut config = None;
    let mut layers = (0..layer_names.len()).map(|_| None).collect::<Vec<_>>();
    archive_file.seek(SeekFrom::Start(0))?;
    let mut archive = tar::Archive::new(archive_file);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = validated_archive_path(&entry.path()?)?;
        if path != config_name && !layer_indexes.contains_key(&path) {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err(LocalImageError::UnsafeArchiveEntry(path));
        }
        let maximum = if path == config_name {
            limits.max_config_bytes
        } else {
            limits.max_layer_bytes
        };
        let output_path = if path == config_name {
            staging.join("config")
        } else {
            staging.join(format!("layer-{}", layer_indexes[&path]))
        };
        let blob = stage_tar_entry(&mut entry, &output_path, maximum)?;
        if path == config_name {
            config = Some(blob);
        } else {
            let index = layer_indexes[&path];
            layers[index] = Some(blob);
        }
    }
    let config = config.ok_or_else(|| LocalImageError::MissingArchiveEntry(config_name.into()))?;
    let layers = layers
        .into_iter()
        .zip(layer_names)
        .map(|(layer, name)| {
            layer.ok_or_else(|| LocalImageError::MissingArchiveEntry(name.clone()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((config, layers))
}

fn stage_tar_entry<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    path: &Path,
    maximum: u64,
) -> Result<StagedBlob, LocalImageError> {
    let size = entry.header().size()?;
    if size > maximum {
        return Err(LocalImageError::SizeLimit {
            resource: "Docker archive blob",
            actual: size,
            maximum,
        });
    }
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let mut hasher = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = vec![0_u8; STREAM_BUFFER_SIZE];
    loop {
        let count = entry.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        copied = copied.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        if copied > size || copied > maximum {
            return Err(LocalImageError::SizeLimit {
                resource: "Docker archive blob",
                actual: copied,
                maximum,
            });
        }
        hasher.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
    }
    if copied != size {
        return Err(LocalImageError::DescriptorSize {
            digest: path.display().to_string(),
            expected: size,
            actual: copied,
        });
    }
    output.sync_all()?;
    Ok(StagedBlob {
        path: path.to_path_buf(),
        size,
        digest: Digest::from_sha256(hasher.finalize().into()),
    })
}

fn read_bounded_regular_file(
    path: &Path,
    maximum: u64,
    resource: &'static str,
) -> Result<Vec<u8>, LocalImageError> {
    let mut file = open_regular_file(path)?;
    let metadata = file.metadata()?;
    if metadata.len() > maximum {
        return Err(LocalImageError::SizeLimit {
            resource,
            actual: metadata.len(),
            maximum,
        });
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    Read::by_ref(&mut file)
        .take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > maximum {
        return Err(LocalImageError::SizeLimit {
            resource,
            actual: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            maximum,
        });
    }
    Ok(bytes)
}

#[cfg(unix)]
fn open_regular_file(path: &Path) -> Result<File, LocalImageError> {
    let descriptor = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let file = File::from(descriptor);
    if !file.metadata()?.is_file() {
        return Err(LocalImageError::UnsafePath(path.into()));
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open_regular_file(path: &Path) -> Result<File, LocalImageError> {
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(LocalImageError::UnsafePath(path.into()));
    }
    Ok(file)
}

fn ensure_real_directory(path: &Path) -> Result<(), LocalImageError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(LocalImageError::UnsafePath(path.into()));
    }
    Ok(())
}

fn validated_archive_path(path: &Path) -> Result<PathBuf, LocalImageError> {
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => safe.push(value),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(LocalImageError::UnsafeArchiveEntry(path.into()));
            }
        }
    }
    if safe.as_os_str().is_empty() {
        return Err(LocalImageError::UnsafeArchiveEntry(path.into()));
    }
    Ok(safe)
}

fn validate_docker_entry_type(kind: tar::EntryType, path: &Path) -> Result<(), LocalImageError> {
    if kind.is_file() || kind.is_dir() {
        Ok(())
    } else {
        Err(LocalImageError::UnsafeArchiveEntry(path.into()))
    }
}

impl LocalImageError {
    fn from_put_stream(
        error: PutStreamError<io::Error>,
        maximum: u64,
        expected_digest: &Digest,
    ) -> Self {
        match error {
            PutStreamError::Source(error) => Self::Io(error),
            PutStreamError::Cas(error) => Self::Cas(error),
            PutStreamError::SizeExceeded { actual } => Self::SizeLimit {
                resource: "local image blob",
                actual,
                maximum,
            },
            PutStreamError::SizeMismatch { expected, actual } => Self::DescriptorSize {
                digest: expected_digest.to_string(),
                expected,
                actual,
            },
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum LocalImageError {
    #[error("local importer received a registry reference")]
    UnsupportedReference,
    #[error("unsupported OCI layout version {0}")]
    UnsupportedLayoutVersion(String),
    #[error("unsupported OCI schema version {0}")]
    UnsupportedSchema(u32),
    #[error("no local image exists for platform {0:?}")]
    PlatformNotFound(Platform),
    #[error("multiple local images match platform {0:?}")]
    AmbiguousPlatform(Platform),
    #[error("image configuration platform mismatch: expected {expected:?}, got {actual:?}")]
    ConfigPlatformMismatch {
        expected: Box<Platform>,
        actual: Box<Platform>,
    },
    #[error("duplicate OCI descriptor {0}")]
    DuplicateDescriptor(String),
    #[error("Docker archive selector was not found: {0}")]
    DockerSelectorNotFound(String),
    #[error("Docker archive selector appears more than once: {0}")]
    DuplicateDockerSelector(String),
    #[error("Docker archive is missing manifest.json")]
    MissingDockerManifest,
    #[error("Docker archive is missing selected entry {}", .0.display())]
    MissingArchiveEntry(PathBuf),
    #[error("Docker config declares {expected} diff IDs for {actual} layers")]
    DockerDiffIdCount { expected: usize, actual: usize },
    #[error("Docker layer {index} diff ID mismatch: expected {expected}, got {actual}")]
    DockerDiffIdMismatch {
        index: usize,
        expected: Digest,
        actual: Digest,
    },
    #[error("Docker archive contains duplicate path {}", .0.display())]
    DuplicateArchivePath(PathBuf),
    #[error("local archive contains too many entries")]
    TooManyArchiveEntries,
    #[error("unsafe local image path {}", .0.display())]
    UnsafePath(PathBuf),
    #[error("unsafe local archive entry {}", .0.display())]
    UnsafeArchiveEntry(PathBuf),
    #[error("{resource} is {actual} bytes, exceeding the {maximum}-byte limit")]
    SizeLimit {
        resource: &'static str,
        actual: u64,
        maximum: u64,
    },
    #[error("descriptor {digest} size mismatch: expected {expected}, got {actual}")]
    DescriptorSize {
        digest: String,
        expected: u64,
        actual: u64,
    },
    #[error("image has {actual} layers, exceeding the limit of {maximum}")]
    LayerCount { actual: usize, maximum: usize },
    #[error("compressed image content exceeds the {0}-byte limit")]
    CompressedLimit(u64),
    #[error("local image JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Cas(#[from] CasError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error("local image I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("local image background task failed: {0}")]
    Task(String),
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use moraebox_core::ImagePullPolicy;
    use proptest::prelude::*;
    use serde_json::json;

    use super::*;
    use crate::ImageCache;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn property_normalized_archive_paths_are_preserved(
            components in prop::collection::vec("[A-Za-z0-9_][A-Za-z0-9_.-]{0,15}", 1..8)
        ) {
            let path = components.join("/");
            prop_assert_eq!(validated_archive_path(Path::new(&path)).unwrap(), PathBuf::from(path));
        }

        #[test]
        fn property_archive_path_validation_never_panics(value in ".{0,512}") {
            let _ = validated_archive_path(Path::new(&value));
        }
    }

    fn platform(architecture: &str) -> Platform {
        Platform {
            os: "linux".into(),
            architecture: architecture.into(),
            variant: None,
        }
    }

    fn tar_layer(path: &str, contents: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_size(u64::try_from(contents.len()).unwrap());
            header.set_cksum();
            builder
                .append_data(&mut header, path, Cursor::new(contents))
                .unwrap();
            builder.finish().unwrap();
        }
        bytes
    }

    fn write_oci_blob(root: &Path, bytes: &[u8], media_type: &str) -> Descriptor {
        let digest = Digest::from_bytes(bytes);
        fs::write(root.join("blobs/sha256").join(digest.hex()), bytes).unwrap();
        Descriptor {
            media_type: media_type.into(),
            digest: digest.to_string(),
            size: u64::try_from(bytes.len()).unwrap(),
            platform: None,
        }
    }

    fn create_multi_platform_layout(root: &Path) {
        fs::create_dir_all(root.join("blobs/sha256")).unwrap();
        fs::write(
            root.join("oci-layout"),
            br#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();
        let manifests = ["amd64", "arm64"]
            .into_iter()
            .map(|architecture| {
                let config = serde_json::to_vec(&json!({
                    "os": "linux",
                    "architecture": architecture
                }))
                .unwrap();
                let config =
                    write_oci_blob(root, &config, "application/vnd.oci.image.config.v1+json");
                let layer = tar_layer("architecture", architecture.as_bytes());
                let layer = write_oci_blob(root, &layer, "application/vnd.oci.image.layer.v1.tar");
                let manifest = serde_json::to_vec(&json!({
                    "schemaVersion": 2,
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "config": config,
                    "layers": [layer]
                }))
                .unwrap();
                let mut descriptor = write_oci_blob(
                    root,
                    &manifest,
                    "application/vnd.oci.image.manifest.v1+json",
                );
                descriptor.platform = Some(platform(architecture));
                descriptor
            })
            .collect::<Vec<_>>();
        fs::write(
            root.join("index.json"),
            serde_json::to_vec(&json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.index.v1+json",
                "manifests": manifests
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn append_tar_file(builder: &mut tar::Builder<&mut File>, path: &str, bytes: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(u64::try_from(bytes.len()).unwrap());
        header.set_cksum();
        builder
            .append_data(&mut header, path, Cursor::new(bytes))
            .unwrap();
    }

    fn create_docker_archive(path: &Path) {
        let first_layer = tar_layer("selected", b"first");
        let second_layer = tar_layer("selected", b"second");
        let first_config = serde_json::to_vec(&json!({
            "os":"linux","architecture":"arm64",
            "rootfs":{"diff_ids":[Digest::from_bytes(&first_layer).to_string()]}
        }))
        .unwrap();
        let second_config = serde_json::to_vec(&json!({
            "os":"linux","architecture":"arm64",
            "rootfs":{"diff_ids":[Digest::from_bytes(&second_layer).to_string()]}
        }))
        .unwrap();
        let manifest = serde_json::to_vec(&json!([
            {"Config":"first.json","RepoTags":["repo/app:v1"],"Layers":["first/layer.tar"]},
            {"Config":"second.json","RepoTags":["repo/app:v2"],"Layers":["second/layer.tar"]}
        ]))
        .unwrap();
        let mut file = File::create(path).unwrap();
        let mut builder = tar::Builder::new(&mut file);
        append_tar_file(&mut builder, "manifest.json", &manifest);
        append_tar_file(&mut builder, "first.json", &first_config);
        append_tar_file(&mut builder, "first/layer.tar", &first_layer);
        append_tar_file(&mut builder, "second.json", &second_config);
        append_tar_file(&mut builder, "second/layer.tar", &second_layer);
        builder.finish().unwrap();
    }

    #[tokio::test]
    async fn imports_a_multi_platform_oci_layout_through_the_cache_pipeline() {
        let state = tempfile::tempdir().unwrap();
        let layout = state.path().join("layout");
        create_multi_platform_layout(&layout);
        let cache = ImageCache::new(state.path().join("cache"));
        let reference = format!("oci-layout:{}", layout.display());

        let image = cache
            .prepare(
                &reference,
                &platform("arm64"),
                None,
                ImagePullPolicy::Missing,
            )
            .await
            .unwrap();

        assert_eq!(
            fs::read(image.rootfs.join("architecture")).unwrap(),
            b"arm64"
        );
        assert!(
            cache
                .prepare(&reference, &platform("arm64"), None, ImagePullPolicy::Never,)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn rejects_corrupt_or_missing_oci_blobs() {
        for mutation in ["corrupt", "missing"] {
            let state = tempfile::tempdir().unwrap();
            let layout = state.path().join("layout");
            create_multi_platform_layout(&layout);
            let index: serde_json::Value =
                serde_json::from_slice(&fs::read(layout.join("index.json")).unwrap()).unwrap();
            let digest = index["manifests"][1]["digest"]
                .as_str()
                .unwrap()
                .trim_start_matches("sha256:");
            let blob = layout.join("blobs/sha256").join(digest);
            if mutation == "corrupt" {
                fs::write(blob, b"corrupt").unwrap();
            } else {
                fs::remove_file(blob).unwrap();
            }
            let cache = ImageCache::new(state.path().join("cache"));
            let reference = format!("oci-layout:{}", layout.display());
            assert!(
                cache
                    .prepare(
                        &reference,
                        &platform("arm64"),
                        None,
                        ImagePullPolicy::Missing,
                    )
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn docker_archive_requires_and_honors_the_explicit_selector() {
        let state = tempfile::tempdir().unwrap();
        let archive = state.path().join("images.tar");
        create_docker_archive(&archive);
        let cache = ImageCache::new(state.path().join("cache"));
        let reference = format!("docker-archive:{}#repo/app:v2", archive.display());

        let image = cache
            .prepare(
                &reference,
                &platform("arm64"),
                None,
                ImagePullPolicy::Missing,
            )
            .await
            .unwrap();

        assert_eq!(fs::read(image.rootfs.join("selected")).unwrap(), b"second");
        assert!(
            cache
                .prepare(
                    &format!("docker-archive:{}#repo/app:missing", archive.display()),
                    &platform("arm64"),
                    None,
                    ImagePullPolicy::Missing,
                )
                .await
                .is_err()
        );
    }

    #[test]
    fn docker_config_diff_ids_are_verified_against_layer_content() {
        let layer = Digest::from_bytes(b"layer");
        let valid = serde_json::to_vec(&json!({
            "rootfs":{"diff_ids":[layer.to_string()]}
        }))
        .unwrap();
        validate_docker_diff_ids(&valid, std::slice::from_ref(&layer)).unwrap();
        let invalid = serde_json::to_vec(&json!({
            "rootfs":{"diff_ids":[Digest::from_bytes(b"other").to_string()]}
        }))
        .unwrap();
        assert!(validate_docker_diff_ids(&invalid, &[layer]).is_err());
    }

    #[tokio::test]
    async fn docker_archive_rejects_duplicate_and_link_entries() {
        assert!(validated_archive_path(Path::new("../escape")).is_err());
        assert!(validated_archive_path(Path::new("/absolute")).is_err());
        for unsafe_kind in ["duplicate", "symlink", "device"] {
            let state = tempfile::tempdir().unwrap();
            let archive_path = state.path().join("unsafe.tar");
            let layer = tar_layer("value", b"value");
            let config = serde_json::to_vec(&json!({
                "os":"linux","architecture":"arm64",
                "rootfs":{"diff_ids":[Digest::from_bytes(&layer).to_string()]}
            }))
            .unwrap();
            let manifest = serde_json::to_vec(&json!([{
                "Config":"config.json","RepoTags":["repo/app:v1"],"Layers":["layer.tar"]
            }]))
            .unwrap();
            let mut file = File::create(&archive_path).unwrap();
            let mut builder = tar::Builder::new(&mut file);
            append_tar_file(&mut builder, "manifest.json", &manifest);
            append_tar_file(&mut builder, "config.json", &config);
            append_tar_file(&mut builder, "layer.tar", &layer);
            if unsafe_kind == "duplicate" {
                append_tar_file(&mut builder, "layer.tar", &layer);
            } else if unsafe_kind == "symlink" {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_mode(0o777);
                header.set_size(0);
                header.set_link_name("outside").unwrap();
                header.set_cksum();
                builder
                    .append_data(&mut header, "link", io::empty())
                    .unwrap();
            } else {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Char);
                header.set_mode(0o600);
                header.set_size(0);
                header.set_cksum();
                builder
                    .append_data(&mut header, "device", io::empty())
                    .unwrap();
            }
            builder.finish().unwrap();
            drop(builder);
            drop(file);
            let cache = ImageCache::new(state.path().join("cache"));
            let reference = format!("docker-archive:{}#repo/app:v1", archive_path.display());
            assert!(
                cache
                    .prepare(
                        &reference,
                        &platform("arm64"),
                        None,
                        ImagePullPolicy::Missing,
                    )
                    .await
                    .is_err()
            );
        }
    }
}
