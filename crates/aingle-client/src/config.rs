use std::{fs, path::PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::ClientError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub api_url: Url,
    pub websocket_url: Url,
    #[serde(default)]
    pub history_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        let dirs = ProjectDirs::from("dev", "aingle", "aingle");
        Self {
            api_url: Url::parse("https://api.aingl.net").expect("valid default URL"),
            websocket_url: Url::parse("wss://api.aingl.net/v1/socket").expect("valid default URL"),
            history_dir: dirs.map(|dirs| dirs.data_dir().to_path_buf()),
        }
    }
}

impl Config {
    pub fn path() -> Result<PathBuf, ClientError> {
        ProjectDirs::from("dev", "aingle", "aingle")
            .map(|dirs| dirs.config_dir().join("config.toml"))
            .ok_or_else(|| ClientError::Config("cannot determine configuration directory".into()))
    }

    pub fn load() -> Result<Self, ClientError> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let content =
            fs::read_to_string(path).map_err(|error| ClientError::Config(error.to_string()))?;
        toml::from_str(&content).map_err(|error| ClientError::Config(error.to_string()))
    }

    pub fn save(&self) -> Result<(), ClientError> {
        let path = Self::path()?;
        fs::create_dir_all(path.parent().expect("config path has parent"))
            .map_err(|error| ClientError::Config(error.to_string()))?;
        fs::write(
            path,
            toml::to_string_pretty(self).map_err(|error| ClientError::Config(error.to_string()))?,
        )
        .map_err(|error| ClientError::Config(error.to_string()))
    }
}
