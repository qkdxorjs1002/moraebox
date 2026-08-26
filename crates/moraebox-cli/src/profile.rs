use std::{
    collections::BTreeMap,
    fs, io,
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
};

use moraebox_core::{
    ImagePullPolicy, NetworkMode, NetworkPolicy, PublishProtocol, PublishRequest, RunSpec,
};
use serde::Deserialize;
use thiserror::Error;

use crate::{
    ResolvedRunArgs, RunArgs,
    commands::{parse_disk_size, parse_kill_grace, parse_output_limit, parse_timeout},
};

pub(super) const DEFAULT_PROFILE_FILE: &str = "morae.toml";
const PROFILE_SCHEMA_VERSION: u32 = 1;
const MAX_PROFILE_FILE_BYTES: u64 = 1024 * 1024;
const MAX_PROFILES: usize = 64;
const MAX_PROFILE_NAME_BYTES: usize = 64;
const MAX_ENV_VARS: usize = 64;
const MAX_ENV_NAME_BYTES: usize = 128;
const MAX_ENV_VALUE_BYTES: usize = 16 * 1024;
const MAX_ENV_TOTAL_BYTES: usize = 256 * 1024;

#[derive(Debug, Error)]
pub(super) enum ProfileError {
    #[error("failed to read profile file {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "profile file {} is {actual} bytes; the maximum is {MAX_PROFILE_FILE_BYTES} bytes",
        path.display()
    )]
    TooLarge { path: PathBuf, actual: u64 },
    #[error("failed to parse profile file {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("profile file {} uses unsupported version {version}; expected version 1", path.display())]
    UnsupportedVersion { path: PathBuf, version: u32 },
    #[error("profile file {} is invalid: {message}", path.display())]
    Invalid { path: PathBuf, message: String },
    #[error("profile `{name}` was not found in {}", path.display())]
    NotFound { path: PathBuf, name: String },
    #[error("failed to determine the current directory for {DEFAULT_PROFILE_FILE}: {source}")]
    CurrentDirectory {
        #[source]
        source: io::Error,
    },
}

#[derive(Debug, Clone)]
pub(super) struct LoadedProfiles {
    path: PathBuf,
    profiles: BTreeMap<String, RunProfile>,
}

impl LoadedProfiles {
    pub(super) fn load(path: impl AsRef<Path>) -> Result<Self, ProfileError> {
        let requested_path = path.as_ref();
        let metadata = fs::metadata(requested_path).map_err(|source| ProfileError::Read {
            path: requested_path.to_path_buf(),
            source,
        })?;
        if metadata.len() > MAX_PROFILE_FILE_BYTES {
            return Err(ProfileError::TooLarge {
                path: requested_path.to_path_buf(),
                actual: metadata.len(),
            });
        }

        let contents = fs::read_to_string(requested_path).map_err(|source| ProfileError::Read {
            path: requested_path.to_path_buf(),
            source,
        })?;
        if u64::try_from(contents.len()).unwrap_or(u64::MAX) > MAX_PROFILE_FILE_BYTES {
            return Err(ProfileError::TooLarge {
                path: requested_path.to_path_buf(),
                actual: u64::try_from(contents.len()).unwrap_or(u64::MAX),
            });
        }

        let path = fs::canonicalize(requested_path).map_err(|source| ProfileError::Read {
            path: requested_path.to_path_buf(),
            source,
        })?;
        let mut document: ProfileFileV1 =
            toml::from_str(&contents).map_err(|source| ProfileError::Parse {
                path: path.clone(),
                source,
            })?;
        if document.version != PROFILE_SCHEMA_VERSION {
            return Err(ProfileError::UnsupportedVersion {
                path,
                version: document.version,
            });
        }
        if document.profiles.len() > MAX_PROFILES {
            return Err(ProfileError::Invalid {
                path,
                message: format!("at most {MAX_PROFILES} profiles are supported"),
            });
        }

        let root = path
            .parent()
            .expect("a canonical file path always has a parent");
        for (name, profile) in &mut document.profiles {
            validate_profile_name(name).map_err(|message| ProfileError::Invalid {
                path: path.clone(),
                message,
            })?;
            profile
                .validate_and_resolve_workspace(root)
                .map_err(|message| ProfileError::Invalid {
                    path: path.clone(),
                    message: format!("profile `{name}`: {message}"),
                })?;
        }

        Ok(Self {
            path,
            profiles: document.profiles,
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn names(&self) -> impl Iterator<Item = &str> {
        self.profiles.keys().map(String::as_str)
    }

    pub(super) fn get(&self, name: &str) -> Result<&RunProfile, ProfileError> {
        self.profiles
            .get(name)
            .ok_or_else(|| ProfileError::NotFound {
                path: self.path.clone(),
                name: name.to_owned(),
            })
    }
}

pub(super) fn load_profiles(config: Option<&Path>) -> Result<LoadedProfiles, ProfileError> {
    let path = config.map_or_else(
        || {
            std::env::current_dir()
                .map(|directory| directory.join(DEFAULT_PROFILE_FILE))
                .map_err(|source| ProfileError::CurrentDirectory { source })
        },
        |path| Ok(path.to_path_buf()),
    )?;
    LoadedProfiles::load(path)
}

#[expect(
    clippy::too_many_lines,
    reason = "profile precedence is kept in one auditable resolution function"
)]
pub(super) fn resolve_run_args(args: RunArgs) -> Result<ResolvedRunArgs, ProfileError> {
    let selected = args
        .profile
        .as_deref()
        .map(|name| {
            let loaded = load_profiles(args.config.as_deref())?;
            Ok((loaded.path.clone(), loaded.get(name)?.clone()))
        })
        .transpose()?;
    let (profile_path, profile) = selected.map_or_else(
        || (None, RunProfile::default()),
        |(path, profile)| (Some(path), profile),
    );

    if args.rootfs.is_some() && (profile.image.is_some() || profile.box_id.is_some()) {
        return Err(ProfileError::Invalid {
            path: profile_path.expect("a populated profile has a source path"),
            message: "CLI --rootfs cannot be combined with a profile image or box".into(),
        });
    }

    let (image, box_id) = if args.image.is_some() {
        (args.image, None)
    } else if args.box_id.is_some() {
        (None, args.box_id)
    } else {
        (profile.image, profile.box_id)
    };
    let pull_policy = args.pull_policy.unwrap_or_else(|| {
        profile
            .pull_policy
            .as_deref()
            .map_or(ImagePullPolicy::Missing, |policy| {
                policy
                    .parse()
                    .expect("profile pull policy was validated while loading")
            })
    });
    let workspace = args.workspace.or(profile.workspace);
    let workspace_writable = if args.workspace_writable {
        true
    } else if args.workspace_read_only {
        false
    } else {
        profile.workspace_writable.unwrap_or(false)
    };
    let tty = if args.tty {
        true
    } else if args.no_tty {
        false
    } else {
        profile.tty.unwrap_or(false)
    };

    let mut env = profile.env;
    env.extend(args.env);

    let mut publish = profile
        .publish
        .iter()
        .map(ProfilePublish::request)
        .collect::<Vec<_>>();
    publish.extend(args.publish);
    let profile_mode = profile.network.as_ref().map(|network| network.mode);
    let mut allow_cidrs = profile
        .network
        .as_ref()
        .map_or_else(Vec::new, |network| network.allow_cidrs.clone());
    let mut allow_domains = profile
        .network
        .as_ref()
        .map_or_else(Vec::new, |network| network.allow_domains.clone());
    let network = if args.no_network {
        allow_cidrs.clear();
        allow_domains.clear();
        publish.clear();
        false
    } else if args.network {
        allow_cidrs.clear();
        allow_domains.clear();
        true
    } else if !args.allow_cidrs.is_empty() || !args.allow_domains.is_empty() {
        if profile_mode != Some(NetworkMode::Allowlist) {
            allow_cidrs.clear();
            allow_domains.clear();
        }
        allow_cidrs.extend(args.allow_cidrs);
        allow_domains.extend(args.allow_domains);
        false
    } else {
        profile_mode == Some(NetworkMode::Unrestricted)
    };

    Ok(ResolvedRunArgs {
        backend: args
            .backend
            .or(profile.backend)
            .unwrap_or_else(|| "libkrun".into()),
        rootfs: args.rootfs,
        image,
        pull_policy,
        box_id,
        cpus: args.cpus.or(profile.cpus).unwrap_or(2),
        memory_mib: args.memory_mib.or(profile.memory_mib).unwrap_or(512),
        workspace,
        workspace_writable,
        workspace_copy_out: args.workspace_copy_out,
        workspace_diff: args.workspace_diff,
        copy_in: args.copy_in,
        copy_out: args.copy_out,
        copy_limit: args.copy_limit,
        registry_username: args.registry_username,
        registry_password: args.registry_password,
        disk_size: args.disk_size.unwrap_or_else(|| {
            profile.disk_size.as_deref().map_or_else(
                || parse_disk_size("8GiB").expect("the default disk size is valid"),
                |value| parse_disk_size(value).expect("profile disk size was validated"),
            )
        }),
        timeout: args
            .timeout
            .or(profile.timeout)
            .unwrap_or_else(|| "1h".into()),
        output_limit: args.output_limit.unwrap_or_else(|| {
            profile.output_limit.as_deref().map_or_else(
                || parse_output_limit("64MiB").expect("the default output limit is valid"),
                |value| parse_output_limit(value).expect("profile output limit was validated"),
            )
        }),
        kill_grace: args.kill_grace.unwrap_or_else(|| {
            profile.kill_grace.as_deref().map_or_else(
                || parse_kill_grace("5s").expect("the default kill grace is valid"),
                |value| parse_kill_grace(value).expect("profile kill grace was validated"),
            )
        }),
        tty,
        interactive: args.interactive,
        inherit_env: args.inherit_env,
        network,
        allow_cidrs,
        allow_domains,
        publish,
        cwd: args.cwd.or(profile.cwd),
        env: env.into_iter().collect(),
        command: if args.command.is_empty() {
            profile.command.unwrap_or_default()
        } else {
            args.command
        },
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFileV1 {
    version: u32,
    profiles: BTreeMap<String, RunProfile>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RunProfile {
    #[serde(default)]
    pub(super) command: Option<Vec<String>>,
    #[serde(default)]
    pub(super) backend: Option<String>,
    #[serde(default)]
    pub(super) image: Option<String>,
    #[serde(default, rename = "box")]
    pub(super) box_id: Option<String>,
    #[serde(default, rename = "pull")]
    pub(super) pull_policy: Option<String>,
    #[serde(default)]
    pub(super) cpus: Option<u8>,
    #[serde(default)]
    pub(super) memory_mib: Option<u32>,
    #[serde(default)]
    pub(super) disk_size: Option<String>,
    #[serde(default)]
    pub(super) workspace: Option<PathBuf>,
    #[serde(default)]
    pub(super) workspace_writable: Option<bool>,
    #[serde(default)]
    pub(super) cwd: Option<PathBuf>,
    #[serde(default)]
    pub(super) env: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) timeout: Option<String>,
    #[serde(default)]
    pub(super) output_limit: Option<String>,
    #[serde(default)]
    pub(super) kill_grace: Option<String>,
    #[serde(default)]
    pub(super) tty: Option<bool>,
    #[serde(default)]
    pub(super) network: Option<ProfileNetwork>,
    #[serde(default)]
    pub(super) publish: Vec<ProfilePublish>,
}

impl RunProfile {
    fn validate_and_resolve_workspace(&mut self, root: &Path) -> Result<(), String> {
        self.validate_fields()?;
        self.resolve_workspace(root)
    }

    fn validate_fields(&self) -> Result<(), String> {
        if let Some(command) = &self.command {
            if command.first().is_none_or(String::is_empty) {
                return Err("command must contain a non-empty executable".into());
            }
            if command.iter().any(|argument| argument.contains('\0')) {
                return Err("command arguments must not contain NUL".into());
            }
        }
        if self
            .backend
            .as_deref()
            .is_some_and(|backend| !matches!(backend, "process" | "libkrun"))
        {
            return Err("backend must be `process` or `libkrun`".into());
        }
        if self.image.is_some() && self.box_id.is_some() {
            return Err("image and box are mutually exclusive".into());
        }
        if self.image.as_deref().is_some_and(str::is_empty)
            || self.box_id.as_deref().is_some_and(str::is_empty)
        {
            return Err("image and box values must not be empty".into());
        }
        if self.box_id.is_some() && self.workspace.is_some() {
            return Err("box and workspace are mutually exclusive".into());
        }
        if self.workspace.is_some() && self.cwd.is_some() {
            return Err("cwd and workspace cannot be combined in this version".into());
        }
        if self
            .pull_policy
            .as_deref()
            .is_some_and(|policy| policy.parse::<ImagePullPolicy>().is_err())
        {
            return Err("pull must be `missing`, `always`, or `never`".into());
        }
        if let Some(value) = &self.disk_size {
            parse_disk_size(value).map_err(|error| format!("disk_size: {error}"))?;
        }
        if let Some(value) = &self.timeout {
            parse_timeout(value).map_err(|error| format!("timeout: {error}"))?;
        }
        if let Some(value) = &self.output_limit {
            parse_output_limit(value).map_err(|error| format!("output_limit: {error}"))?;
        }
        if let Some(value) = &self.kill_grace {
            parse_kill_grace(value).map_err(|error| format!("kill_grace: {error}"))?;
        }
        if self.cpus == Some(0) {
            return Err("cpus must be greater than zero".into());
        }
        if self.memory_mib == Some(0) {
            return Err("memory_mib must be greater than zero".into());
        }
        if self
            .cwd
            .as_deref()
            .is_some_and(|cwd| !normalized_guest_path(cwd))
        {
            return Err("cwd must be a normalized absolute guest path".into());
        }
        validate_environment(&self.env)?;
        self.validate_network()?;
        if self.backend.as_deref() == Some("process") {
            let network_enabled = self
                .network
                .as_ref()
                .is_some_and(|network| network.mode != NetworkMode::Disabled)
                || !self.publish.is_empty();
            if self.image.is_some()
                || self.box_id.is_some()
                || self.workspace.is_some()
                || self.tty == Some(true)
                || network_enabled
            {
                return Err(
                    "process backend profiles cannot select VM images, Boxes, workspaces, TTY, networking, or previews"
                        .into(),
                );
            }
        }

        Ok(())
    }

    fn resolve_workspace(&mut self, root: &Path) -> Result<(), String> {
        if let Some(workspace) = &self.workspace {
            let candidate = if workspace.is_absolute() {
                workspace.clone()
            } else {
                root.join(workspace)
            };
            let resolved = fs::canonicalize(&candidate).map_err(|error| {
                format!(
                    "workspace {} could not be resolved: {error}",
                    candidate.display()
                )
            })?;
            if !resolved.starts_with(root) {
                return Err(format!(
                    "workspace {} escapes the profile directory {}",
                    resolved.display(),
                    root.display()
                ));
            }
            self.workspace = Some(resolved);
        }
        if self.workspace_writable.is_some() && self.workspace.is_none() {
            return Err("workspace_writable requires workspace".into());
        }
        Ok(())
    }

    fn validate_network(&self) -> Result<(), String> {
        let mut spec = RunSpec::command(["profile-validation"]);
        spec.publish = self.publish.iter().map(ProfilePublish::request).collect();
        if let Some(network) = &self.network {
            spec.network_policy = Some(NetworkPolicy {
                mode: network.mode,
                allow_cidrs: network.allow_cidrs.clone(),
                allow_domains: network.allow_domains.clone(),
            });
        } else if !spec.publish.is_empty() {
            spec.network_policy = Some(NetworkPolicy {
                mode: NetworkMode::Allowlist,
                allow_cidrs: Vec::new(),
                allow_domains: Vec::new(),
            });
        }
        spec.validate().map_err(str::to_owned)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProfileNetwork {
    pub(super) mode: NetworkMode,
    #[serde(default)]
    pub(super) allow_cidrs: Vec<String>,
    #[serde(default)]
    pub(super) allow_domains: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProfilePublish {
    #[serde(default)]
    pub(super) host_port: u16,
    pub(super) guest_port: u16,
}

impl ProfilePublish {
    pub(super) fn request(&self) -> PublishRequest {
        PublishRequest {
            protocol: PublishProtocol::Tcp,
            host_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            host_port: self.host_port,
            guest_port: self.guest_port,
        }
    }
}

fn validate_profile_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= MAX_PROFILE_NAME_BYTES
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "profile name `{name}` must match [A-Za-z0-9][A-Za-z0-9._-]* and be at most {MAX_PROFILE_NAME_BYTES} bytes"
        ))
    }
}

fn validate_environment(env: &BTreeMap<String, String>) -> Result<(), String> {
    if env.len() > MAX_ENV_VARS {
        return Err(format!("env supports at most {MAX_ENV_VARS} entries"));
    }
    let mut total_bytes = 0usize;
    for (name, value) in env {
        let mut characters = name.chars();
        let valid_name = name.len() <= MAX_ENV_NAME_BYTES
            && characters
                .next()
                .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
            && characters.all(|character| character.is_ascii_alphanumeric() || character == '_');
        if !valid_name {
            return Err(format!(
                "env name `{name}` must match [A-Za-z_][A-Za-z0-9_]* and be at most {MAX_ENV_NAME_BYTES} bytes"
            ));
        }
        if value.len() > MAX_ENV_VALUE_BYTES || value.contains('\0') {
            return Err(format!(
                "env `{name}` must be at most {MAX_ENV_VALUE_BYTES} bytes and NUL-free"
            ));
        }
        total_bytes = total_bytes
            .saturating_add(name.len())
            .saturating_add(value.len());
        if total_bytes > MAX_ENV_TOTAL_BYTES {
            return Err(format!(
                "env entries must total at most {MAX_ENV_TOTAL_BYTES} bytes"
            ));
        }
    }
    Ok(())
}

fn normalized_guest_path(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    path.components().all(|component| {
        matches!(
            component,
            std::path::Component::RootDir | std::path::Component::Normal(_)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command};
    use clap::Parser;

    fn write_profile(directory: &Path, body: &str) -> PathBuf {
        let path = directory.join(DEFAULT_PROFILE_FILE);
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn loads_and_normalizes_a_safe_profile() {
        let temporary = tempfile::tempdir().unwrap();
        let path = write_profile(
            temporary.path(),
            r#"
version = 1

[profiles.dev]
backend = "libkrun"
image = "python:3.12"
command = ["python", "-m", "app"]
cpus = 2
memory_mib = 1024
workspace = "."
timeout = "30m"
tty = true

[profiles.dev.env]
APP_ENV = "development"

[profiles.dev.network]
mode = "allowlist"
allow_domains = ["api.example.com"]

[[profiles.dev.publish]]
guest_port = 3000
host_port = 0
"#,
        );

        let loaded = LoadedProfiles::load(path).unwrap();
        let profile = loaded.get("dev").unwrap();
        assert_eq!(profile.command.as_deref().unwrap()[0], "python");
        assert_eq!(
            profile.workspace.as_deref(),
            Some(fs::canonicalize(temporary.path()).unwrap().as_path())
        );
        assert_eq!(profile.publish[0].request().host_port, 0);
        assert_eq!(loaded.names().collect::<Vec<_>>(), ["dev"]);
    }

    #[test]
    fn rejects_unknown_fields_versions_and_invalid_names() {
        let temporary = tempfile::tempdir().unwrap();
        let unknown = write_profile(
            temporary.path(),
            "version = 1\n[profiles.dev]\nshell = \"echo unsafe\"\n",
        );
        assert!(matches!(
            LoadedProfiles::load(unknown),
            Err(ProfileError::Parse { .. })
        ));

        let unsupported = write_profile(temporary.path(), "version = 2\n[profiles.dev]\n");
        assert!(matches!(
            LoadedProfiles::load(unsupported),
            Err(ProfileError::UnsupportedVersion { version: 2, .. })
        ));

        let invalid_name = write_profile(
            temporary.path(),
            "version = 1\n[profiles.\"bad name\"]\ncommand = [\"true\"]\n",
        );
        assert!(matches!(
            LoadedProfiles::load(invalid_name),
            Err(ProfileError::Invalid { .. })
        ));
    }

    #[test]
    fn rejects_workspace_traversal_and_symlink_escape() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let outside = root.path().join("outside");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&outside).unwrap();

        let traversal = write_profile(
            &project,
            "version = 1\n[profiles.dev]\nworkspace = \"../outside\"\n",
        );
        assert!(matches!(
            LoadedProfiles::load(traversal),
            Err(ProfileError::Invalid { .. })
        ));

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, project.join("linked")).unwrap();
            let symlink = write_profile(
                &project,
                "version = 1\n[profiles.dev]\nworkspace = \"linked\"\n",
            );
            assert!(matches!(
                LoadedProfiles::load(symlink),
                Err(ProfileError::Invalid { .. })
            ));
        }
    }

    #[test]
    fn rejects_oversized_files_and_environment() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join(DEFAULT_PROFILE_FILE);
        fs::write(
            &path,
            vec![b'x'; usize::try_from(MAX_PROFILE_FILE_BYTES).unwrap() + 1],
        )
        .unwrap();
        assert!(matches!(
            LoadedProfiles::load(&path),
            Err(ProfileError::TooLarge { .. })
        ));

        let invalid_env = write_profile(
            temporary.path(),
            "version = 1\n[profiles.dev.env]\n\"BAD-NAME\" = \"value\"\n",
        );
        assert!(matches!(
            LoadedProfiles::load(invalid_env),
            Err(ProfileError::Invalid { .. })
        ));
    }

    #[test]
    fn rejects_invalid_network_and_publish_values() {
        let temporary = tempfile::tempdir().unwrap();
        let invalid_domain = write_profile(
            temporary.path(),
            "version = 1\n[profiles.dev.network]\nmode = \"allowlist\"\nallow_domains = [\"*\"]\n",
        );
        assert!(matches!(
            LoadedProfiles::load(invalid_domain),
            Err(ProfileError::Invalid { .. })
        ));

        let invalid_port = write_profile(
            temporary.path(),
            "version = 1\n[[profiles.dev.publish]]\nguest_port = 0\n",
        );
        assert!(matches!(
            LoadedProfiles::load(invalid_port),
            Err(ProfileError::Invalid { .. })
        ));
    }

    #[test]
    fn rejects_cross_field_backend_and_workspace_conflicts() {
        let temporary = tempfile::tempdir().unwrap();
        let process_image = write_profile(
            temporary.path(),
            "version = 1\n[profiles.dev]\nbackend = \"process\"\nimage = \"python:3.12\"\n",
        );
        assert!(matches!(
            LoadedProfiles::load(process_image),
            Err(ProfileError::Invalid { .. })
        ));

        let workspace_cwd = write_profile(
            temporary.path(),
            "version = 1\n[profiles.dev]\nworkspace = \".\"\ncwd = \"/workspace\"\n",
        );
        assert!(matches!(
            LoadedProfiles::load(workspace_cwd),
            Err(ProfileError::Invalid { .. })
        ));
    }

    fn parse_run(
        arguments: impl IntoIterator<Item = impl Into<std::ffi::OsString> + Clone>,
    ) -> RunArgs {
        let cli = Cli::try_parse_from(arguments).unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };
        *args
    }

    #[test]
    fn preserves_existing_defaults_without_a_profile() {
        let resolved = resolve_run_args(parse_run(["morae", "run", "--", "true"])).unwrap();
        assert_eq!(resolved.backend, "libkrun");
        assert_eq!(resolved.pull_policy, ImagePullPolicy::Missing);
        assert_eq!((resolved.cpus, resolved.memory_mib), (2, 512));
        assert_eq!(resolved.disk_size, 8 * 1024 * 1024 * 1024);
        assert_eq!(resolved.timeout, "1h");
        assert_eq!(resolved.output_limit, 64 * 1024 * 1024);
        assert_eq!(resolved.kill_grace, std::time::Duration::from_secs(5));
        assert!(!resolved.tty);
        assert!(!resolved.network);
        assert_eq!(resolved.command, ["true"]);
    }

    #[test]
    fn merges_profile_values_below_explicit_cli_values() {
        let temporary = tempfile::tempdir().unwrap();
        let path = write_profile(
            temporary.path(),
            r#"
version = 1
[profiles.dev]
backend = "libkrun"
command = ["profile-command", "profile-arg"]
cpus = 4
memory_mib = 2048
timeout = "20m"
output_limit = "8MiB"
kill_grace = "10s"
tty = true

[profiles.dev.env]
PROFILE_ONLY = "yes"
SHARED = "profile"

[profiles.dev.network]
mode = "allowlist"
allow_domains = ["profile.example.com"]

[[profiles.dev.publish]]
guest_port = 3000
"#,
        );
        let path = path.to_string_lossy().into_owned();
        let args = parse_run([
            "morae",
            "run",
            "--config",
            &path,
            "--profile",
            "dev",
            "--cpus",
            "8",
            "--env",
            "SHARED=cli",
            "--allow-domain",
            "cli.example.com",
            "--publish",
            "8080:8080",
            "--no-tty",
            "--",
            "cli-command",
        ]);
        let resolved = resolve_run_args(args).unwrap();

        assert_eq!(resolved.command, ["cli-command"]);
        assert_eq!(resolved.cpus, 8);
        assert_eq!(resolved.memory_mib, 2048);
        assert!(!resolved.tty);
        assert_eq!(
            resolved.env,
            [
                ("PROFILE_ONLY".into(), "yes".into()),
                ("SHARED".into(), "cli".into())
            ]
        );
        assert_eq!(
            resolved.allow_domains,
            ["profile.example.com", "cli.example.com"]
        );
        assert_eq!(resolved.publish.len(), 2);
        assert_eq!(resolved.publish[0].host_port, 0);
        assert_eq!(resolved.publish[1].host_port, 8080);
    }

    #[test]
    fn explicit_network_modes_replace_profile_policy() {
        let temporary = tempfile::tempdir().unwrap();
        let path = write_profile(
            temporary.path(),
            r#"
version = 1
[profiles.dev]
command = ["true"]
[profiles.dev.network]
mode = "allowlist"
allow_domains = ["example.com"]
[[profiles.dev.publish]]
guest_port = 3000
"#,
        );
        let path = path.to_string_lossy().into_owned();

        let unrestricted = resolve_run_args(parse_run([
            "morae",
            "run",
            "--config",
            &path,
            "--profile",
            "dev",
            "--network",
        ]))
        .unwrap();
        assert!(unrestricted.network);
        assert!(unrestricted.allow_domains.is_empty());
        assert_eq!(unrestricted.publish.len(), 1);

        let disabled = resolve_run_args(parse_run([
            "morae",
            "run",
            "--config",
            &path,
            "--profile",
            "dev",
            "--no-network",
        ]))
        .unwrap();
        assert!(!disabled.network);
        assert!(disabled.allow_domains.is_empty());
        assert!(disabled.publish.is_empty());
    }

    #[test]
    fn rejects_cli_rootfs_with_a_profile_source() {
        let temporary = tempfile::tempdir().unwrap();
        let path = write_profile(
            temporary.path(),
            "version = 1\n[profiles.dev]\nimage = \"python:3.12\"\ncommand = [\"true\"]\n",
        );
        let path = path.to_string_lossy().into_owned();
        let error = resolve_run_args(parse_run([
            "morae",
            "run",
            "--config",
            &path,
            "--profile",
            "dev",
            "--rootfs",
            "/tmp/rootfs",
        ]))
        .unwrap_err();
        assert!(matches!(error, ProfileError::Invalid { .. }));
    }
}
