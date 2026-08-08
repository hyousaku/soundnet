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
//! * Otherwise the Route belongs to two other peers — ignore it.
//!
//! Routes are applied by both endpoints independently. The control plane
//! is just a synchronisation layer; each engine is authoritative for its
//! own hardware.

use anyhow::{anyhow, Context, Result};
use once_cell::sync::Lazy;
use soundnet_protocol::{Route, ServerMsg};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::audio::{
    capture::{self, CaptureControl},
    playback::{self, PlaybackControl},
};
use crate::state::EngineState;
use crate::transport::{receiver as rx, sender as tx, RocContext};

/// Live handles for a Route on this engine.
pub struct RunningRoute {
    pub cap: Option<CaptureControl>,
    pub tx: Option<tx::SenderHandle>,
    pub rx: Option<rx::ReceiverHandle>,
    pub pb: Option<PlaybackControl>,
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

pub async fn apply_route(state: &Arc<EngineState>, route: Route) -> Result<()> {
    // Idempotent — remove any existing running instance first.
    if let Some((_, existing)) = state.running.remove(&route.id) {
        shutdown_running(existing);
    }

    let self_id = &state.identity.node_id;
    let mut running = RunningRoute { cap: None, tx: None, rx: None, pb: None };

    if route.src.node_id == *self_id {
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

        let cap = capture::spawn(&port.alsa_name, &route.spec)?;
        let ctx = roc_context().await?;
        let sender = tx::spawn(
            ctx,
            &dst_node.addr,
            dst_node.audio_port,
            &route.spec,
            cap.consumer,
        )?;
        running.cap = Some(cap.control);
        running.tx = Some(sender);
    }

    if route.dst.node_id == *self_id {
        let port = state
            .local_ports
            .get(&route.dst.port_id)
            .map(|p| p.clone())
            .ok_or_else(|| anyhow!("unknown local dst port {}", route.dst.port_id))?;
        let ctx = roc_context().await?;
        let pb = playback::spawn(&port.alsa_name, &route.spec)?;
        let receiver = rx::spawn(
            ctx,
            "0.0.0.0",
            state.identity.audio_port,
            &route.spec,
            pb.producer,
        )?;
        running.rx = Some(receiver);
        running.pb = Some(pb.control);
    }

    state.routes.insert(route.id.clone(), route.clone());
    state.running.insert(route.id.clone(), running);

    let _ = state.events.send(ServerMsg::RouteAdded { route });
    Ok(())
}

pub async fn remove_route(state: &Arc<EngineState>, id: &str) {
    if let Some((_, running)) = state.running.remove(id) {
        shutdown_running(running);
    }
    state.routes.remove(id);
    state.stats.remove(id);
    let _ = state.events.send(ServerMsg::RouteRemoved { id: id.to_string() });
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
    if let Some(h) = running.cap {
        h.stop_and_join();
    }
    if let Some(h) = running.tx {
        h.stop_and_join();
    }
    if let Some(h) = running.rx {
        h.stop_and_join();
    }
    if let Some(h) = running.pb {
        h.stop_and_join();
    }
}
