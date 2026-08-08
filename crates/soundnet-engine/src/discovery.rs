//! mDNS (Bonjour/Avahi) advertise + browse for `_soundnet._udp.local.`.
//!
//! Each engine advertises: hostname, control port, audio port, node id.
//! When it sees another engine, it fetches `/api/state` from that peer
//! and stores the returned ports in EngineState.peers so the GUI can
//! render everything.

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use soundnet_protocol::{LocalPort, Node, ServerMsg};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::routing;
use crate::state::{EngineState, PeerRecord};

const SERVICE_TYPE: &str = "_soundnet._udp.local.";

pub fn spawn(state: Arc<EngineState>, ip: IpAddr, control_port: u16, audio_port: u16) {
    tokio::spawn(async move {
        if let Err(err) = run(state, ip, control_port, audio_port).await {
            tracing::error!("discovery worker exited: {err:#}");
        }
    });
}

async fn run(
    state: Arc<EngineState>,
    ip: IpAddr,
    control_port: u16,
    audio_port: u16,
) -> anyhow::Result<()> {
    let daemon = ServiceDaemon::new()?;

    let host_label = format!("{}-{}", state.identity.hostname, &state.identity.node_id[..8]);
    let instance = &state.identity.node_id;
    let mut props = std::collections::HashMap::new();
    props.insert("node_id".to_string(), state.identity.node_id.clone());
    props.insert("audio_port".to_string(), audio_port.to_string());
    props.insert("version".to_string(), state.identity.version.clone());
    props.insert("hostname".to_string(), state.identity.hostname.clone());

    let mut info = ServiceInfo::new(
        SERVICE_TYPE,
        instance,
        &format!("{host_label}.local."),
        ip.to_string().as_str(),
        control_port,
        Some(props),
    )?;
    info = info.enable_addr_auto();
    daemon.register(info)?;
    tracing::info!("mDNS: advertising {SERVICE_TYPE} on {ip}:{control_port}");

    // Start browsing for peers.
    let receiver = daemon.browse(SERVICE_TYPE)?;

    while let Ok(event) = receiver.recv_async().await {
        match event {
            ServiceEvent::ServiceResolved(info) => {
                // Ignore self.
                if info
                    .get_property_val_str("node_id")
                    .map(|s| s == state.identity.node_id)
                    .unwrap_or(false)
                {
                    continue;
                }
                let addr = info
                    .get_addresses()
                    .iter()
                    .find(|a| a.is_ipv4())
                    .copied();
                let Some(addr) = addr else { continue };
                let node = Node {
                    id: info.get_property_val_str("node_id").unwrap_or("").to_string(),
                    hostname: info
                        .get_property_val_str("hostname")
                        .unwrap_or(info.get_hostname())
                        .to_string(),
                    addr: addr.to_string(),
                    port: info.get_port(),
                    audio_port: info
                        .get_property_val_str("audio_port")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(10001),
                    version: info
                        .get_property_val_str("version")
                        .unwrap_or("?")
                        .to_string(),
                };
                if node.id.is_empty() {
                    continue;
                }
                tracing::info!("mDNS: peer {} ({}:{})", node.hostname, node.addr, node.port);
                fetch_peer_state(&state, node).await;
            }
            ServiceEvent::ServiceRemoved(_ty, fullname) => {
                // Find and drop peer by fullname match (best-effort — fullname
                // includes the node id we registered).
                let removed: Option<String> = state
                    .peers
                    .iter()
                    .find_map(|entry| {
                        if fullname.contains(&entry.key()[..8]) {
                            Some(entry.key().clone())
                        } else {
                            None
                        }
                    });
                if let Some(id) = removed {
                    state.peers.remove(&id);
                    let _ = state.events.send(ServerMsg::NodeDisappeared { node_id: id });
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Kick off a background fetch for a manually-added host. If the fetch
/// succeeds, the peer is added to state and broadcast as if mDNS had found it.
pub fn probe_manual(state: Arc<EngineState>, addr: String, port: u16) {
    tokio::spawn(async move {
        // We don't yet know the node id / audio port — start with placeholders
        // and let fetch_peer_state overwrite once the snapshot lands.
        let seed = Node {
            id: format!("manual:{addr}:{port}"),
            hostname: addr.clone(),
            addr,
            port,
            audio_port: 10001,
            version: "?".into(),
        };
        fetch_peer_state(&state, seed).await;
    });
}

/// Add a manual host to persistent config and try to reach it now.
pub async fn add_manual(state: &Arc<EngineState>, addr: String, port: u16) {
    {
        let mut hosts = state.manual_hosts.write().await;
        let already = hosts.iter().any(|h| h.addr == addr && h.port == port);
        if !already {
            hosts.push(crate::config::ManualHost { addr: addr.clone(), port });
        }
    }
    routing::persist(state).await;
    probe_manual(state.clone(), addr, port);
}

pub async fn remove_manual(state: &Arc<EngineState>, addr: &str, port: u16) {
    {
        let mut hosts = state.manual_hosts.write().await;
        hosts.retain(|h| !(h.addr == addr && h.port == port));
    }
    // Drop any peer entries pointing at this host.
    let removed: Vec<String> = state
        .peers
        .iter()
        .filter_map(|entry| {
            if entry.node.addr == addr && entry.node.port == port {
                Some(entry.key().clone())
            } else {
                None
            }
        })
        .collect();
    for id in removed {
        state.peers.remove(&id);
        let _ = state.events.send(ServerMsg::NodeDisappeared { node_id: id });
    }
    routing::persist(state).await;
}

async fn fetch_peer_state(state: &Arc<EngineState>, node: Node) {
    let url = format!("http://{}:{}/api/state", node.addr, node.port);
    let fetched = tokio::task::spawn_blocking(move || {
        ureq::get(&url)
            .timeout(Duration::from_secs(2))
            .call()
            .and_then(|resp| resp.into_json::<soundnet_protocol::StateSnapshot>().map_err(Into::into))
    })
    .await;
    match fetched {
        Ok(Ok(snap)) => {
            // Prefer the ids/ports the peer reports over whatever we had.
            let node = snap.self_node.clone();
            let ports: Vec<LocalPort> = snap.local_ports;
            let record = PeerRecord { node: node.clone(), ports: ports.clone() };
            state.peers.insert(node.id.clone(), record);
            let _ = state.events.send(ServerMsg::NodeAppeared { node, ports });
        }
        Ok(Err(err)) => tracing::warn!("failed to fetch peer state from {}:{}: {err:#}", node.addr, node.port),
        Err(err) => tracing::warn!("peer state task panicked: {err}"),
    }
}
