//! On-disk configuration: persisted routes and manually-added hosts.

use anyhow::Result;
use serde::{Deserialize, Serialize};
pub use soundnet_protocol::ManualHost;
use soundnet_protocol::Route;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub manual_hosts: Vec<ManualHost>,
}

impl Config {
    pub fn load_or_default(override_path: Option<&Path>) -> Result<Self> {
        let path = match override_path {
            Some(p) => p.to_path_buf(),
            None => default_path(),
        };
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)?;
        let cfg: Config = toml::from_str(&raw)?;
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let out = toml::to_string_pretty(self)?;
        std::fs::write(path, out)?;
        Ok(())
    }
}

pub fn default_path() -> PathBuf {
    if let Some(pd) = directories::ProjectDirs::from("net", "soundnet", "soundnet") {
        pd.config_dir().join("config.toml")
    } else {
        PathBuf::from("./soundnet.toml")
    }
}
