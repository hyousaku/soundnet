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

    /// Compact encoding so a pipeline thread can publish the format it
    /// actually negotiated through an `AtomicU8`, which is the only kind of
    /// shared state cheap enough to touch from a `SCHED_FIFO` audio thread.
    /// `UNKNOWN_FORMAT` covers "the device hasn't been opened yet" — and,
    /// permanently, a tone source, which has no device to negotiate with.
    pub fn as_u8(self) -> u8 {
        match self {
            SampleFormat::S16Le => 0,
            SampleFormat::S24Le3 => 1,
            SampleFormat::S32Le => 2,
            SampleFormat::F32Le => 3,
        }
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(SampleFormat::S16Le),
            1 => Some(SampleFormat::S24Le3),
            2 => Some(SampleFormat::S32Le),
            3 => Some(SampleFormat::F32Le),
            _ => None,
        }
    }
}

/// Sentinel for `SampleFormat::from_u8`: nothing negotiated (yet).
pub const UNKNOWN_FORMAT: u8 = u8::MAX;

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
    /// True when the device could not be opened to ask what it supports, so
    /// `max_channels` / `supported_formats` / `supported_rates` are
    /// placeholders rather than facts.
    ///
    /// The usual cause is that something else holds the card — on a desktop
    /// that is PipeWire. The old fallback silently claimed "2 channels,
    /// S16_LE, 48 kHz", which is indistinguishable from a real stereo
    /// interface and sends you looking for the missing channels in the wrong
    /// place entirely.
    #[serde(default)]
    pub probe_failed: bool,
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
///
/// **No engine can measure a route's whole path.** `RunningRoute` on the
/// source machine holds the capture + roc-sender handles; on the
/// destination machine it holds the roc-receiver + playback handles. So the
/// sender engine can only ever know its own ALSA capture buffering, and the
/// receiver engine can only ever know roc's own end-to-end figure (via
/// RTCP) plus its ALSA playback buffering. A browser is connected to
/// exactly one engine and only ever sees that engine's half.
///
/// This is why latency is three separate optional fields rather than one
/// combined figure: summing whatever a single engine happens to know and
/// presenting it as "end-to-end latency" would be worse than showing
/// nothing, because an operator tuning against it would believe they're at
/// the partial number when they're actually higher. `None` means "this
/// engine doesn't know" — either because it has no local role in that half
/// of the route, or because nothing's been sampled yet — and the UI is
/// expected to say so rather than treating it as zero (see RouteEditor.tsx
/// / Patchbay.tsx).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStats {
    /// Every xrun this engine saw for this route, both directions summed.
    /// The breakdown is in `capture_xruns` / `playback_xruns` — this stays a
    /// single number because "did anything glitch?" is the question the
    /// column answers, and for a long time it silently answered it with the
    /// playback side only, which let a route drop capture periods while
    /// reporting a clean zero.
    pub xruns: u32,
    pub jitter_ms: f32,
    pub level_db: f32,
    pub health: RouteHealth,
    /// Roc's own end-to-end latency (frame written by the sender's
    /// `roc_sender_write` → frame read by the receiver's `roc_receiver_read`),
    /// computed by libroc from RTCP + system clock. Only ever populated by
    /// the engine holding this route's roc receiver.
    #[serde(default)]
    pub roc_e2e_ms: Option<f32>,
    /// Frames currently queued in the local ALSA capture device. Only ever
    /// populated by the engine holding this route's capture side (never for
    /// a Tone source, which has no ALSA buffer to report).
    #[serde(default)]
    pub capture_buffer_ms: Option<f32>,
    /// Frames currently queued in the local ALSA playback device. Only ever
    /// populated by the engine holding this route's playback side.
    #[serde(default)]
    pub playback_buffer_ms: Option<f32>,
    /// The format the capture device was actually opened with, which is not
    /// always the one the route asked for: ALSA has no "nearest" fallback
    /// for format the way it does for rate and period, so a device that
    /// rejects the request gets substituted (see `audio::format::pick_format`).
    /// Reporting only the requested format would hide that a route asking for
    /// F32_LE is really running S24_LE3 — and then two settings that sound
    /// identical look like a mystery instead of the same thing twice.
    #[serde(default)]
    pub capture_format: Option<SampleFormat>,
    /// As `capture_format`, for the playback device. The two ends of a route
    /// negotiate independently and can genuinely differ.
    #[serde(default)]
    pub playback_format: Option<SampleFormat>,
    /// Xruns on the capture device alone (overruns: the device had samples
    /// ready and we were late collecting them). `None` when this engine
    /// holds no capture side, including a tone source.
    #[serde(default)]
    pub capture_xruns: Option<u32>,
    /// Xruns on the playback device alone (underruns: the device wanted
    /// samples and we were late supplying them). `None` when this engine
    /// holds no playback side.
    #[serde(default)]
    pub playback_xruns: Option<u32>,
    /// Samples that arrived at the playback device outside [-1.0, 1.0] and
    /// had to be clamped.
    ///
    /// This distinguishes the two things that both sound like a click on a
    /// loud sustained note. A timing glitch drops or repeats a period, so it
    /// happens at a rate set by clock drift and is merely *inaudible* during
    /// silence; clipping only happens when the signal is actually near full
    /// scale — and a resampler can push a peak that was just under 1.0 to
    /// just over it, which is a gain-staging problem, not a timing one.
    /// Without this counter the two are indistinguishable by ear.
    #[serde(default)]
    pub clipped_samples: Option<u32>,
}

/// A host the user added manually (mDNS was blocked / offline). Rendered in
/// the sidebar with a delete button so the operator can drop it if it moves.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManualHost {
    pub addr: String,
    pub port: u16,
}

/// A usable local network interface, offered to the operator so they can pin
/// mDNS advertisement + audio egress to a specific NIC on multi-homed hosts
/// (e.g. wired and wireless on the same subnet).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetInterface {
    pub name: String,
    pub addr: String,
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
    /// Usable network interfaces on *this* node, for the interface-pinning
    /// control in the UI. Describes `self_node` only — a peer's own snapshot
    /// (fetched during discovery) describes itself, not us, so this can't be
    /// used to control anything but the local engine. `#[serde(default)]`
    /// because it's fetched from peers that may still be running an older
    /// build without this field.
    #[serde(default)]
    pub interfaces: Vec<NetInterface>,
    /// Name of the interface currently pinned for mDNS/audio egress, or
    /// `None` for automatic selection. Mirrors `Config::interface` on the
    /// engine this snapshot came from.
    #[serde(default)]
    pub selected_interface: Option<String>,
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
    /// Pin (or, with `name: None`, un-pin back to automatic) the network
    /// interface this engine advertises over mDNS and sends audio out of.
    /// Only ever meaningful sent to the engine that owns the interface — an
    /// engine can only reconfigure itself, not a peer.
    SetInterface { name: Option<String> },
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
