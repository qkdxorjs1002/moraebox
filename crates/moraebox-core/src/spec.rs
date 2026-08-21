use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60 * 60);
pub const DEFAULT_KILL_GRACE: Duration = Duration::from_secs(5);
pub const DEFAULT_OUTPUT_LIMIT: usize = 64 * 1024 * 1024;
pub const MAX_KILL_GRACE: Duration = Duration::from_secs(60);
pub const MAX_OUTPUT_LIMIT: usize = 1024 * 1024 * 1024;
pub const DEFAULT_TTY_ROWS: u16 = 24;
pub const DEFAULT_TTY_COLUMNS: u16 = 80;
pub const DEFAULT_COPY_LIMIT: u64 = 64 * 1024 * 1024;
pub const MAX_COPY_LIMIT: u64 = 1024 * 1024 * 1024;
pub const WORKSPACE_DIFF_GUEST_PATH: &str = "/run/moraebox-workspace/diff.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::str::FromStr for SessionId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

/// Identifies a persistent root filesystem lineage across otherwise independent runs.
///
/// A `BoxId` is not a VM identity or an authentication token. Every run still receives a new
/// [`SessionId`] and a fresh backend instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BoxId(Uuid);

impl BoxId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for BoxId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for BoxId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::str::FromStr for BoxId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "milliseconds", rename_all = "snake_case")]
pub enum TimeoutPolicy {
    Limited(u64),
    Unlimited,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImagePullPolicy {
    #[default]
    Missing,
    Always,
    Never,
}

impl std::fmt::Display for ImagePullPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Missing => "missing",
            Self::Always => "always",
            Self::Never => "never",
        })
    }
}

impl std::str::FromStr for ImagePullPolicy {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "missing" => Ok(Self::Missing),
            "always" => Ok(Self::Always),
            "never" => Ok(Self::Never),
            _ => Err("pull policy must be one of: missing, always, never"),
        }
    }
}

impl TimeoutPolicy {
    pub fn duration(self) -> Option<Duration> {
        match self {
            Self::Limited(milliseconds) => Some(Duration::from_millis(milliseconds)),
            Self::Unlimited => None,
        }
    }
}

impl Default for TimeoutPolicy {
    fn default() -> Self {
        Self::Limited(3_600_000)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Signal {
    Interrupt,
    Terminate,
    Kill,
    Hangup,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode {
    #[default]
    ReadOnly,
    Overlay,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyInSpec {
    pub source: PathBuf,
    pub destination: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyOutSpec {
    pub source: String,
    pub destination: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSpec {
    pub session_id: SessionId,
    #[serde(default)]
    pub box_id: Option<BoxId>,
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub inherit_env: bool,
    #[serde(default)]
    pub network: bool,
    #[serde(default)]
    pub image_pull_policy: ImagePullPolicy,
    pub stdin: Vec<u8>,
    pub timeout: TimeoutPolicy,
    #[serde(with = "duration_millis")]
    pub kill_grace: Duration,
    pub output_limit: usize,
    pub tty: bool,
    pub tty_rows: u16,
    pub tty_columns: u16,
    #[serde(default)]
    pub copy_in: Vec<CopyInSpec>,
    #[serde(default)]
    pub copy_out: Vec<CopyOutSpec>,
    #[serde(default = "default_copy_limit")]
    pub copy_limit_bytes: u64,
    #[serde(default)]
    pub workspace_mode: WorkspaceMode,
}

impl RunSpec {
    pub fn command(argv: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            session_id: SessionId::new(),
            box_id: None,
            argv: argv.into_iter().map(Into::into).collect(),
            cwd: None,
            env: BTreeMap::new(),
            inherit_env: false,
            network: false,
            image_pull_policy: ImagePullPolicy::default(),
            stdin: Vec::new(),
            timeout: TimeoutPolicy::default(),
            kill_grace: DEFAULT_KILL_GRACE,
            output_limit: DEFAULT_OUTPUT_LIMIT,
            tty: false,
            tty_rows: DEFAULT_TTY_ROWS,
            tty_columns: DEFAULT_TTY_COLUMNS,
            copy_in: Vec::new(),
            copy_out: Vec::new(),
            copy_limit_bytes: DEFAULT_COPY_LIMIT,
            workspace_mode: WorkspaceMode::ReadOnly,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.argv.is_empty() || self.argv[0].is_empty() {
            return Err("argv must contain a non-empty executable");
        }
        if self.output_limit == 0 {
            return Err("output_limit must be greater than zero");
        }
        if self.output_limit > MAX_OUTPUT_LIMIT {
            return Err("output_limit must not exceed 1 GiB");
        }
        if self.kill_grace.is_zero() {
            return Err("kill_grace must be greater than zero");
        }
        if self.kill_grace > MAX_KILL_GRACE {
            return Err("kill_grace must not exceed 60 seconds");
        }
        if self.tty && (self.tty_rows == 0 || self.tty_columns == 0) {
            return Err("PTY rows and columns must be greater than zero");
        }
        if self.copy_limit_bytes == 0 || self.copy_limit_bytes > MAX_COPY_LIMIT {
            return Err("copy_limit_bytes must be between 1 byte and 1 GiB");
        }
        if self.copy_in.iter().any(|copy| {
            copy.source.as_os_str().is_empty() || !valid_guest_transfer_path(&copy.destination)
        }) {
            return Err("copy-in requires a host source and normalized absolute guest destination");
        }
        if self
            .copy_out
            .iter()
            .any(|copy| !valid_guest_transfer_path(&copy.source) || !copy.destination.is_absolute())
        {
            return Err(
                "copy-out requires a normalized absolute guest source and host destination",
            );
        }
        let mut copy_in_destinations = std::collections::BTreeSet::new();
        if self
            .copy_in
            .iter()
            .any(|copy| !copy_in_destinations.insert(&copy.destination))
        {
            return Err("copy-in guest destinations must be unique");
        }
        let mut copy_out_destinations = std::collections::BTreeSet::new();
        if self
            .copy_out
            .iter()
            .any(|copy| !copy_out_destinations.insert(&copy.destination))
        {
            return Err("copy-out host destinations must be unique");
        }
        Ok(())
    }
}

const fn default_copy_limit() -> u64 {
    DEFAULT_COPY_LIMIT
}

fn valid_guest_transfer_path(path: &str) -> bool {
    path.len() >= 2
        && path.len() <= 4096
        && path.starts_with('/')
        && path != "/"
        && !path.contains('\0')
        && path
            .split('/')
            .skip(1)
            .all(|component| !component.is_empty() && !matches!(component, "." | ".."))
}

mod duration_millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let milliseconds = u64::try_from(duration.as_millis())
            .map_err(|_| serde::ser::Error::custom("duration exceeds u64 milliseconds"))?;
        serializer.serialize_u64(milliseconds)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        u64::deserialize(deserializer).map(Duration::from_millis)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_timeout_is_one_hour() {
        assert_eq!(TimeoutPolicy::default().duration(), Some(DEFAULT_TIMEOUT));
    }

    #[test]
    fn rejects_an_empty_command() {
        let spec = RunSpec::command(Vec::<String>::new());
        assert!(spec.validate().is_err());
    }

    #[test]
    fn network_access_is_opt_in() {
        assert!(!RunSpec::command(["true"]).network);
    }

    #[test]
    fn image_pull_policy_defaults_to_missing_and_round_trips() {
        let spec = RunSpec::command(["true"]);
        assert_eq!(spec.image_pull_policy, ImagePullPolicy::Missing);
        assert_eq!("always".parse(), Ok(ImagePullPolicy::Always));
        assert_eq!(ImagePullPolicy::Never.to_string(), "never");
        assert!("sometimes".parse::<ImagePullPolicy>().is_err());
    }

    #[test]
    fn execution_resource_controls_are_bounded() {
        let defaults = RunSpec::command(["true"]);
        assert_eq!(defaults.output_limit, DEFAULT_OUTPUT_LIMIT);
        assert_eq!(defaults.kill_grace, DEFAULT_KILL_GRACE);

        let mut invalid = defaults.clone();
        invalid.output_limit = 0;
        assert_eq!(
            invalid.validate(),
            Err("output_limit must be greater than zero")
        );
        invalid.output_limit = MAX_OUTPUT_LIMIT + 1;
        assert_eq!(
            invalid.validate(),
            Err("output_limit must not exceed 1 GiB")
        );
        invalid.output_limit = DEFAULT_OUTPUT_LIMIT;
        invalid.kill_grace = Duration::ZERO;
        assert_eq!(
            invalid.validate(),
            Err("kill_grace must be greater than zero")
        );
        invalid.kill_grace = MAX_KILL_GRACE + Duration::from_millis(1);
        assert_eq!(
            invalid.validate(),
            Err("kill_grace must not exceed 60 seconds")
        );
    }

    #[test]
    fn older_serialized_specs_default_to_no_network() {
        let mut value = serde_json::to_value(RunSpec::command(["true"])).unwrap();
        value.as_object_mut().unwrap().remove("network");

        let spec: RunSpec = serde_json::from_value(value).unwrap();

        assert!(!spec.network);
    }

    #[test]
    fn older_serialized_specs_default_to_no_box() {
        let mut value = serde_json::to_value(RunSpec::command(["true"])).unwrap();
        value.as_object_mut().unwrap().remove("box_id");

        let spec: RunSpec = serde_json::from_value(value).unwrap();

        assert_eq!(spec.box_id, None);
    }

    #[test]
    fn box_id_round_trips_as_a_uuid() {
        let id = BoxId::new();
        assert_eq!(id.to_string().parse::<BoxId>().unwrap(), id);
    }

    #[test]
    fn copy_transfers_are_bounded_and_use_normalized_guest_paths() {
        let mut spec = RunSpec::command(["true"]);
        spec.copy_in.push(CopyInSpec {
            source: "/host/input".into(),
            destination: "/workspace/input".into(),
        });
        spec.copy_out.push(CopyOutSpec {
            source: "/workspace/output".into(),
            destination: "/host/output".into(),
        });
        assert!(spec.validate().is_ok());

        spec.copy_out[0].source = "/workspace/../host".into();
        assert!(spec.validate().is_err());
        spec.copy_out[0].source = "/workspace/output".into();
        spec.copy_limit_bytes = MAX_COPY_LIMIT + 1;
        assert!(spec.validate().is_err());
    }
}
