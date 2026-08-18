use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60 * 60);
pub const DEFAULT_KILL_GRACE: Duration = Duration::from_secs(5);
pub const DEFAULT_OUTPUT_LIMIT: usize = 64 * 1024 * 1024;
pub const DEFAULT_TTY_ROWS: u16 = 24;
pub const DEFAULT_TTY_COLUMNS: u16 = 80;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "milliseconds", rename_all = "snake_case")]
pub enum TimeoutPolicy {
    Limited(u64),
    Unlimited,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSpec {
    pub session_id: SessionId,
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub inherit_env: bool,
    pub stdin: Vec<u8>,
    pub timeout: TimeoutPolicy,
    #[serde(with = "duration_millis")]
    pub kill_grace: Duration,
    pub output_limit: usize,
    pub tty: bool,
    pub tty_rows: u16,
    pub tty_columns: u16,
}

impl RunSpec {
    pub fn command(argv: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            session_id: SessionId::new(),
            argv: argv.into_iter().map(Into::into).collect(),
            cwd: None,
            env: BTreeMap::new(),
            inherit_env: false,
            stdin: Vec::new(),
            timeout: TimeoutPolicy::default(),
            kill_grace: DEFAULT_KILL_GRACE,
            output_limit: DEFAULT_OUTPUT_LIMIT,
            tty: false,
            tty_rows: DEFAULT_TTY_ROWS,
            tty_columns: DEFAULT_TTY_COLUMNS,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.argv.is_empty() || self.argv[0].is_empty() {
            return Err("argv must contain a non-empty executable");
        }
        if self.output_limit == 0 {
            return Err("output_limit must be greater than zero");
        }
        if self.tty && (self.tty_rows == 0 || self.tty_columns == 0) {
            return Err("PTY rows and columns must be greater than zero");
        }
        Ok(())
    }
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
}
