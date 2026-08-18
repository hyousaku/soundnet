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
  // True when the device could not be opened to ask what it supports, so the
  // three capability fields are placeholders. Almost always means something
  // else holds the card — PipeWire, on a desktop.
  probe_failed: boolean;
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

// "stalled" is a live pipeline whose device has gone quiet: it is neither
// producing nor accepting periods and has not for some seconds. It is not a
// failure and nothing is being retried — the engine deliberately does not
// restart into a wedged device — so it is its own state rather than a flavour
// of "retrying". It clears by itself when the device comes back. `capture`
// and `playback` say which side; both can be true on a self-loop.
export type RouteHealth =
  | { type: "ok" }
  | { type: "retrying"; attempts: number; reason: string; next_retry_ms: number }
  | { type: "stalled"; capture: boolean; playback: boolean };

// No single engine can measure a route's whole path: the sender engine
// only ever knows its own ALSA capture buffering, the receiver engine only
// ever knows roc's own e2e figure (via RTCP) plus its ALSA playback
// buffering. `null` means "this engine doesn't know" — never treat it as
// zero, and never sum whatever fields happen to be present and present the
// result as a total unless all three are non-null. See the doc comment on
// StreamStats in crates/soundnet-protocol/src/lib.rs and latency.ts.
export interface StreamStats {
  xruns: number;
  /** Peak dBFS of what this engine captured; null when it holds no capture
   *  side of the route. null and -120 are different: -120 is silence. */
  capture_level_db: number | null;
  /** Peak dBFS of what this engine played out; null when it holds no
   *  playback side. */
  playback_level_db: number | null;
  health: RouteHealth;
  roc_e2e_ms: number | null;
  capture_buffer_ms: number | null;
  playback_buffer_ms: number | null;
  // The format each device was *actually* opened with. ALSA has no
  // "nearest" fallback for format the way it does for rate and period, so a
  // device that rejects the requested one gets substituted silently — and
  // then two settings that land on the same substitute sound identical,
  // which looks like a mystery until you can see this. `null` = this engine
  // has no device on that side (or hasn't opened it yet).
  capture_format: SampleFormat | null;
  playback_format: SampleFormat | null;
  // Breakdown behind `xruns`. Capture xruns are overruns (the device had a
  // period ready before we collected it, so those samples are gone);
  // playback xruns are underruns (the device wanted samples we hadn't
  // supplied). Both are audible clicks, and which one it is says whether
  // the input or the output side is late.
  capture_xruns: number | null;
  playback_xruns: number | null;
  // Samples that had to be clamped at the rails on their way to the device.
  // Separates the two things that both sound like a click on a loud
  // sustained note: a timing glitch happens at a rate set by clock drift and
  // is merely inaudible during silence, while clipping only happens when the
  // signal is genuinely near full scale.
  clipped_samples: number | null;
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
