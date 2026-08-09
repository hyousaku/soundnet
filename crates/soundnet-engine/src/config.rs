//! On-disk configuration: persisted routes and manually-added hosts.

use anyhow::Result;
use serde::{Deserialize, Serialize};
pub use soundnet_protocol::ManualHost;
use soundnet_protocol::Route;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Stable node identity, generated once on first run and persisted
    /// forever after. Without this, every restart (crash, `systemctl
    /// restart`, a rebuild) would mint a fresh UUID and peers would show the
    /// old and new identity as two permanently-stale nodes side by side.
    #[serde(default)]
    pub node_id: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The node_id is what makes a restart look like the same node to every
    /// peer; if it silently dropped out of the save/load round trip (e.g. a
    /// missed `#[serde(default)]` or a field left out of a manual struct
    /// literal) every restart would regress to the old bug of minting a new
    /// UUID.
    #[test]
    fn node_id_round_trips_through_save_and_load() {
        let path = std::env::temp_dir().join(format!(
            "soundnet-config-test-{}-node-id.toml",
            std::process::id()
        ));

        let mut cfg = Config::default();
        cfg.node_id = Some("fixed-test-node-id".to_string());
        cfg.save(&path).expect("save");

        let loaded = Config::load_or_default(Some(&path)).expect("load");
        assert_eq!(loaded.node_id.as_deref(), Some("fixed-test-node-id"));

        let _ = std::fs::remove_file(&path);
    }

    /// A config file written before node_id existed (or one hand-edited
    /// without it) must still load — `#[serde(default)]` is what lets old
    /// on-disk configs on the two already-deployed machines keep working
    /// after this upgrade.
    #[test]
    fn missing_node_id_defaults_to_none() {
        let path = std::env::temp_dir().join(format!(
            "soundnet-config-test-{}-legacy.toml",
            std::process::id()
        ));
        std::fs::write(&path, "routes = []\nmanual_hosts = []\n").expect("write");

        let loaded = Config::load_or_default(Some(&path)).expect("load");
        assert_eq!(loaded.node_id, None);

        let _ = std::fs::remove_file(&path);
    }
}
