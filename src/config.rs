use std::{
    fmt::{self, Display},
    path::{Path, PathBuf},
};

use serde::Deserialize;

fn default_max_attempts_per_sha() -> u32 {
    3
}

fn default_max_reviews_per_pr() -> u32 {
    20
}

/// Caps that break review loops: a head sha is retried at most
/// `max_attempts_per_sha` times after failures, and a PR is reviewed at most
/// `max_reviews_per_pr` times over its lifetime.
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct ReviewLimits {
    #[serde(default = "default_max_attempts_per_sha")]
    pub max_attempts_per_sha: u32,
    #[serde(default = "default_max_reviews_per_pr")]
    pub max_reviews_per_pr: u32,
}

impl Default for ReviewLimits {
    fn default() -> Self {
        Self {
            max_attempts_per_sha: default_max_attempts_per_sha(),
            max_reviews_per_pr: default_max_reviews_per_pr(),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    pub auto_merge: bool,
    pub repos_path: PathBuf,
    /// Where PR state is persisted. Defaults to `<repos_path>/.state.json`.
    #[serde(default)]
    pub state_path: Option<PathBuf>,
    #[serde(flatten)]
    pub limits: ReviewLimits,
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(toml::de::Error),
}

impl Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "reading config: {e}"),
            ConfigError::Parse(e) => write!(f, "parsing config: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(ConfigError::Io)?;
        toml::from_str(&raw).map_err(ConfigError::Parse)
    }

    pub fn state_path(&self) -> PathBuf {
        self.state_path
            .clone()
            .unwrap_or_else(|| self.repos_path.join(".state.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config_with_default_limits() {
        let config: Config = toml::from_str(
            r#"
            auto_merge = true
            repos_path = "/var/repos"
            "#,
        )
        .unwrap();

        assert!(config.auto_merge);
        assert_eq!(config.repos_path, PathBuf::from("/var/repos"));
        assert_eq!(config.state_path(), PathBuf::from("/var/repos/.state.json"));
        assert_eq!(config.limits.max_attempts_per_sha, 3);
        assert_eq!(config.limits.max_reviews_per_pr, 20);
    }

    #[test]
    fn parses_full_config() {
        let config: Config = toml::from_str(
            r#"
            auto_merge = false
            repos_path = "/var/repos"
            state_path = "/var/state.json"
            max_attempts_per_sha = 5
            max_reviews_per_pr = 10
            "#,
        )
        .unwrap();

        assert!(!config.auto_merge);
        assert_eq!(config.state_path(), PathBuf::from("/var/state.json"));
        assert_eq!(config.limits.max_attempts_per_sha, 5);
        assert_eq!(config.limits.max_reviews_per_pr, 10);
    }

    #[test]
    fn rejects_config_missing_required_fields() {
        assert!(toml::from_str::<Config>("auto_merge = true").is_err());
    }
}
