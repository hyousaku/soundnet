//! On-disk configuration: persisted routes and manually-added hosts.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
pub use soundnet_protocol::ManualHost;
use soundnet_protocol::Route;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Stable node identity, generated once on first run and persisted
    /// forever after. Without this, every restart (crash, `systemctl
    /// restart`, a rebuild) would mint a fresh UUID and peers would show the
    /// old and new identity as two permanently-stale nodes side by side.
    #[serde(default)]
    pub node_id: Option<String>,
    /// Name (not IP — DHCP can reassign the IP but not the NIC) of the
    /// network interface pinned for mDNS advertisement and audio egress on
    /// multi-homed hosts. `None` means pick automatically, the same
    /// behaviour as before this field existed.
    #[serde(default)]
    pub interface: Option<String>,
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
        write_atomically(path, |f| f.write_all(out.as_bytes()))
    }
}

/// Write a file by filling a temporary one, flushing it to the platter, and
/// renaming it over the target. `rename` within a directory is atomic, so a
/// reader either sees the whole old file or the whole new one and never a
/// truncated mixture.
///
/// This matters more here than the usual "be careful with files" reflex.
/// `routing::persist` rewrites the *entire* config on every route edit, every
/// manual-host change and every interface change, so the window in which the
/// file is half-written is not a rare startup event — it is open several
/// times during ordinary use. A plain `fs::write` truncates first, so losing
/// power in that window leaves a config that fails to parse, and the engine
/// comes back with no routes *and no `node_id`* — which means it re-mints its
/// identity and shows up to every peer as a second, unfamiliar node. That is
/// the ghost-node bug this project already paid to fix once, and it would
/// return through the one path nobody is watching. A system whose whole point
/// is unattended recovery cannot have its identity resting on a non-atomic
/// write.
///
/// The content is supplied by a closure rather than a byte slice so a caller
/// (today: the test) can fail partway through a write and check that the
/// previous file survived, without threading a fault-injection flag through
/// production code.
fn write_atomically(
    path: &Path,
    write: impl FnOnce(&mut File) -> std::io::Result<()>,
) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = temp_sibling(path);

    // Scoped so the file is closed before the rename: on some filesystems
    // renaming over a path while still holding the source open is asking for
    // trouble, and there is nothing to gain by keeping it.
    let result = (|| -> std::io::Result<()> {
        let mut f = File::create(&tmp)?;
        write(&mut f)?;
        // Without this the rename can land before the data does, and a power
        // cut leaves the *new* name pointing at a file full of zeroes — worse
        // than the problem being fixed, because it looks like a successful
        // write.
        f.sync_all()?;
        Ok(())
    })();

    if let Err(err) = result {
        // Best-effort: if this fails too there is nothing useful left to do,
        // and the important guarantee (the old file is untouched) already
        // holds.
        let _ = std::fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("writing {}", tmp.display()));
    }

    std::fs::rename(&tmp, path)
        .with_context(|| format!("replacing {} with {}", path.display(), tmp.display()))?;

    // The rename is atomic with respect to readers immediately, but the
    // directory entry itself is only durable once the directory is synced.
    // Best-effort: a config that is correct but a few seconds stale after a
    // power cut is a much smaller problem than a corrupt one, and this is not
    // supported on every filesystem.
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// A temporary path next to `path` — same directory, because `rename` is only
/// atomic within one filesystem and `/tmp` is routinely a different one.
///
/// The counter is not decoration: `persist()` is called from async tasks and
/// two route edits landing together would otherwise pick the same name, with
/// one save writing into the other's temporary file.
fn temp_sibling(path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".to_string());
    dir.join(format!(
        ".{name}.tmp.{}.{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
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

        let cfg = Config {
            node_id: Some("fixed-test-node-id".to_string()),
            ..Default::default()
        };
        cfg.save(&path).expect("save");

        let loaded = Config::load_or_default(Some(&path)).expect("load");
        assert_eq!(loaded.node_id.as_deref(), Some("fixed-test-node-id"));

        let _ = std::fs::remove_file(&path);
    }

    /// A config file written before node_id (or interface) existed, or one
    /// hand-edited without them, must still load — `#[serde(default)]` is
    /// what lets old on-disk configs on the two already-deployed machines
    /// keep working after each upgrade that adds a field here.
    #[test]
    fn missing_node_id_defaults_to_none() {
        let path = std::env::temp_dir().join(format!(
            "soundnet-config-test-{}-legacy.toml",
            std::process::id()
        ));
        std::fs::write(&path, "routes = []\nmanual_hosts = []\n").expect("write");

        let loaded = Config::load_or_default(Some(&path)).expect("load");
        assert_eq!(loaded.node_id, None);
        assert_eq!(loaded.interface, None);

        let _ = std::fs::remove_file(&path);
    }

    /// Same round-trip guarantee as `node_id`: the interface field holds a
    /// NIC *name*, not an IP, and `routing::persist()` rebuilds the whole
    /// Config from live state on every route/manual-host change — if this
    /// field isn't threaded through there too, a pinned interface would get
    /// silently wiped back to "automatic" the next time the operator adds a
    /// route. See `routing::persist` for the other half of this guarantee.
    #[test]
    fn interface_round_trips_through_save_and_load() {
        let path = std::env::temp_dir().join(format!(
            "soundnet-config-test-{}-interface.toml",
            std::process::id()
        ));

        let cfg = Config {
            interface: Some("eth0".to_string()),
            ..Default::default()
        };
        cfg.save(&path).expect("save");

        let loaded = Config::load_or_default(Some(&path)).expect("load");
        assert_eq!(loaded.interface.as_deref(), Some("eth0"));

        let _ = std::fs::remove_file(&path);
    }

    /// A directory of its own, so the leftover-file assertion below can look
    /// at everything in it rather than guessing temporary-file names.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("soundnet-config-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    /// The reason `save` does not just call `fs::write`: `persist()` rewrites
    /// the whole config on every route edit, so a write that dies partway
    /// through is an ordinary-use event, not a rare one. If it took the old
    /// file with it the engine would come back with no routes and no
    /// `node_id` — re-minting its identity and reappearing to every peer as a
    /// stranger.
    ///
    /// The failure is injected through the write closure, which is why
    /// `write_atomically` takes one.
    #[test]
    fn a_write_that_dies_partway_leaves_the_previous_config_intact() {
        let dir = scratch_dir("atomic");
        let path = dir.join("config.toml");

        let cfg = Config {
            node_id: Some("survives-the-crash".to_string()),
            interface: Some("eth0".to_string()),
            ..Default::default()
        };
        cfg.save(&path).expect("save");

        let err = write_atomically(&path, |f| {
            // Enough bytes to be a plausible partial config, so the test
            // would notice a truncated file rather than an empty one.
            f.write_all(b"node_id = \"half-writ")?;
            Err(std::io::Error::other("simulated disk full"))
        })
        .expect_err("the injected failure must surface, not be swallowed");
        assert!(
            format!("{err:#}").contains("simulated disk full"),
            "the cause should reach the caller, got: {err:#}"
        );

        let loaded = Config::load_or_default(Some(&path)).expect("the old config must still parse");
        assert_eq!(loaded.node_id.as_deref(), Some("survives-the-crash"));
        assert_eq!(loaded.interface.as_deref(), Some("eth0"));

        // A failed save must not leave its scratch file behind either, or
        // every power cut would add one to the config directory forever.
        let left: Vec<String> = std::fs::read_dir(&dir)
            .expect("read scratch dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "config.toml")
            .collect();
        assert!(left.is_empty(), "temporary files left behind: {left:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The temporary file has to be a sibling of the target: `rename` is only
    /// atomic within one filesystem, and a config directory on a different
    /// mount from `/tmp` is the normal case, not an exotic one.
    #[test]
    fn the_scratch_file_is_written_next_to_the_target() {
        let path = Path::new("/some/config/dir/config.toml");
        let tmp = temp_sibling(path);
        assert_eq!(tmp.parent(), path.parent());
        assert_ne!(tmp, path.to_path_buf());
    }

    /// Two saves racing (two route edits arriving together) must not write
    /// into the same scratch file.
    #[test]
    fn concurrent_saves_do_not_share_a_scratch_file() {
        let path = Path::new("/some/config/dir/config.toml");
        assert_ne!(temp_sibling(path), temp_sibling(path));
    }
}
