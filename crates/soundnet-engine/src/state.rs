//! In-memory engine state shared across HTTP handlers, discovery, and workers.

use dashmap::DashMap;
use soundnet_protocol::{LocalPort, Node, PortId, Route, RouteId, StreamStats};
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use crate::routing::RunningRoute;

#[derive(Debug, Clone)]
pub struct EngineIdentity {
    pub node_id: String,
    pub hostname: String,
    pub addr: IpAddr,
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

    /// Rolling per-route stats (updated by workers).
    pub stats: DashMap<RouteId, StreamStats>,

    /// Persisted config path we write back to on route changes.
    pub config_path: RwLock<Option<std::path::PathBuf>>,

    /// Broadcasted server messages to all connected WebSocket clients.
    pub events: broadcast::Sender<soundnet_protocol::ServerMsg>,
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
            stats: DashMap::new(),
            config_path: RwLock::new(None),
            events: tx,
        })
    }

    pub fn self_node(&self) -> Node {
        Node {
            id: self.identity.node_id.clone(),
            hostname: self.identity.hostname.clone(),
            addr: self.identity.addr.to_string(),
            port: self.identity.control_port,
            audio_port: self.identity.audio_port,
            version: self.identity.version.clone(),
        }
    }
}
