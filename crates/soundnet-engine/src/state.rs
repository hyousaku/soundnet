//! In-memory engine state shared across HTTP handlers, discovery, and workers.

use dashmap::DashMap;
use mdns_sd::ServiceDaemon;
use soundnet_protocol::{LocalPort, Node, PortId, Route, RouteId, StreamStats};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock as StdRwLock};
use tokio::sync::{broadcast, RwLock};

use crate::config::ManualHost;
use crate::routing::{RouteFailure, RunningRoute};

#[derive(Debug)]
pub struct EngineIdentity {
    pub node_id: String,
    pub hostname: String,
    /// Effective advertised/egress address. Behind a lock (not `Arc`-and-done
    /// like the rest of identity) because the operator can pin a different
    /// interface at runtime — see `iface::set_selected` — and every reader
    /// (mDNS, `self_node()`, route senders) needs to see the change without
    /// a restart. A plain `std::sync::RwLock` is enough: reads/writes are
    /// brief and never held across an `.await`.
    pub addr: StdRwLock<IpAddr>,
    pub control_port: u16,
    pub audio_port: u16,
    pub version: String,
}

pub struct EngineState {
    pub identity: EngineIdentity,

    /// Local capture/playback/tone ports on THIS node.
    pub local_ports: DashMap<PortId, LocalPort>,

    /// Nodes we've seen via mDNS (or manual add). Keyed by node id.
    pub peers: DashMap<String, PeerRecord>,

    /// Routes we're currently running (RouteId → running task handle).
    pub routes: DashMap<RouteId, Route>,
    pub running: DashMap<RouteId, RunningRoute>,

    /// Backoff bookkeeping for routes this engine has a local role in but
    /// that failed to start (or whose workers died) — see `routing::try_start`.
    pub failures: DashMap<RouteId, RouteFailure>,

    /// Rolling per-route stats (updated by workers).
    pub stats: DashMap<RouteId, StreamStats>,

    /// Persisted config path we write back to on route/manual-host changes.
    pub config_path: RwLock<Option<PathBuf>>,

    /// Manually added hosts (fallback when mDNS is blocked).
    pub manual_hosts: RwLock<Vec<ManualHost>>,

    /// Name of the interface pinned for mDNS/audio egress, or `None` for
    /// automatic. Mirrors `Config::interface`; kept here (not just read from
    /// disk on demand) so handlers can read/change it without file I/O, and
    /// so `routing::persist()` can write it back out on every save.
    pub selected_interface: RwLock<Option<String>>,

    /// Broadcasted server messages to all connected WebSocket clients.
    pub events: broadcast::Sender<soundnet_protocol::ServerMsg>,

    /// Set once discovery has registered us with mDNS. Used on shutdown to
    /// send a graceful "goodbye" (unregister) so peers drop us immediately
    /// instead of waiting out the mDNS record TTL — otherwise every
    /// `systemctl restart` (SIGTERM, not SIGINT) leaves a ghost node in
    /// every other engine's peer list until the stale record expires.
    pub mdns: RwLock<Option<MdnsHandle>>,
}

#[derive(Clone)]
pub struct MdnsHandle {
    pub daemon: ServiceDaemon,
    pub fullname: String,
}

#[derive(Debug, Clone)]
pub struct PeerRecord {
    pub node: Node,
    /// Ports advertised by the peer (fetched from its /api/state on discovery).
    pub ports: Vec<LocalPort>,
}

impl EngineState {
    pub fn new(identity: EngineIdentity) -> Arc<Self> {
        let (tx, _rx) = broadcast::channel(256);
        Arc::new(Self {
            identity,
            local_ports: DashMap::new(),
            peers: DashMap::new(),
            routes: DashMap::new(),
            running: DashMap::new(),
            failures: DashMap::new(),
            stats: DashMap::new(),
            config_path: RwLock::new(None),
            manual_hosts: RwLock::new(Vec::new()),
            selected_interface: RwLock::new(None),
            events: tx,
            mdns: RwLock::new(None),
        })
    }

    pub fn self_node(&self) -> Node {
        Node {
            id: self.identity.node_id.clone(),
            hostname: self.identity.hostname.clone(),
            addr: self.identity.addr.read().unwrap().to_string(),
            port: self.identity.control_port,
            audio_port: self.identity.audio_port,
            version: self.identity.version.clone(),
        }
    }
}
