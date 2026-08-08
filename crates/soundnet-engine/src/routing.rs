//! Wire up local capture → remote sender, and remote receiver → local
//! playback, one Route at a time.
//!
//! Each engine decides its own responsibility based on where it sits in the
//! Route:
//!
//! * If `route.src.node_id == self`, spawn a local capture worker (ALSA or
//!   tone) and a roc sender that connects to the destination's audio port.
//! * If `route.dst.node_id == self`, spawn a roc receiver bound to our audio
//!   port and a playback worker that plays into local ALSA.
//! * Otherwise the Route belongs to two other peers — nothing to do locally,
//!   but the route still lives in state so the UI can render it.
//!
//! Route add/remove is gossiped between engines over HTTP so both endpoints
//! spin up their side without the browser having to talk to both.

use anyhow::{anyhow, Context, Result};
use once_cell::sync::Lazy;
use soundnet_protocol::{Route, ServerMsg, StreamStats};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::audio::{
    capture::{self, CaptureControl},
    playback::{self, PlaybackControl},
};
use crate::config::Config;
use crate::state::EngineState;
use crate::transport::{receiver as rx, sender as tx, RocContext};

/// Live handles for a Route on this engine.
pub struct RunningRoute {
    pub cap: Option<CaptureControl>,
    pub tx: Option<tx::SenderHandle>,
    pub rx: Option<rx::ReceiverHandle>,
    pub pb: Option<PlaybackControl>,
    pub level_bits: Option<Arc<AtomicU32>>,
    pub xruns: Option<Arc<AtomicUsize>>,
    pub e2e_ns: Option<Arc<AtomicU64>>,
}

/// One process-wide roc context — allocating per Route would explode the
/// number of network-worker threads.
static ROC_CTX: Lazy<Mutex<Option<Arc<RocContext>>>> = Lazy::new(|| Mutex::new(None));

async fn roc_context() -> Result<Arc<RocContext>> {
    let mut guard = ROC_CTX.lock().await;
    if let Some(ctx) = guard.as_ref() {
        return Ok(ctx.clone());
    }
    let ctx = RocContext::new()?;
    *guard = Some(ctx.clone());
    Ok(ctx)
}

/// Deterministic port offset from a route id. Both sender and receiver
/// engines derive the same value so they meet on the same UDP port without
/// needing to negotiate it. Repair packets take `+1`, so we step by 2.
///
/// Range: `audio_port + 2 * (0..1000)` — that's 1000 concurrent inbound
/// routes per engine before collisions become likely.
pub fn route_port(audio_port_base: u16, route_id: &str) -> u16 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    route_id.hash(&mut hasher);
    let h = hasher.finish();
    audio_port_base.saturating_add(2 + ((h % 1000) as u16) * 2)
}

/// Whether this route asks for a route source and this engine is the sender.
pub fn is_local_src(state: &Arc<EngineState>, route: &Route) -> bool {
    route.src.node_id == state.identity.node_id
}

pub fn is_local_dst(state: &Arc<EngineState>, route: &Route) -> bool {
    route.dst.node_id == state.identity.node_id
}

/// Insert (or replace) a route in state, persist, notify browser clients, and
/// try to start the workers. Also gossip to the peer(s) mentioned in the
/// route so their engines can start their halves.
///
/// **Not fatal if endpoints aren't reachable yet** — the route lives in
/// `state.routes` regardless and `try_start` retries on peer discovery.
pub async fn apply_route(state: &Arc<EngineState>, route: Route, gossip: bool) -> Result<()> {
    let is_new_or_changed = state
        .routes
        .get(&route.id)
        .map(|r| !routes_equal(&r, &route))
        .unwrap_or(true);

    state.routes.insert(route.id.clone(), route.clone());
    persist(state).await;

    // If the running config no longer matches (or nothing was running yet),
    // reboot the workers.
    if is_new_or_changed {
        if let Some((_, existing)) = state.running.remove(&route.id) {
            shutdown_running(existing);
        }
        if let Err(err) = try_start(state, &route).await {
            tracing::warn!("route {} could not start yet: {err:#}", route.id);
        }
        if gossip {
            gossip_add(state, &route).await;
        }
    }

    let _ = state.events.send(ServerMsg::RouteAdded { route });
    Ok(())
}

/// Try to spawn the local workers for this route. Returns an error if the
/// required endpoints aren't currently available (e.g. the peer hasn't been
/// discovered yet); callers should keep the route in state and retry later.
pub async fn try_start(state: &Arc<EngineState>, route: &Route) -> Result<()> {
    if state.running.contains_key(&route.id) {
        return Ok(());
    }
    let mut running = RunningRoute {
        cap: None, tx: None, rx: None, pb: None,
        level_bits: None, xruns: None, e2e_ns: None,
    };

    if is_local_src(state, route) {
        let port = state
            .local_ports
            .get(&route.src.port_id)
            .map(|p| p.clone())
            .ok_or_else(|| anyhow!("unknown local src port {}", route.src.port_id))?;
        let dst_node = state
            .peers
            .get(&route.dst.node_id)
            .map(|r| r.node.clone())
            .with_context(|| format!("unknown dst peer {}", route.dst.node_id))?;

        let dst_port = route_port(dst_node.audio_port, &route.id);
        let cap = capture::spawn(&port.alsa_name, &route.spec)?;
        let ctx = roc_context().await?;
        let sender = tx::spawn(
            ctx,
            &dst_node.addr,
            dst_port,
            &route.spec,
            cap.consumer,
        )?;
        running.cap = Some(cap.control);
        running.tx = Some(sender);
    }

    if is_local_dst(state, route) {
        let port = state
            .local_ports
            .get(&route.dst.port_id)
            .map(|p| p.clone())
            .ok_or_else(|| anyhow!("unknown local dst port {}", route.dst.port_id))?;
        let bind_port = route_port(state.identity.audio_port, &route.id);
        let ctx = roc_context().await?;
        let pb = playback::spawn(&port.alsa_name, &route.spec)?;
        let receiver = rx::spawn(
            ctx,
            "0.0.0.0",
            bind_port,
            &route.spec,
            pb.producer,
        )?;
        running.level_bits = Some(pb.level_bits.clone());
        running.xruns = Some(pb.xruns.clone());
        running.e2e_ns = Some(receiver.e2e_latency_ns.clone());
        running.rx = Some(receiver);
        running.pb = Some(pb.control);
    }

    if running.cap.is_some() || running.pb.is_some() {
        state.running.insert(route.id.clone(), running);
    }
    Ok(())
}

/// After a new peer is discovered, walk existing routes and start any that
/// were waiting for this peer.
pub async fn retry_pending_for_peer(state: &Arc<EngineState>, peer_id: &str) {
    let candidates: Vec<Route> = state
        .routes
        .iter()
        .filter(|entry| {
            let r = entry.value();
            !state.running.contains_key(&r.id)
                && (r.src.node_id == peer_id || r.dst.node_id == peer_id)
        })
        .map(|e| e.value().clone())
        .collect();
    for r in candidates {
        if let Err(err) = try_start(state, &r).await {
            tracing::debug!("retry_pending still waiting on route {}: {err:#}", r.id);
        }
    }
}

pub async fn remove_route(state: &Arc<EngineState>, id: &str, gossip: bool) {
    let route = state.routes.remove(id).map(|(_, r)| r);
    if let Some((_, running)) = state.running.remove(id) {
        shutdown_running(running);
    }
    state.stats.remove(id);
    persist(state).await;
    if gossip {
        if let Some(route) = route {
            gossip_remove(state, &route).await;
        }
    }
    let _ = state.events.send(ServerMsg::RouteRemoved { id: id.to_string() });
}

/// Snapshot in-memory state to the configured TOML file. Best-effort — a
/// failure to write is logged but never fails the caller.
pub async fn persist(state: &Arc<EngineState>) {
    let path = state.config_path.read().await.clone();
    let Some(path) = path else { return };
    let cfg = Config {
        routes: state.routes.iter().map(|e| e.value().clone()).collect(),
        manual_hosts: state.manual_hosts.read().await.clone(),
    };
    if let Err(err) = cfg.save(&path) {
        tracing::warn!("failed to persist config to {}: {err:#}", path.display());
    }
}

/// Send `POST /api/routes?gossip=false` to each peer mentioned in the route
/// (other than self) so they spin up their side of the wire. The
/// `gossip=false` query stops them from bouncing it back and creating a
/// loop.
async fn gossip_add(state: &Arc<EngineState>, route: &Route) {
    let targets = gossip_targets(state, route);
    let body = match serde_json::to_string(route) {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!("gossip: serialize route: {err}");
            return;
        }
    };
    for (addr, port) in targets {
        let body = body.clone();
        tokio::task::spawn_blocking(move || {
            let url = format!("http://{addr}:{port}/api/routes?gossip=false");
            match ureq::post(&url)
                .set("content-type", "application/json")
                .timeout(Duration::from_secs(3))
                .send_string(&body)
            {
                Ok(_) => {}
                Err(err) => tracing::warn!("gossip add to {addr}:{port} failed: {err}"),
            }
        });
    }
}

async fn gossip_remove(state: &Arc<EngineState>, route: &Route) {
    let targets = gossip_targets(state, route);
    let id = route.id.clone();
    for (addr, port) in targets {
        let id = id.clone();
        tokio::task::spawn_blocking(move || {
            let url = format!("http://{addr}:{port}/api/routes/{id}?gossip=false");
            let _ = ureq::delete(&url)
                .timeout(Duration::from_secs(3))
                .call();
        });
    }
}

fn gossip_targets(state: &Arc<EngineState>, route: &Route) -> Vec<(String, u16)> {
    let self_id = &state.identity.node_id;
    let mut out = Vec::new();
    for id in [&route.src.node_id, &route.dst.node_id] {
        if id == self_id {
            continue;
        }
        if let Some(peer) = state.peers.get(id) {
            let entry = (peer.node.addr.clone(), peer.node.port);
            if !out.contains(&entry) {
                out.push(entry);
            }
        }
    }
    out
}

fn routes_equal(a: &Route, b: &Route) -> bool {
    // Serialize both to JSON and compare — cheap and honest.
    serde_json::to_string(a).ok() == serde_json::to_string(b).ok()
}

/// Every 200ms, sample the live stats atomics for every running Route and
/// broadcast a `Stats` message.
pub fn spawn_stats_pump(state: Arc<EngineState>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(200));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if state.events.receiver_count() == 0 {
                continue;
            }
            let mut map = std::collections::HashMap::new();
            for entry in state.running.iter() {
                let route_id = entry.key().clone();
                let running = entry.value();
                let level = running
                    .level_bits
                    .as_ref()
                    .map(|b| f32::from_bits(b.load(Ordering::Relaxed)))
                    .unwrap_or(0.0);
                let xruns = running
                    .xruns
                    .as_ref()
                    .map(|c| c.load(Ordering::Relaxed) as u32)
                    .unwrap_or(0);
                let e2e_ms = running
                    .e2e_ns
                    .as_ref()
                    .map(|n| n.load(Ordering::Relaxed) as f32 / 1_000_000.0)
                    .unwrap_or(0.0);
                let stats = StreamStats {
                    xruns,
                    jitter_ms: 0.0,
                    level_db: if level > 0.0 { 20.0 * level.log10() } else { -120.0 },
                    e2e_latency_ms: e2e_ms,
                };
                map.insert(route_id, stats);
            }
            if !map.is_empty() {
                let _ = state.events.send(ServerMsg::Stats { stats: map });
            }
        }
    });
}

pub async fn shutdown_all(state: &Arc<EngineState>) {
    let ids: Vec<_> = state.running.iter().map(|e| e.key().clone()).collect();
    for id in ids {
        if let Some((_, running)) = state.running.remove(&id) {
            shutdown_running(running);
        }
    }
}

fn shutdown_running(running: RunningRoute) {
    // Stop transport first so we don't keep pumping into stale audio workers,
    // then close the audio side.
    if let Some(h) = running.tx {
        h.stop_and_join();
    }
    if let Some(h) = running.cap {
        h.stop_and_join();
    }
    if let Some(h) = running.rx {
        h.stop_and_join();
    }
    if let Some(h) = running.pb {
        h.stop_and_join();
    }
}

#[cfg(test)]
mod tests {
    use super::route_port;

    #[test]
    fn route_port_is_deterministic_and_in_range() {
        let id = "abcd-1234";
        let a = route_port(10_001, id);
        let b = route_port(10_001, id);
        assert_eq!(a, b, "same id must produce same port on both engines");
        assert!(a >= 10_003 && a <= 10_001 + 2 + 998 * 2, "port {a} out of range");
    }

    #[test]
    fn route_port_leaves_room_for_repair() {
        // Repair packets take audio_port + 1, so per-route ports must be even
        // offsets to avoid the repair port clashing with the next route's
        // source port.
        for id in ["r1", "r2", "12345", "550e8400-e29b-41d4-a716-446655440000"] {
            let p = route_port(10_001, id);
            assert_eq!((p - 10_001) % 2, 0, "port for {id} = {p} is not even offset");
        }
    }
}
