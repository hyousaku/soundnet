// TypeScript mirror of `crates/soundnet-protocol/src/lib.rs`.
// Keep field names and enum casings aligned with the serde derives on the
// Rust side (protocol module uses `snake_case` tags, `SCREAMING_SNAKE_CASE`
// for SampleFormat, `lowercase` for PortKind / Encoding).

export type NodeId = string;
export type PortId = string;
export type RouteId = string;

export type PortKind = "capture" | "playback" | "tone";

export type SampleFormat = "S16_LE" | "S24_LE3" | "S32_LE" | "F32_LE";

export interface Node {
  id: NodeId;
  hostname: string;
  addr: string;
  port: number;
  audio_port: number;
  version: string;
}

export interface LocalPort {
  node_id: NodeId;
  id: PortId;
  kind: PortKind;
  alsa_name: string;
  label: string;
  max_channels: number;
  supported_formats: SampleFormat[];
  supported_rates: number[];
}

export interface PortRef {
  node_id: NodeId;
  port_id: PortId;
  channel_offset?: number;
}

export type Encoding = { kind: "pcm" };

export interface StreamSpec {
  encoding: Encoding;
  rate: number;
  channels: number;
  frames_per_period: number;
  alsa_format: SampleFormat;
  target_latency_ms: number;
  fec: boolean;
}

export interface Route {
  id: RouteId;
  src: PortRef;
  dst: PortRef;
  spec: StreamSpec;
}

export type RouteHealth =
  | { type: "ok" }
  | { type: "retrying"; attempts: number; reason: string; next_retry_ms: number };

// No single engine can measure a route's whole path: the sender engine
// only ever knows its own ALSA capture buffering, the receiver engine only
// ever knows roc's own e2e figure (via RTCP) plus its ALSA playback
// buffering. `null` means "this engine doesn't know" — never treat it as
// zero, and never sum whatever fields happen to be present and present the
// result as a total unless all three are non-null. See the doc comment on
// StreamStats in crates/soundnet-protocol/src/lib.rs and latency.ts.
export interface StreamStats {
  xruns: number;
  jitter_ms: number;
  level_db: number;
  health: RouteHealth;
  roc_e2e_ms: number | null;
  capture_buffer_ms: number | null;
  playback_buffer_ms: number | null;
}

export interface ManualHost {
  addr: string;
  port: number;
}

// A usable local network interface, for the "This node" interface-pinning
// control. Only meaningful for self_node — see StateSnapshot.interfaces.
export interface NetInterface {
  name: string;
  addr: string;
}

export interface StateSnapshot {
  self_node: Node;
  nodes: Node[];
  local_ports: LocalPort[];
  remote_ports: LocalPort[];
  routes: Route[];
  manual_hosts: ManualHost[];
  // Describes self_node only: a peer's own snapshot describes itself, not us.
  interfaces: NetInterface[];
  selected_interface: string | null;
}

export type ServerMsg =
  | { type: "state"; snapshot: StateSnapshot }
  | { type: "node_appeared"; node: Node; ports: LocalPort[] }
  | { type: "node_disappeared"; node_id: NodeId }
  | { type: "route_added"; route: Route }
  | { type: "route_removed"; id: RouteId }
  | { type: "route_updated"; route: Route }
  | { type: "stats"; stats: Record<RouteId, StreamStats> }
  | { type: "error"; message: string };

export type ClientMsg =
  | { type: "hello" }
  | { type: "add_route"; route: Route }
  | { type: "remove_route"; id: RouteId }
  | { type: "update_spec"; id: RouteId; spec: StreamSpec }
  | { type: "add_manual_host"; addr: string; port: number }
  | { type: "remove_manual_host"; addr: string; port: number }
  | { type: "rescan_devices" }
  // Pin (or, with name: null, un-pin back to automatic) the network
  // interface this engine advertises over mDNS and sends audio out of. Only
  // meaningful sent to the engine that owns the interface.
  | { type: "set_interface"; name: string | null };

export const defaultSpec = (): StreamSpec => ({
  encoding: { kind: "pcm" },
  rate: 48000,
  channels: 2,
  frames_per_period: 128,
  alsa_format: "S24_LE3",
  target_latency_ms: 10,
  fec: true,
});
