//! mDNS (Bonjour/Avahi) advertise + browse for `_soundnet._udp.local.`.
//!
//! Each engine advertises: hostname, control port, audio port, node id.
//! When it sees another engine, it fetches `/api/state` from that peer
//! and stores the returned ports in EngineState.peers so the GUI can
//! render everything.

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use soundnet_protocol::{LocalPort, Node, PeerPortsPush, ServerMsg};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::routing;
use crate::state::{EngineState, MdnsHandle, PeerRecord};

const SERVICE_TYPE: &str = "_soundnet._udp.local.";

/// How often we poll each known peer's `/api/state` to check it's still
/// alive. mDNS's ServiceRemoved only fires on a graceful goodbye, so this is
/// the only thing that notices a peer that crashed, got unplugged, or moved
/// to a new IP via DHCP.
const LIVENESS_INTERVAL: Duration = Duration::from_secs(10);
/// A single dropped request could just be a busy peer or a lost packet — wait
/// for three in a row (~30s) before declaring the peer gone.
const LIVENESS_FAILURE_THRESHOLD: u32 = 3;
const LIVENESS_TIMEOUT: Duration = Duration::from_secs(2);

pub fn spawn(state: Arc<EngineState>, ip: IpAddr, control_port: u16, audio_port: u16) {
    tokio::spawn(async move {
        if let Err(err) = run(state, ip, control_port, audio_port).await {
            tracing::error!("discovery worker exited: {err:#}");
        }
    });
}

/// Build the `ServiceInfo` this engine advertises for a given address.
/// Deliberately does **not** call `enable_addr_auto()`: that registers every
/// interface IP on the box, and on a multi-homed host (wired + wireless on
/// the same subnet) peers then get re-resolved every ~2s alternating between
/// addresses, forever. We always advertise exactly one address — either the
/// pinned interface's or the automatically-chosen one — and it's the caller's
/// job to have already picked which.
fn build_service_info(state: &EngineState, ip: IpAddr, audio_port: u16) -> anyhow::Result<ServiceInfo> {
    let host_label = format!("{}-{}", state.identity.hostname, &state.identity.node_id[..8]);
    let instance = &state.identity.node_id;
    let mut props = std::collections::HashMap::new();
    props.insert("node_id".to_string(), state.identity.node_id.clone());
    props.insert("audio_port".to_string(), audio_port.to_string());
    props.insert("version".to_string(), state.identity.version.clone());
    props.insert("hostname".to_string(), state.identity.hostname.clone());

    let info = ServiceInfo::new(
        SERVICE_TYPE,
        instance,
        &format!("{host_label}.local."),
        ip.to_string().as_str(),
        state.identity.control_port,
        Some(props),
    )?;
    Ok(info)
}

async fn run(
    state: Arc<EngineState>,
    ip: IpAddr,
    control_port: u16,
    audio_port: u16,
) -> anyhow::Result<()> {
    let daemon = ServiceDaemon::new()?;

    let info = build_service_info(&state, ip, audio_port)?;
    let fullname = info.get_fullname().to_string();
    daemon.register(info)?;
    tracing::info!("mDNS: advertising {SERVICE_TYPE} on {ip}:{control_port}");

    // Stash the daemon + fullname so main.rs can send a graceful goodbye on
    // shutdown (SIGTERM from systemd, or Ctrl-C) instead of leaving a ghost
    // record for peers to time out on.
    *state.mdns.write().await = Some(MdnsHandle { daemon: daemon.clone(), fullname });

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

/// Re-register this engine's mDNS record under a new address — used when the
/// operator pins (or un-pins) a network interface at runtime (see
/// `iface::set_selected`). Unregisters the old record first so peers pick up
/// the new address promptly instead of caching the old one until its TTL
/// expires with two records momentarily in flight.
pub async fn reregister(state: &Arc<EngineState>, ip: IpAddr) -> anyhow::Result<()> {
    let mut guard = state.mdns.write().await;
    let Some(handle) = guard.as_ref() else {
        // Discovery hasn't finished its own initial registration yet (a
        // narrow startup race). Nothing to re-register — that first
        // registration reads the current address when it runs, so it'll
        // already be correct.
        return Ok(());
    };
    let daemon = handle.daemon.clone();
    let old_fullname = handle.fullname.clone();

    let info = build_service_info(state, ip, state.identity.audio_port)?;
    let new_fullname = info.get_fullname().to_string();

    if let Ok(rx) = daemon.unregister(&old_fullname) {
        // Best-effort wait for the goodbye to actually go out before we
        // advertise the replacement, capped so a slow/stuck mDNS event loop
        // can't hang an interface switch indefinitely.
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), async {
            let _ = tokio::task::spawn_blocking(move || rx.recv()).await;
        })
        .await;
    }
    daemon.register(info)?;
    tracing::info!("mDNS: re-advertising {SERVICE_TYPE} on {ip}");

    *guard = Some(MdnsHandle { daemon, fullname: new_fullname });
    Ok(())
}

/// Periodically probe every known peer's control plane and drop the ones
/// that stop answering. This is what actually gets rid of a peer record once
/// its process is gone — mDNS ServiceRemoved only covers a clean shutdown,
/// and the previous ServiceRemoved matching (fullname substring on the first
/// 8 chars of a node id) wasn't reliable enough to depend on for that either.
pub fn spawn_liveness_checker(state: Arc<EngineState>) {
    tokio::spawn(async move {
        // Failure counts live only in this task; nothing else needs to see
        // them, and keeping them here avoids putting transient bookkeeping
        // on EngineState.
        let mut failures: HashMap<String, u32> = HashMap::new();
        let mut interval = tokio::time::interval(LIVENESS_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            check_peers_once(&state, &mut failures).await;
        }
    });
}

async fn check_peers_once(state: &Arc<EngineState>, failures: &mut HashMap<String, u32>) {
    let snapshot: Vec<(String, Node)> =
        state.peers.iter().map(|e| (e.key().clone(), e.value().node.clone())).collect();
    // Drop stale bookkeeping for ids that already left state.peers by some
    // other path (e.g. remove_manual) so the map doesn't grow forever.
    failures.retain(|id, _| snapshot.iter().any(|(pid, _)| pid == id));

    for (id, node) in snapshot {
        let url = format!("http://{}:{}/api/state", node.addr, node.port);
        let fetched = tokio::task::spawn_blocking(move || {
            ureq::get(&url)
                .timeout(LIVENESS_TIMEOUT)
                .call()
                .and_then(|resp| resp.into_json::<soundnet_protocol::StateSnapshot>().map_err(Into::into))
        })
        .await;

        match fetched {
            Ok(Ok(snap)) => {
                failures.remove(&id);
                let real_id = snap.self_node.id.clone();
                if real_id != id {
                    // The address we had cached for `id` now answers as a
                    // different node id — the classic ghost case, e.g. this
                    // engine restarted with a new identity while this
                    // machine still had the old record cached (or, before
                    // the node_id persistence fix, just restarted at all).
                    // The id that's actually reachable wins; drop the stale
                    // one instead of leaving both permanently in the list.
                    tracing::info!(
                        "liveness: {}:{} now answers as {} (was cached as {}); dropping the stale record",
                        node.addr, node.port, real_id, id
                    );
                    state.peers.remove(&id);
                    let _ = state.events.send(ServerMsg::NodeDisappeared { node_id: id });
                }
                // Refresh the cached record either way — ports may have
                // changed since we first saw this peer, and this is cheap.
                state.peers.insert(
                    real_id,
                    PeerRecord { node: snap.self_node.clone(), ports: snap.local_ports },
                );
            }
            _ => {
                let count = failures.entry(id.clone()).or_insert(0);
                *count += 1;
                if *count >= LIVENESS_FAILURE_THRESHOLD {
                    if state.peers.remove(&id).is_some() {
                        tracing::info!(
                            "liveness: peer {id} unreachable after {count} checks, dropping"
                        );
                        let _ = state.events.send(ServerMsg::NodeDisappeared { node_id: id.clone() });
                    }
                    failures.remove(&id);
                }
            }
        }
    }
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
            hosts.push(soundnet_protocol::ManualHost { addr: addr.clone(), port });
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

/// Push our current port list to every peer we know about. Fire-and-forget:
/// best effort, no retry — a peer that's down or unreachable just keeps its
/// stale copy until it next re-fetches on its own (e.g. its own rescan, or
/// the next time mDNS resolves us fresh on its end).
pub fn push_ports_to_peers(state: &Arc<EngineState>) {
    let self_node = state.self_node();
    let mut ports: Vec<LocalPort> = state.local_ports.iter().map(|e| e.value().clone()).collect();
    crate::audio::devices::sort_ports(&mut ports);
    let targets: Vec<(String, u16)> = state
        .peers
        .iter()
        .map(|e| (e.node.addr.clone(), e.node.port))
        .collect();

    for (addr, port) in targets {
        let push = PeerPortsPush { node: self_node.clone(), ports: ports.clone() };
        let body = match serde_json::to_string(&push) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!("push_ports_to_peers: serialize failed: {err}");
                continue;
            }
        };
        tokio::task::spawn_blocking(move || {
            let url = format!("http://{addr}:{port}/api/peer-ports");
            if let Err(err) = ureq::post(&url)
                .set("content-type", "application/json")
                .timeout(Duration::from_secs(3))
                .send_string(&body)
            {
                tracing::debug!("push_ports_to_peers: {addr}:{port} unreachable: {err}");
            }
        });
    }
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
            let _ = state
                .events
                .send(ServerMsg::NodeAppeared { node: node.clone(), ports });
            // Retry any routes that were waiting for this peer.
            routing::retry_pending_for_peer(state, &node.id).await;
        }
        Ok(Err(err)) => tracing::warn!("failed to fetch peer state from {}:{}: {err:#}", node.addr, node.port),
        Err(err) => tracing::warn!("peer state task panicked: {err}"),
    }
}
