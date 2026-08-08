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

export interface StreamStats {
  xruns: number;
  jitter_ms: number;
  level_db: number;
  e2e_latency_ms: number;
}

export interface ManualHost {
  addr: string;
  port: number;
}

export interface StateSnapshot {
  self_node: Node;
  nodes: Node[];
  local_ports: LocalPort[];
  remote_ports: LocalPort[];
  routes: Route[];
  manual_hosts: ManualHost[];
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
  | { type: "remove_manual_host"; addr: string; port: number };

export const defaultSpec = (): StreamSpec => ({
  encoding: { kind: "pcm" },
  rate: 48000,
  channels: 2,
  frames_per_period: 128,
  alsa_format: "S24_LE3",
  target_latency_ms: 10,
  fec: true,
});
