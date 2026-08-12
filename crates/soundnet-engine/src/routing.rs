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
    pub level_bits: Option<Arc<AtomicU32>>,
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
            slot.as_ref()?.lock().ok()?.clone()
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
        if let Some((_, existing)) = state.running.remove(&route.id) {
            shutdown_running(existing);
        }
        state.failures.remove(&route.id);
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
pub async fn try_start(state: &Arc<EngineState>, route: &Route) -> Result<()> {
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
            shutdown_running(dead_route);
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
        level_bits: None,
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
        running.level_bits = Some(pipeline.level_bits.clone());
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
    let route = state.routes.remove(id).map(|(_, r)| r);
    if let Some((_, running)) = state.running.remove(id) {
        shutdown_running(running);
    }
    state.failures.remove(id);
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
                let level = running
                    .level_bits
                    .as_ref()
                    .map(|b| f32::from_bits(b.load(Ordering::Relaxed)))
                    .unwrap_or(0.0);
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
                    level_db: if level > 0.0 {
                        20.0 * level.log10()
                    } else {
                        -120.0
                    },
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
                        level_db: -120.0,
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
    let ids: Vec<_> = state.running.iter().map(|e| e.key().clone()).collect();
    for id in ids {
        if let Some((_, running)) = state.running.remove(&id) {
            shutdown_running(running);
        }
    }
}

fn shutdown_running(running: RunningRoute) {
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
    use super::{backoff_for_attempts, route_port};
    use std::time::Duration;

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
