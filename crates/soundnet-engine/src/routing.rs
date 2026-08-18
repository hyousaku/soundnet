//! Wire up local capture → remote sender, and remote receiver → local
//! playback, one Route at a time.
//!
//! Each engine decides its own responsibility based on where it sits in the
//! Route:
//!
//! * If `route.src.node_id == self`, spawn a send pipeline: one thread that
//!   reads the local device (ALSA or tone) and streams to the destination.
//! * If `route.dst.node_id == self`, spawn a receive pipeline: one thread
//!   bound to our audio port that plays into the local device.
//! * Otherwise the Route belongs to two other peers — nothing to do locally,
//!   but the route still lives in state so the UI can render it.
//!
//! Route add/remove is gossiped between engines over HTTP so both endpoints
//! spin up their side without the browser having to talk to both.

use anyhow::{anyhow, bail, Context, Result};
use once_cell::sync::Lazy;
use soundnet_protocol::{PortKind, Route, RouteHealth, SampleFormat, ServerMsg, StreamStats};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::config::Config;
use crate::pipeline::{recv, send};
use crate::state::EngineState;
use crate::transport::RocContext;

/// Live handles for a Route on this engine.
pub struct RunningRoute {
    /// Set when this engine is the route's source (capture/tone -> network).
    pub send: Option<send::SendHandle>,
    /// Set when this engine is the route's destination (network -> playback).
    pub recv: Option<recv::RecvHandle>,
    /// Peak level of what this engine put on the wire, set only when it
    /// holds the route's capture (or tone) side.
    pub cap_level_bits: Option<Arc<AtomicU32>>,
    /// Peak level of what this engine played out, set only when it holds the
    /// route's playback side. Kept separate from the capture level for the
    /// reason spelled out on `StreamStats::capture_level_db`: a browser sees
    /// one engine, and one engine usually holds one end.
    pub pb_level_bits: Option<Arc<AtomicU32>>,
    pub xruns: Option<Arc<AtomicUsize>>,
    pub e2e_ns: Option<Arc<AtomicU64>>,
    pub jitter_ns: Option<Arc<AtomicU64>>,
    /// ALSA capture-buffer delay, only ever set when this engine holds the
    /// route's capture side (see the honesty note on `StreamStats`).
    pub cap_buffer_ns: Option<Arc<AtomicU64>>,
    /// ALSA playback-buffer delay, only ever set when this engine holds the
    /// route's playback side.
    pub pb_buffer_ns: Option<Arc<AtomicU64>>,
    /// Format the capture device was actually opened with (see the note on
    /// `StreamStats::capture_format`).
    pub cap_format: Option<Arc<AtomicU8>>,
    /// Format the playback device was actually opened with.
    pub pb_format: Option<Arc<AtomicU8>>,
    /// Capture-side xrun counter, only set when this engine holds the
    /// route's capture side.
    pub cap_xruns: Option<Arc<AtomicUsize>>,
    /// Clamped-sample counter, only set when this engine holds the route's
    /// playback side.
    pub clipped: Option<Arc<AtomicUsize>>,
}

impl RunningRoute {
    /// A pipeline thread can outlive its usefulness silently: `send::spawn`
    /// and `recv::spawn` return `Ok` the moment the OS thread is created,
    /// well before the ALSA or roc call inside it has had a chance to fail.
    /// `JoinHandle::is_finished` is the only way to notice after the fact
    /// that a "running" route's pipeline actually died.
    fn is_dead(&self) -> bool {
        self.send
            .as_ref()
            .map(|s| s.thread.is_finished())
            .unwrap_or(false)
            || self
                .recv
                .as_ref()
                .map(|r| r.thread.is_finished())
                .unwrap_or(false)
    }

    /// Why the pipeline stopped, as reported by the thread itself.
    ///
    /// "worker exited unexpectedly" was the only thing the UI used to say,
    /// which names the symptom and withholds every fact needed to act: an
    /// ALSA device held by PipeWire, a format no device would accept and a
    /// UDP port already bound all looked identical, and the operator had to
    /// go read the journal to find out which. The thread already knows.
    fn failure_reason(&self) -> String {
        let from = |slot: &Option<Arc<std::sync::Mutex<Option<String>>>>| -> Option<String> {
            // `ok()?` here would have thrown the reason away exactly when a
            // thread had panicked — turning the most interesting failure into
            // "stopped without reporting an error". Same reasoning as the
            // writers: see the doc on `SendHandle::last_error`.
            slot.as_ref()?
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        };
        let send = from(&self.send.as_ref().map(|s| s.last_error.clone()));
        let recv = from(&self.recv.as_ref().map(|r| r.last_error.clone()));
        match (send, recv) {
            (Some(s), Some(r)) => format!("send: {s}; recv: {r}"),
            (Some(s), None) => s,
            (None, Some(r)) => r,
            // The thread finished without recording anything: it returned
            // Ok, which for these loops only happens on a stop request. Say
            // so rather than inventing a cause.
            (None, None) => "pipeline stopped without reporting an error".to_string(),
        }
    }

    /// Raise the stop flag on both halves without waiting for either. Cheap
    /// and non-blocking — the threads only act on it when they next come
    /// around their loops.
    fn request_stop(&self) {
        if let Some(h) = self.send.as_ref() {
            h.request_stop();
        }
        if let Some(h) = self.recv.as_ref() {
            h.request_stop();
        }
    }
}

/// Backoff bookkeeping for a route this engine has a local role in but
/// couldn't (or couldn't keep) running. Kept separate from `RunningRoute`
/// because it must persist across the route *not* being in `state.running`
/// at all (e.g. the peer has never been discovered).
pub struct RouteFailure {
    attempts: u32,
    reason: String,
    next_retry_at: Instant,
}

impl RouteFailure {
    fn to_health(&self) -> RouteHealth {
        RouteHealth::Retrying {
            attempts: self.attempts,
            reason: self.reason.clone(),
            next_retry_ms: self
                .next_retry_at
                .saturating_duration_since(Instant::now())
                .as_millis() as u64,
        }
    }
}

/// Exponential backoff for repeated route-start failures: doubles from 2s,
/// capped at 60s so a route with a persistent problem (bad format, unplugged
/// device, peer that's gone for good) stops burning CPU/threads on every
/// mDNS resolve, while still trying again eventually in case the operator
/// fixes it — without needing to delete and recreate the route.
fn backoff_for_attempts(attempts: u32) -> Duration {
    let shift = attempts.saturating_sub(1).min(5); // 2,4,8,16,32,60(capped)
    let secs = 2u64.checked_shl(shift).unwrap_or(u64::MAX);
    Duration::from_secs(secs.min(60))
}

/// Record (or extend) a failure for `id`, bumping its attempt count and
/// pushing `next_retry_at` out by the backoff for that attempt count.
fn record_failure(state: &Arc<EngineState>, id: &str, reason: &str) {
    let mut entry = state
        .failures
        .entry(id.to_string())
        .or_insert_with(|| RouteFailure {
            attempts: 0,
            reason: String::new(),
            next_retry_at: Instant::now(),
        });
    entry.attempts += 1;
    entry.reason = reason.to_string();
    entry.next_retry_at = Instant::now() + backoff_for_attempts(entry.attempts);
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
/// needing to negotiate it. Each route now claims *three* consecutive
/// ports: the value returned here (source), `+1` (FEC repair), and `+2`
/// (RTCP control — see transport/sender.rs and transport/receiver.rs). The
/// stride between routes therefore has to be (at least) 3, not 2: at
/// stride 2, a route's control port at `+2` would land exactly on the next
/// route's source port, silently pulling audio into what's supposed to be
/// a control channel. Widening the stride to 3 keeps each route's 3-port
/// window disjoint from its neighbors' by construction.
///
/// Range: `audio_port + 3 * (1..=1000)` — that's still 1000 concurrent
/// inbound routes per engine before same-bucket collisions become likely
/// (see `route_ports_never_partially_overlap` for what "collide" does and
/// doesn't mean here).
///
/// This is a pure function of `route_id` and the peer's advertised
/// `audio_port` — it has to stay that way, since both engines derive it
/// independently with no negotiation.
pub fn route_port(audio_port_base: u16, route_id: &str) -> u16 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    route_id.hash(&mut hasher);
    let h = hasher.finish();
    audio_port_base.saturating_add(3 + ((h % 1000) as u16) * 3)
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
    validate_route(state, &route)?;

    let is_new_or_changed = state
        .routes
        .get(&route.id)
        .map(|r| !routes_equal(&r, &route))
        .unwrap_or(true);

    state.routes.insert(route.id.clone(), route.clone());
    persist(state).await;

    // If the running config no longer matches (or nothing was running yet),
    // reboot the workers. Also clear any backoff from a prior failure — the
    // operator just changed something (e.g. picked a format the device
    // actually supports), so this deserves an immediate attempt rather than
    // waiting out whatever window the last failure scheduled.
    if is_new_or_changed {
        {
            // One critical section for the whole reboot, not one per step.
            // Between the `remove` and the start there are several awaits,
            // and a supervisor tick landing in that window would find
            // nothing running and start the route itself — from the route
            // it just read out of `state.routes`, i.e. the new spec, using
            // the device this teardown has not finished releasing.
            let lock = state.route_lock(&route.id);
            let _guard = lock.lock().await;
            if let Some((_, existing)) = state.running.remove(&route.id) {
                shutdown_running(existing).await;
            }
            state.failures.remove(&route.id);
            if let Err(err) = start_locked(state, &route).await {
                tracing::warn!("route {} could not start yet: {err:#}", route.id);
            }
        }
        if gossip {
            // Outside the lock: this is HTTP to another host with a 3s
            // timeout, and nothing about it touches our pipelines.
            gossip_add(state, &route).await;
        }
    }

    let _ = state.events.send(ServerMsg::RouteAdded { route });
    Ok(())
}

/// Reject obviously invalid routes before they touch any hardware.
fn validate_route(state: &Arc<EngineState>, route: &Route) -> Result<()> {
    // If the port is on THIS engine we can check its kind — Tone/Capture
    // ports are inputs only, Playback ports are outputs only. For ports on
    // remote peers we trust the peer to reject bogus requests.
    if is_local_src(state, route) {
        if let Some(p) = state.local_ports.get(&route.src.port_id) {
            if matches!(p.kind, PortKind::Playback) {
                bail!(
                    "src port {} is a Playback port; sources must be Capture or Tone",
                    route.src.port_id
                );
            }
        }
    }
    if is_local_dst(state, route) {
        if let Some(p) = state.local_ports.get(&route.dst.port_id) {
            if !matches!(p.kind, PortKind::Playback) {
                bail!(
                    "dst port {} is a {:?} port; destinations must be Playback",
                    route.dst.port_id,
                    p.kind
                );
            }
        }
    }
    if route.spec.channels == 0 {
        bail!("channels must be >= 1");
    }
    if route.spec.frames_per_period == 0 {
        bail!("frames_per_period must be >= 1");
    }
    Ok(())
}

/// Try to spawn the local workers for this route. Returns an error if the
/// required endpoints aren't currently available (e.g. the peer hasn't been
/// discovered yet); callers should keep the route in state and retry later.
///
/// Throttled: a route that just failed (or whose workers were just found
/// dead) won't be re-attempted again until its backoff window elapses — see
/// `record_failure`/`backoff_for_attempts`. Called both on-demand (peer
/// discovery) and from a periodic sweep (`spawn_route_supervisor`), so this
/// needs to be cheap to call repeatedly when there's nothing to do.
///
/// Serialized per route, and it has to be. Three callers reach this
/// concurrently for the same route id — `apply_route` (UI or gossip),
/// `retry_pending_for_peer` (mDNS resolve) and `spawn_route_supervisor`
/// (every 3s) — and the body reads `state.running`, awaits, and only then
/// writes it back. `try_start_inner` awaits at least twice on the way
/// (`roc_context()`, `selected_interface.read()`), so two callers could both
/// see nothing running, both spawn a full pipeline, and the second `insert`
/// would drop the first `RunningRoute` on the floor.
///
/// That drop is the expensive part. Nothing implements `Drop` on
/// `SendHandle`/`RecvHandle`, so dropping the handle *detaches* its thread:
/// the stop flag goes with it, and the pipeline keeps running forever,
/// holding its `hw:` device against every later attempt to open it and
/// still putting audio on the wire. Serializing is what makes the
/// check-then-insert atomic; per-route rather than global so one device
/// opening slowly doesn't stall the others.
///
/// A skip-guard ("if someone else is starting this, return") would be
/// cheaper and is wrong here: `apply_route` would then be able to skip its
/// own start because a supervisor tick got there first, leaving the route
/// running the spec the operator just changed away from, indefinitely.
/// Waiting costs a few milliseconds; skipping costs a wrong-sounding patch
/// with no error anywhere.
pub async fn try_start(state: &Arc<EngineState>, route: &Route) -> Result<()> {
    let lock = state.route_lock(&route.id);
    let _guard = lock.lock().await;
    start_locked(state, route).await
}

/// The body of `try_start`, for callers that already hold the route's lock.
async fn start_locked(state: &Arc<EngineState>, route: &Route) -> Result<()> {
    // Both retry paths iterate a *snapshot* of `state.routes`, so by the time
    // this runs the operator may have deleted the route — and `remove_route`
    // drops it from `state.routes` before it takes this lock, precisely so
    // that this check sees the deletion. Without it a supervisor tick holding
    // a stale clone would start a pipeline for a route that no longer exists,
    // and nothing would ever stop it again: teardown is driven from
    // `state.routes`.
    if !state.routes.contains_key(&route.id) {
        return Ok(());
    }

    if let Some(entry) = state.running.get(&route.id) {
        let dead = entry.is_dead();
        let dead_reason = if dead {
            Some(entry.failure_reason())
        } else {
            None
        };
        drop(entry);
        if !dead {
            // Confirmed alive as of this check — any backoff from an earlier
            // failure no longer applies.
            state.failures.remove(&route.id);
            return Ok(());
        }
        if let Some((_, dead_route)) = state.running.remove(&route.id) {
            shutdown_running(dead_route).await;
        }
        let reason = dead_reason.unwrap_or_else(|| "pipeline exited".to_string());
        tracing::warn!(
            "route {} pipeline died: {reason}; backing off before retry",
            route.id
        );
        record_failure(state, &route.id, &reason);
    }

    if let Some(failure) = state.failures.get(&route.id) {
        if Instant::now() < failure.next_retry_at {
            return Ok(());
        }
    }

    match try_start_inner(state, route).await {
        Ok(Some(running)) => {
            // Don't clear the failure entry yet: a worker that dies right
            // after spawning (the ALSA-format case) looks identical to a
            // healthy start from here. It's cleared above once a later check
            // finds the route still alive.
            state.running.insert(route.id.clone(), running);
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(err) => {
            record_failure(state, &route.id, &format!("{err:#}"));
            Err(err)
        }
    }
}

/// Does the actual work of spawning local workers for `route`. `Ok(None)`
/// means this route has no local role at all (both endpoints are other
/// peers) — not a failure, nothing to retry.
async fn try_start_inner(state: &Arc<EngineState>, route: &Route) -> Result<Option<RunningRoute>> {
    let mut running = RunningRoute {
        send: None,
        recv: None,
        cap_level_bits: None,
        pb_level_bits: None,
        xruns: None,
        e2e_ns: None,
        jitter_ns: None,
        cap_buffer_ns: None,
        pb_buffer_ns: None,
        cap_format: None,
        pb_format: None,
        cap_xruns: None,
        clipped: None,
    };

    if is_local_src(state, route) {
        let port = state
            .local_ports
            .get(&route.src.port_id)
            .map(|p| p.clone())
            .ok_or_else(|| anyhow!("unknown local src port {}", route.src.port_id))?;
        // Self-loop: dst is us too — send to our own audio port on loopback,
        // no peer lookup needed. (Useful for local tone → local playback tests.)
        let dst_node = if is_local_dst(state, route) {
            state.self_node()
        } else {
            state
                .peers
                .get(&route.dst.node_id)
                .map(|r| r.node.clone())
                .with_context(|| format!("unknown dst peer {}", route.dst.node_id))?
        };

        let dst_port = route_port(dst_node.audio_port, &route.id);
        let ctx = roc_context().await?;
        // Only pin the sender's outgoing interface when the operator
        // explicitly chose one; otherwise leave it to the OS routing table,
        // same as before this feature existed (see transport/sender.rs).
        let outgoing = if state.selected_interface.read().await.is_some() {
            Some(*state.identity.addr.read().unwrap())
        } else {
            None
        };
        let pipeline = send::spawn(
            &port.alsa_name,
            &route.spec,
            ctx,
            &dst_node.addr,
            dst_port,
            outgoing,
            route.src.channel_offset,
        )?;
        running.cap_level_bits = Some(pipeline.level_bits.clone());
        running.cap_buffer_ns = Some(pipeline.buffer_ns.clone());
        running.cap_format = Some(pipeline.format.clone());
        running.cap_xruns = Some(pipeline.xruns.clone());
        running.send = Some(pipeline);
    }

    if is_local_dst(state, route) {
        let port = state
            .local_ports
            .get(&route.dst.port_id)
            .map(|p| p.clone())
            .ok_or_else(|| anyhow!("unknown local dst port {}", route.dst.port_id))?;
        let bind_port = route_port(state.identity.audio_port, &route.id);
        let ctx = roc_context().await?;
        let pipeline = recv::spawn(
            &port.alsa_name,
            &route.spec,
            ctx,
            "0.0.0.0",
            bind_port,
            route.dst.channel_offset,
        )?;
        running.pb_level_bits = Some(pipeline.level_bits.clone());
        running.xruns = Some(pipeline.xruns.clone());
        running.e2e_ns = Some(pipeline.e2e_ns.clone());
        running.jitter_ns = Some(pipeline.jitter_ns.clone());
        running.pb_buffer_ns = Some(pipeline.buffer_ns.clone());
        running.pb_format = Some(pipeline.format.clone());
        running.clipped = Some(pipeline.clipped.clone());
        running.recv = Some(pipeline);
    }

    if running.send.is_some() || running.recv.is_some() {
        Ok(Some(running))
    } else {
        Ok(None)
    }
}

/// After a new peer is discovered, walk existing routes and (re-)try any
/// that reference it. `try_start` is cheap to call when there's nothing to
/// do (already running and healthy, or still backing off from a recent
/// failure), so no need to pre-filter beyond the peer match.
pub async fn retry_pending_for_peer(state: &Arc<EngineState>, peer_id: &str) {
    let candidates: Vec<Route> = state
        .routes
        .iter()
        .filter(|entry| {
            let r = entry.value();
            r.src.node_id == peer_id || r.dst.node_id == peer_id
        })
        .map(|e| e.value().clone())
        .collect();
    for r in candidates {
        if let Err(err) = try_start(state, &r).await {
            tracing::debug!("retry_pending still waiting on route {}: {err:#}", r.id);
        }
    }
}

/// Periodic safety net so a failing route recovers on its own once its
/// underlying problem clears (device replugged, format changed in the UI,
/// peer's engine restarted) without requiring a fresh mDNS resolve to drive
/// `retry_pending_for_peer` — discovery only calls that for routes touching
/// the specific peer that was just (re-)resolved. Each tick's calls are
/// throttled the same way as any other `try_start` call, so this stays cheap
/// even with many routes.
pub fn spawn_route_supervisor(state: Arc<EngineState>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(3));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let routes: Vec<Route> = state.routes.iter().map(|e| e.value().clone()).collect();
            for route in routes {
                if let Err(err) = try_start(&state, &route).await {
                    tracing::debug!(
                        "route supervisor: route {} still failing: {err:#}",
                        route.id
                    );
                }
            }
        }
    });
}

pub async fn remove_route(state: &Arc<EngineState>, id: &str, gossip: bool) {
    // Drop it from `state.routes` *before* taking the lock. Anyone already
    // queued on the lock to start this route re-checks `state.routes` first
    // thing (see `start_locked`), so doing it in this order means the
    // deletion wins the race instead of being overwritten by a start that
    // was decided a moment earlier.
    let route = state.routes.remove(id).map(|(_, r)| r);
    {
        let lock = state.route_lock(id);
        let _guard = lock.lock().await;
        if let Some((_, running)) = state.running.remove(id) {
            shutdown_running(running).await;
        }
        state.failures.remove(id);
    }
    state.stats.remove(id);
    persist(state).await;
    if gossip {
        if let Some(route) = route {
            gossip_remove(state, &route).await;
        }
    }
    let _ = state
        .events
        .send(ServerMsg::RouteRemoved { id: id.to_string() });
}

/// Snapshot in-memory state to the configured TOML file. Best-effort — a
/// failure to write is logged but never fails the caller.
pub async fn persist(state: &Arc<EngineState>) {
    let path = state.config_path.read().await.clone();
    let Some(path) = path else { return };
    let cfg = Config {
        // The identity is set once at startup (see main.rs) and never
        // changes at runtime, but persist() rebuilds the whole Config from
        // scratch — leaving this out would silently erase the stored
        // node_id the next time a route or manual host change triggers a
        // save, reintroducing the "new UUID every restart" bug.
        node_id: Some(state.identity.node_id.clone()),
        // Same trap as node_id: this rebuilds the whole Config from live
        // state on every route/manual-host change, so a field left out here
        // gets silently wiped back to its default the next time the operator
        // does anything else — see the interface field's own comment on
        // `Config` and the `interface_round_trips_through_save_and_load` test.
        interface: state.selected_interface.read().await.clone(),
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
            let _ = ureq::delete(&url).timeout(Duration::from_secs(3)).call();
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

/// Convert a nanosecond atomic reading to milliseconds, treating `u64::MAX`
/// as the shared sentinel worker threads use for "nothing sampled yet"
/// (see the doc comments on `ReceiverHandle::e2e_latency_ns` and
/// `CaptureHandle`/`PlaybackHandle::buffer_ns`). A real latency or buffer
/// depth is never anywhere close to `u64::MAX` ns (~584 years), so it's an
/// otherwise-unused corner of the value space rather than a plausible
/// duration — using it instead of 0 matters here specifically because 0 is
/// a value these metrics can genuinely take (an empty ALSA buffer, a
/// freshly-connected RTCP session), so it can't double as "no data".
/// Read back a format an audio thread published. `None` covers both "this
/// engine has no device on that side of the route" and "the device isn't
/// open yet" — and, permanently, a tone source, which negotiates with
/// nothing. See `StreamStats::capture_format`.
fn atomic_format(atomic: &Arc<AtomicU8>) -> Option<SampleFormat> {
    SampleFormat::from_u8(atomic.load(Ordering::Relaxed))
}

fn ns_to_ms(atomic: &Arc<AtomicU64>) -> Option<f32> {
    let raw = atomic.load(Ordering::Relaxed);
    if raw == u64::MAX {
        None
    } else {
        Some(raw as f32 / 1_000_000.0)
    }
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
                // `None` means "this engine holds no such side", which the UI
                // must not draw as a meter pinned at the bottom — see the doc
                // on `StreamStats::capture_level_db`.
                let level_db = |slot: &Option<Arc<AtomicU32>>| -> Option<f32> {
                    let peak = f32::from_bits(slot.as_ref()?.load(Ordering::Relaxed));
                    Some(if peak > 0.0 {
                        20.0 * peak.log10()
                    } else {
                        -120.0
                    })
                };
                let capture_level_db = level_db(&running.cap_level_bits);
                let playback_level_db = level_db(&running.pb_level_bits);
                // Both directions, because a route can glitch on either and
                // for a long time only the playback side was counted — a
                // route dropping capture periods reported a clean zero.
                let capture_xruns = running
                    .cap_xruns
                    .as_ref()
                    .map(|c| c.load(Ordering::Relaxed) as u32);
                let playback_xruns = running
                    .xruns
                    .as_ref()
                    .map(|c| c.load(Ordering::Relaxed) as u32);
                let xruns = capture_xruns.unwrap_or(0) + playback_xruns.unwrap_or(0);
                let jitter_ms = running
                    .jitter_ns
                    .as_ref()
                    .map(|n| n.load(Ordering::Relaxed) as f32 / 1_000_000.0)
                    .unwrap_or(0.0);
                // Each of these is only ever populated on the engine that
                // holds the corresponding half of the route — see the
                // honesty note on StreamStats. `None` covers two distinct
                // cases the browser can't tell apart from here (this engine
                // has no role for that component at all, vs. it has the
                // role but nothing's been sampled yet) — both mean "don't
                // show a number", so collapsing them is fine.
                let roc_e2e_ms = running.e2e_ns.as_ref().and_then(ns_to_ms);
                let capture_buffer_ms = running.cap_buffer_ns.as_ref().and_then(ns_to_ms);
                let playback_buffer_ms = running.pb_buffer_ns.as_ref().and_then(ns_to_ms);
                let clipped_samples = running
                    .clipped
                    .as_ref()
                    .map(|c| c.load(Ordering::Relaxed) as u32);
                let capture_format = running.cap_format.as_ref().and_then(atomic_format);
                let playback_format = running.pb_format.as_ref().and_then(atomic_format);
                // A worker can have died since the last try_start check (that
                // only happens on the next discovery event or supervisor
                // tick) — report it as retrying immediately rather than
                // waiting for eviction to catch up.
                let health = if running.is_dead() {
                    state
                        .failures
                        .get(&route_id)
                        .map(|f| f.to_health())
                        .unwrap_or(RouteHealth::Ok)
                } else {
                    RouteHealth::Ok
                };
                let stats = StreamStats {
                    xruns,
                    jitter_ms,
                    capture_level_db,
                    playback_level_db,
                    health,
                    roc_e2e_ms,
                    capture_buffer_ms,
                    playback_buffer_ms,
                    capture_format,
                    playback_format,
                    capture_xruns,
                    playback_xruns,
                    clipped_samples,
                };
                map.insert(route_id, stats);
            }
            // Routes that never made it into `running` at all (peer never
            // discovered, or evicted after dying and not yet retried) still
            // need to be visible — otherwise they just look identical to a
            // route this engine has no local role in.
            for entry in state.failures.iter() {
                let route_id = entry.key().clone();
                if map.contains_key(&route_id) {
                    continue;
                }
                map.insert(
                    route_id,
                    StreamStats {
                        xruns: 0,
                        jitter_ms: 0.0,
                        capture_level_db: None,
                        playback_level_db: None,
                        health: entry.value().to_health(),
                        roc_e2e_ms: None,
                        capture_buffer_ms: None,
                        playback_buffer_ms: None,
                        capture_format: None,
                        playback_format: None,
                        capture_xruns: None,
                        playback_xruns: None,
                        clipped_samples: None,
                    },
                );
            }
            if !map.is_empty() {
                let _ = state.events.send(ServerMsg::Stats { stats: map });
            }
        }
    });
}

pub async fn shutdown_all(state: &Arc<EngineState>) {
    // Two passes, deliberately: raise every stop flag first, then wait.
    //
    // Joining route by route serializes the teardown. A pipeline only
    // notices the request when it next comes around its loop, so asking one
    // route at a time costs one period-wait *per route*, in sequence —
    // and `iface::set_selected` tears everything down on an interface
    // change, so that would get slower in proportion to how much is patched.
    // Flagging everything up front lets the threads wind down concurrently,
    // which makes the total wait roughly the slowest single device instead
    // of the sum of all of them.
    //
    // This is only safe because each pipeline owns its own device and its
    // own roc endpoint: there is no ordering to preserve between them, which
    // is exactly the property the merged send/recv pipelines were built for.
    let ids: Vec<_> = state.running.iter().map(|e| e.key().clone()).collect();
    let mut draining = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some((_, running)) = state.running.remove(&id) {
            running.request_stop();
            draining.push(running);
        }
    }
    if draining.is_empty() {
        return;
    }
    // One blocking-pool thread joins them all: they are already winding down
    // in parallel, so a task each would buy nothing.
    let _ = tokio::task::spawn_blocking(move || {
        for running in draining {
            join_running(running);
        }
    })
    .await;
}

/// Stop both halves of a route and wait for their threads to be gone.
///
/// The wait is real. `stop_and_join` ends in `std::thread::join`, and the
/// thread it waits on is sitting inside a blocking ALSA call
/// (`snd_pcm_readi` / `snd_pcm_writei`), so this returns no sooner than the
/// device's next period — and if the device has stopped answering (a USB
/// interface unplugged mid-stream, a driver wedged in D state), not at all.
///
/// That wait must not happen on the async executor. Every caller of this is
/// an axum handler or a tokio task, and blocking one of those parks a
/// runtime worker for the duration: at best a few milliseconds stolen from
/// the request path, at worst one of the runtime's handful of workers is
/// gone permanently and deleting a single route from the UI takes the
/// engine's whole control plane with it. `gossip_add`/`gossip_remove`
/// already push their blocking `ureq` calls onto the blocking pool for
/// exactly this reason; this is the same move.
///
/// What this does **not** fix: the audio thread's own stop latency is still
/// unbounded. A wedged device now wedges a blocking-pool thread instead of a
/// runtime worker — a much softer failure, since that pool exists to absorb
/// open-ended waits and grows on demand while the async side keeps serving —
/// but bounding how long a pipeline can take to notice `stop` is a change to
/// the loops themselves, not to their callers.
///
/// Awaited rather than detached, on purpose: `apply_route` re-opens the same
/// ALSA device immediately afterwards, and `hw:` devices are handed out
/// exclusively. Letting the teardown race the new `snd_pcm_open` would turn
/// every format or period change into an intermittent "device or resource
/// busy" that only shows up under timing luck.
async fn shutdown_running(running: RunningRoute) {
    running.request_stop();
    let _ = tokio::task::spawn_blocking(move || join_running(running)).await;
}

/// The blocking half of `shutdown_running`, kept separate so `shutdown_all`
/// can drain many routes on a single blocking-pool thread. Never call this
/// from an async context.
fn join_running(running: RunningRoute) {
    // Each pipeline owns its device and its roc endpoint, so stopping the
    // thread is the whole teardown — no ordering to get right between a
    // transport worker and the audio worker feeding it any more.
    if let Some(h) = running.send {
        h.stop_and_join();
    }
    if let Some(h) = running.recv {
        h.stop_and_join();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_route, backoff_for_attempts, record_failure, remove_route, retry_pending_for_peer,
        route_port, try_start, validate_route, RunningRoute,
    };
    use crate::state::{EngineIdentity, EngineState};
    use soundnet_protocol::{
        Encoding, LocalPort, PortKind, PortRef, Route, RouteHealth, SampleFormat, StreamSpec,
        StreamStats,
    };
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::Arc;
    use std::time::Duration;

    // ---- fixtures ---------------------------------------------------
    //
    // Everything below runs with no sound card, no peer and no libroc, and
    // that is not a compromise — it is a property of the code under test.
    // `EngineState::new` allocates maps and a broadcast channel and nothing
    // else, and `try_start_inner` looks up the local port and the destination
    // peer *before* it asks for a roc context. So a route whose port or peer
    // is unknown exercises the whole failure-and-backoff path and stops short
    // of the one dependency a test machine cannot have.
    //
    // If a later change moves `roc_context()` above those lookups, these
    // tests will start trying to open a real roc context, and this comment is
    // the explanation of why they suddenly need a library.

    const SELF_ID: &str = "self-node";
    const PEER_A: &str = "peer-a";
    const PEER_B: &str = "peer-b";

    fn engine() -> Arc<EngineState> {
        EngineState::new(EngineIdentity {
            node_id: SELF_ID.to_string(),
            hostname: "test-host".to_string(),
            addr: std::sync::RwLock::new(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            control_port: 7788,
            audio_port: 10_001,
            version: "test".to_string(),
        })
    }

    fn spec() -> StreamSpec {
        StreamSpec {
            encoding: Encoding::Pcm,
            rate: 48_000,
            channels: 2,
            frames_per_period: 128,
            alsa_format: SampleFormat::S16Le,
            target_latency_ms: 10,
            fec: false,
        }
    }

    fn port_ref(node: &str, port: &str) -> PortRef {
        PortRef {
            node_id: node.to_string(),
            port_id: port.to_string(),
            channel_offset: 0,
        }
    }

    fn route(id: &str, src: (&str, &str), dst: (&str, &str)) -> Route {
        Route {
            id: id.to_string(),
            src: port_ref(src.0, src.1),
            dst: port_ref(dst.0, dst.1),
            spec: spec(),
        }
    }

    /// A route between two *other* peers. This engine has no role in it, so
    /// `try_start` returns without touching a device — which leaves the
    /// bookkeeping in `apply_route`/`remove_route` as the only thing the test
    /// is observing.
    fn foreign_route(id: &str) -> Route {
        route(id, (PEER_A, "out"), (PEER_B, "in"))
    }

    fn local_port(id: &str, kind: PortKind) -> LocalPort {
        LocalPort {
            node_id: SELF_ID.to_string(),
            id: id.to_string(),
            kind,
            alsa_name: format!("hw:{id}"),
            label: id.to_string(),
            max_channels: 2,
            probe_failed: false,
            supported_formats: vec![SampleFormat::S16Le],
            supported_rates: vec![48_000],
        }
    }

    /// A `RunningRoute` with no threads behind it, standing in for a live
    /// pipeline.
    ///
    /// Every field is optional, so this costs nothing to build: `is_dead()`
    /// reads `false` for it and `shutdown_running` finds nothing to join.
    /// What the tests watch is whether the state machine *removed the entry*
    /// at the right moment — the part that has gone wrong before. Whether a
    /// thread actually stopped is the subject of the teardown work, and is
    /// not something a unit test can honestly claim to have checked.
    fn idle_running() -> RunningRoute {
        RunningRoute {
            send: None,
            recv: None,
            cap_level_bits: None,
            pb_level_bits: None,
            xruns: None,
            e2e_ns: None,
            jitter_ns: None,
            cap_buffer_ns: None,
            pb_buffer_ns: None,
            cap_format: None,
            pb_format: None,
            cap_xruns: None,
            clipped: None,
        }
    }

    fn stats_sentinel() -> StreamStats {
        StreamStats {
            xruns: 7,
            jitter_ms: 0.0,
            capture_level_db: None,
            playback_level_db: None,
            health: RouteHealth::Ok,
            roc_e2e_ms: None,
            capture_buffer_ms: None,
            playback_buffer_ms: None,
            capture_format: None,
            playback_format: None,
            capture_xruns: None,
            playback_xruns: None,
            clipped_samples: None,
        }
    }

    // ---- the state machine ------------------------------------------

    /// Ports have a direction, and a route that ignores it would open a
    /// playback device for reading — an error from ALSA much later, with
    /// nothing pointing back at the patch that caused it.
    #[test]
    fn a_route_cannot_run_backwards_through_a_local_port() {
        let state = engine();
        state
            .local_ports
            .insert("out".to_string(), local_port("out", PortKind::Playback));
        state
            .local_ports
            .insert("in".to_string(), local_port("in", PortKind::Capture));
        state
            .local_ports
            .insert("tone".to_string(), local_port("tone", PortKind::Tone));

        let err = validate_route(&state, &route("r", (SELF_ID, "out"), (PEER_A, "x")))
            .expect_err("a Playback port must not be accepted as a source");
        assert!(
            format!("{err:#}").contains("Playback"),
            "the error should name the problem, got: {err:#}"
        );

        let err = validate_route(&state, &route("r", (PEER_A, "x"), (SELF_ID, "in")))
            .expect_err("a Capture port must not be accepted as a destination");
        assert!(
            format!("{err:#}").contains("Playback"),
            "the error should say what a destination has to be, got: {err:#}"
        );

        // The directions that do make sense, including a tone as a source.
        validate_route(&state, &route("r", (SELF_ID, "in"), (SELF_ID, "out"))).unwrap();
        validate_route(&state, &route("r", (SELF_ID, "tone"), (SELF_ID, "out"))).unwrap();
        // A port on a peer is that peer's business to validate; we only know
        // its id here, so this must not be rejected locally.
        validate_route(
            &state,
            &route("r", (PEER_A, "anything"), (PEER_B, "anything")),
        )
        .unwrap();
    }

    /// Re-sending a route the engine already has must not touch the workers.
    ///
    /// The UI re-posts whole routes for edits that change nothing about the
    /// stream — the channel-offset control sends the entire route back, and
    /// gossip echoes routes between engines. If every one of those restarted
    /// the pipelines, audio would drop out each time somebody looked at the
    /// patch table.
    #[tokio::test]
    async fn reapplying_an_unchanged_route_leaves_the_workers_alone() {
        let state = engine();
        let r = foreign_route("r1");
        apply_route(&state, r.clone(), false).await.unwrap();

        state.running.insert(r.id.clone(), idle_running());
        apply_route(&state, r.clone(), false).await.unwrap();

        assert!(
            state.running.contains_key(&r.id),
            "an identical route was re-applied and the running pipeline was torn down anyway"
        );
    }

    /// ...and a route that *did* change must restart, and must get an
    /// immediate attempt rather than serving out the backoff from whatever
    /// was wrong before the operator changed it.
    #[tokio::test]
    async fn changing_a_route_tears_down_the_old_workers_and_clears_the_backoff() {
        let state = engine();
        let mut r = foreign_route("r1");
        apply_route(&state, r.clone(), false).await.unwrap();

        state.running.insert(r.id.clone(), idle_running());
        record_failure(&state, &r.id, "device or resource busy");
        assert!(state.failures.contains_key(&r.id), "fixture failed to arm");

        r.spec.rate = 96_000;
        apply_route(&state, r.clone(), false).await.unwrap();

        assert!(
            !state.running.contains_key(&r.id),
            "the pipeline running the old spec must be stopped, not left playing 48k"
        );
        assert!(
            !state.failures.contains_key(&r.id),
            "the operator just changed something; making them wait out the old              backoff is how a fixed route looks broken"
        );
        assert_eq!(state.routes.get(&r.id).unwrap().spec.rate, 96_000);
    }

    /// A peer coming back must not disturb routes that have nothing to do
    /// with it. Retrying everything would reset healthy routes' backoff and,
    /// on a busy patch, restart pipelines that were never affected.
    #[tokio::test]
    async fn a_returning_peer_only_retries_its_own_routes() {
        let state = engine();
        // Both routes name a local source whose port does not exist, so
        // `try_start` fails at the port lookup and records a failure. That is
        // the observable: a route that was retried has a failure entry, one
        // that was skipped has none.
        let mine = route("to-a", (SELF_ID, "missing"), (PEER_A, "in"));
        let other = route("to-b", (SELF_ID, "missing"), (PEER_B, "in"));
        state.routes.insert(mine.id.clone(), mine.clone());
        state.routes.insert(other.id.clone(), other.clone());

        retry_pending_for_peer(&state, PEER_A).await;

        assert!(
            state.failures.contains_key(&mine.id),
            "the route to the peer that just appeared should have been retried"
        );
        assert!(
            !state.failures.contains_key(&other.id),
            "a route to an unrelated peer was retried too"
        );
    }

    /// Removing a route has to empty every map that knows about it. A
    /// leftover entry in `running` holds a device open against the next route
    /// that wants it; one in `failures` makes a deleted route reappear in the
    /// UI as "retrying" forever, because the stats pump reports failures for
    /// routes it can no longer find.
    #[tokio::test]
    async fn removing_a_route_leaves_nothing_behind() {
        let state = engine();
        let r = foreign_route("r1");
        apply_route(&state, r.clone(), false).await.unwrap();
        state.running.insert(r.id.clone(), idle_running());
        record_failure(&state, &r.id, "boom");
        // Nothing populates `state.stats` today — the stats pump broadcasts a
        // map it builds locally. The entry is planted by hand so that the
        // teardown of this map is actually asserted rather than passing by
        // virtue of always being empty.
        state.stats.insert(r.id.clone(), stats_sentinel());

        remove_route(&state, &r.id, false).await;

        assert!(!state.routes.contains_key(&r.id), "routes");
        assert!(!state.running.contains_key(&r.id), "running");
        assert!(!state.failures.contains_key(&r.id), "failures");
        assert!(!state.stats.contains_key(&r.id), "stats");
    }

    /// `retry_pending_for_peer` and the supervisor both iterate a snapshot of
    /// `state.routes`, so a route deleted mid-sweep is still handed to
    /// `try_start` afterwards. Acting on that stale clone spawns a pipeline
    /// for a route nothing knows about any more — and since teardown is
    /// driven from `state.routes`, nothing would ever stop it: the device
    /// stays busy and the audio keeps flowing until the process dies.
    ///
    /// Observable here without a sound card: the route names a local port
    /// that doesn't exist, so a start that got as far as `try_start_inner`
    /// leaves a `failures` entry behind — which is also how a deleted route
    /// comes back in the UI as "retrying" forever.
    #[tokio::test]
    async fn a_route_deleted_mid_sweep_is_not_started_from_the_stale_copy() {
        let state = engine();
        let r = route("r1", (SELF_ID, "missing"), (PEER_A, "in"));
        apply_route(&state, r.clone(), false).await.unwrap();
        state.failures.remove(&r.id);

        // What a sweep is holding when the operator hits delete.
        let stale = r.clone();
        remove_route(&state, &r.id, false).await;
        try_start(&state, &stale).await.unwrap();

        assert!(
            !state.failures.contains_key(&r.id),
            "a deleted route was started again from a stale copy"
        );
        assert!(!state.running.contains_key(&r.id), "running");
        assert!(!state.routes.contains_key(&r.id), "routes");
    }

    /// The check in `try_start` ("is it already running?") and the insert at
    /// the end are separated by several awaits, so without a lock two callers
    /// can both spawn a full pipeline and the loser's handles get dropped —
    /// which detaches its threads, taking the stop flag with them and leaving
    /// an unstoppable pipeline holding the device.
    ///
    /// Proven here by holding the route's lock by hand: a start for that
    /// route must not make progress, and a start for a different route must
    /// not be affected by it.
    #[tokio::test]
    async fn two_starts_of_the_same_route_cannot_overlap() {
        let state = engine();
        let r = route("r1", (SELF_ID, "missing"), (PEER_A, "in"));
        let other = route("r2", (SELF_ID, "missing"), (PEER_A, "in"));
        state.routes.insert(r.id.clone(), r.clone());
        state.routes.insert(other.id.clone(), other.clone());

        let held = state.route_lock(&r.id);
        let guard = held.lock().await;

        let blocked = tokio::time::timeout(Duration::from_millis(200), try_start(&state, &r)).await;
        assert!(
            blocked.is_err(),
            "a second start of the same route ran while the first still held the route"
        );

        tokio::time::timeout(Duration::from_millis(200), try_start(&state, &other))
            .await
            .expect("an unrelated route must not queue behind this one")
            .expect_err("r2's local port does not exist, so its start should fail");

        drop(guard);
        tokio::time::timeout(Duration::from_millis(200), try_start(&state, &r))
            .await
            .expect("the start should proceed once the route is free again")
            .expect_err("r1's local port does not exist either");
    }

    #[test]
    fn backoff_doubles_then_caps() {
        assert_eq!(backoff_for_attempts(1), Duration::from_secs(2));
        assert_eq!(backoff_for_attempts(2), Duration::from_secs(4));
        assert_eq!(backoff_for_attempts(3), Duration::from_secs(8));
        assert_eq!(backoff_for_attempts(4), Duration::from_secs(16));
        assert_eq!(backoff_for_attempts(5), Duration::from_secs(32));
        // 2 * 2^5 = 64, which is where the 60s ceiling kicks in.
        assert_eq!(backoff_for_attempts(6), Duration::from_secs(60));
        assert_eq!(backoff_for_attempts(20), Duration::from_secs(60));
    }

    #[test]
    fn backoff_is_monotonically_nondecreasing() {
        let mut prev = Duration::from_secs(0);
        for attempt in 1..30 {
            let cur = backoff_for_attempts(attempt);
            assert!(cur >= prev, "backoff decreased at attempt {attempt}");
            prev = cur;
        }
    }

    #[test]
    fn route_port_is_deterministic_and_in_range() {
        let id = "abcd-1234";
        let a = route_port(10_001, id);
        let b = route_port(10_001, id);
        assert_eq!(a, b, "same id must produce same port on both engines");
        assert!(
            (10_001 + 3..=10_001 + 3 + 999 * 3).contains(&a),
            "port {a} out of range"
        );
    }

    #[test]
    fn route_port_leaves_room_for_repair_and_control() {
        // A route uses 3 consecutive ports: source (returned here), +1
        // repair, +2 RTCP control. Per-route base offsets must therefore be
        // 3-aligned, or a route's control (or repair) port would clash with
        // a neighboring route's source port.
        for id in ["r1", "r2", "12345", "550e8400-e29b-41d4-a716-446655440000"] {
            let p = route_port(10_001, id);
            assert_eq!(
                (p - 10_001) % 3,
                0,
                "port for {id} = {p} is not a 3-aligned offset"
            );
        }
    }

    /// The part of `route_port` most likely to break silently in
    /// production: verify that for a large sample of *distinct* route ids,
    /// no route's {source, repair, control} 3-port window partially
    /// overlaps another's. Two different ids landing on the exact same
    /// bucket (`h % 1000`) is a separate, pre-existing, and much rarer risk
    /// documented on `route_port` itself — indistinguishable from two
    /// routes that are genuinely the same route, so it's not a bug. A
    /// *partial* overlap (e.g. one route's control port landing on
    /// another's source port) is exactly the class of bug this stride
    /// widening (2 -> 3) was meant to fix, and is what this test targets.
    #[test]
    fn route_ports_never_partially_overlap() {
        let base = 10_001u16;
        let ids: Vec<String> = (0..300).map(|i| format!("route-{i}")).collect();
        for a in &ids {
            let pa = route_port(base, a);
            assert!(
                pa >= base + 3,
                "{a}: port {pa} must land at or after base+3"
            );
            for b in &ids {
                if a == b {
                    continue;
                }
                let pb = route_port(base, b);
                let diff = pa.abs_diff(pb);
                assert!(
                    diff == 0 || diff >= 3,
                    "routes {a:?} (port {pa}) and {b:?} (port {pb}) partially overlap \
                     (diff {diff}); each route's 3-port window must be fully identical \
                     to another's (same hash bucket) or fully disjoint, never overlapping \
                     at just one or two ports"
                );
            }
        }
    }
}
