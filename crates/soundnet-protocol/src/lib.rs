//! Wire types shared by `soundnet-engine` and the browser UI.
//!
//! All messages are JSON; enum variants use `serde` internally-tagged form
//! (`{"type": "...", ...}`) so the TypeScript side can dispatch on `type`.

use serde::{Deserialize, Serialize};

pub type NodeId = String;
pub type PortId = String;
pub type RouteId = String;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PortKind {
    Capture,
    Playback,
    /// Virtual capture port that generates a preview tone (see `TonePreset`).
    Tone,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SampleFormat {
    S16Le,
    S24Le3,
    S32Le,
    F32Le,
}

impl SampleFormat {
    pub fn bytes_per_sample(self) -> usize {
        match self {
            SampleFormat::S16Le => 2,
            SampleFormat::S24Le3 => 3,
            SampleFormat::S32Le => 4,
            SampleFormat::F32Le => 4,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub hostname: String,
    pub addr: String,
    /// HTTP/WebSocket control-plane port.
    pub port: u16,
    /// UDP port the node is bound to for roc audio streams (source).
    /// Repair packets use `audio_port + 1` when FEC is on.
    pub audio_port: u16,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalPort {
    pub node_id: NodeId,
    pub id: PortId,
    pub kind: PortKind,
    /// ALSA name such as `hw:1,0` or, for tone ports, a synthetic id like `tone:440`.
    pub alsa_name: String,
    /// Human-readable label shown in the UI.
    pub label: String,
    pub max_channels: u8,
    pub supported_formats: Vec<SampleFormat>,
    pub supported_rates: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortRef {
    pub node_id: NodeId,
    pub port_id: PortId,
    /// 0-based channel offset into the ALSA device (defaults to 0).
    #[serde(default)]
    pub channel_offset: u8,
}

/// Codec used on the wire.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Encoding {
    /// Raw PCM. Format on the wire is float32; local ALSA is converted to/from f32.
    Pcm,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamSpec {
    pub encoding: Encoding,
    pub rate: u32,
    pub channels: u8,
    pub frames_per_period: u32,
    /// Local ALSA sample format; transport is always f32 internally.
    pub alsa_format: SampleFormat,
    pub target_latency_ms: u16,
    pub fec: bool,
}

impl Default for StreamSpec {
    fn default() -> Self {
        Self {
            encoding: Encoding::Pcm,
            rate: 48_000,
            channels: 2,
            frames_per_period: 128,
            alsa_format: SampleFormat::S24Le3,
            target_latency_ms: 10,
            fec: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub id: RouteId,
    pub src: PortRef,
    pub dst: PortRef,
    pub spec: StreamSpec,
}

/// Local-engine assessment of a route's health. Carried alongside its stats
/// so the UI can show *why* audio has gone quiet instead of just going quiet
/// itself. `Retrying` covers everything from "the peer isn't discovered yet"
/// to "the capture/playback worker crashed" — those are indistinguishable
/// from here (a worker can fail on its own thread well after the engine
/// already reported it started), so `reason` is best-effort context, not a
/// stable error code.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RouteHealth {
    Ok,
    Retrying {
        attempts: u32,
        reason: String,
        next_retry_ms: u64,
    },
}

/// Live per-route runtime stats streamed on the WS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStats {
    pub xruns: u32,
    pub jitter_ms: f32,
    pub level_db: f32,
    pub e2e_latency_ms: f32,
    pub health: RouteHealth,
}

/// A host the user added manually (mDNS was blocked / offline). Rendered in
/// the sidebar with a delete button so the operator can drop it if it moves.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManualHost {
    pub addr: String,
    pub port: u16,
}

/// Pushed by an engine to every peer it knows about whenever its local port
/// list changes (e.g. after a rescan). Lets peers refresh their cached copy
/// without waiting for mDNS to re-resolve — mDNS TXT records don't carry the
/// port list, so a silent local change would otherwise go unnoticed by
/// anyone who discovered this node earlier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerPortsPush {
    pub node: Node,
    pub ports: Vec<LocalPort>,
}

/// Full state snapshot pushed on connect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub self_node: Node,
    pub nodes: Vec<Node>,
    pub local_ports: Vec<LocalPort>,
    pub remote_ports: Vec<LocalPort>,
    pub routes: Vec<Route>,
    #[serde(default)]
    pub manual_hosts: Vec<ManualHost>,
}

// ---------- WS client -> server ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello,
    AddRoute { route: Route },
    RemoveRoute { id: RouteId },
    UpdateSpec { id: RouteId, spec: StreamSpec },
    AddManualHost { addr: String, port: u16 },
    RemoveManualHost { addr: String, port: u16 },
    /// Re-enumerate local ALSA devices (e.g. after plugging in a new USB
    /// interface). The engine responds by broadcasting a fresh State snapshot.
    RescanDevices,
}

// ---------- WS server -> client ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    State { snapshot: StateSnapshot },
    NodeAppeared { node: Node, ports: Vec<LocalPort> },
    NodeDisappeared { node_id: NodeId },
    RouteAdded { route: Route },
    RouteRemoved { id: RouteId },
    RouteUpdated { route: Route },
    Stats {
        stats: std::collections::HashMap<RouteId, StreamStats>,
    },
    Error { message: String },
}
