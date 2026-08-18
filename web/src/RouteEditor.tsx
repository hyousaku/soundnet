import { useStore } from "./store";
import type { LocalPort, Route, SampleFormat, StreamSpec, StreamStats } from "./protocol";
import { summarizeLatency } from "./latency";
import { describeHealth } from "./health";

const RATES = [44100, 48000, 88200, 96000];
const FORMATS: SampleFormat[] = ["S16_LE", "S24_LE3", "S24_LE", "S32_LE", "F32_LE"];
const PERIODS = [32, 64, 128, 256, 512];
const LATENCIES = [3, 5, 10, 20, 40, 80];

export default function RouteEditor() {
  const routes = useStore((s) => s.routes);
  const nodes = useStore((s) => s.nodes);
  const stats = useStore((s) => s.stats);
  const send = useStore((s) => s.send);
  const ports = useStore((s) => s.ports);

  /// How many channels each end's device has, or null when that is not a
  /// real constraint: a tone synthesizes as many as asked for, and an
  /// unprobed device's "2" is a placeholder rather than a limit — clamping to
  /// it would turn a display problem into a real one.
  const deviceWidths = (r: Route): { src: number | null; dst: number | null } => {
    const find = (nodeId: string, portId: string): LocalPort | undefined =>
      (ports[nodeId] ?? []).find((p) => p.id === portId);
    const width = (p?: LocalPort) =>
      p && p.kind !== "tone" && !p.probe_failed ? p.max_channels : null;
    return { src: width(find(r.src.node_id, r.src.port_id)), dst: width(find(r.dst.node_id, r.dst.port_id)) };
  };

  /// Widest window that still fits inside both devices from their current
  /// starting channels.
  const maxWidth = (r: Route): number => {
    const w = deviceWidths(r);
    const room = [
      w.src === null ? null : w.src - (r.src.channel_offset ?? 0),
      w.dst === null ? null : w.dst - (r.dst.channel_offset ?? 0),
    ].filter((n): n is number => n !== null);
    return room.length === 0 ? 32 : Math.max(1, Math.min(...room));
  };

  const routeList = Object.values(routes);
  if (routeList.length === 0) {
    return (
      <div className="route-editor">
        <div className="hint">
          No routes yet. Connect two nodes in the graph above to create one.
          Drag from a source node to a destination — the first available
          Capture/Tone port on the source is wired to the first Playback port
          on the destination.
        </div>
      </div>
    );
  }

  return (
    <div className="route-editor">
      <table>
        <thead>
          <tr>
            <th>Src → Dst</th>
            <th>Rate</th>
            <th title="How many channels this route carries.">Ch</th>
            <th title="First channel of the source device this route takes, counting from 1.">src ch</th>
            <th title="First channel of the destination device this route lands on, counting from 1.">dst ch</th>
            <th>Format</th>
            <th>Period</th>
            <th>Latency</th>
            <th>FEC</th>
            <th title="Peak level in and out, as measured by THIS engine. A route's two ends usually live on two machines, and each engine can only meter the half it holds — so a dash means &quot;not mine to measure&quot;, not silence. Open the other machine's UI to see its half.">Level</th>
            <th title="Latency this engine can actually account for — see the cell tooltips for what's missing on a partial figure.">
              latency
            </th>
            <th>xr</th>
            <th title="Samples clamped at full scale on the way to the device. Non-zero means the clicks are gain staging, not timing — turn the input down.">clip</th>
            <th>health</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {routeList.map((r) => {
            const health = describeHealth(stats[r.id]?.health);
            return (
            <tr key={r.id} style={health.bad ? { background: `${health.color}14` } : undefined}>
              <td>
                {nodes[r.src.node_id]?.hostname ?? r.src.node_id.slice(0, 8)} →{" "}
                {nodes[r.dst.node_id]?.hostname ?? r.dst.node_id.slice(0, 8)}
              </td>
              <td>
                <select
                  value={r.spec.rate}
                  onChange={(e) => update(send, r.id, r.spec, { rate: Number(e.target.value) })}
                >
                  {RATES.map((v) => (
                    <option key={v} value={v}>{v / 1000}k</option>
                  ))}
                </select>
              </td>
              <td style={{ whiteSpace: "nowrap" }}>
                <input
                  type="number"
                  min={1}
                  max={maxWidth(r)}
                  style={{ width: 46 }}
                  value={r.spec.channels}
                  onChange={(e) =>
                    update(send, r.id, r.spec, {
                      channels: Math.max(1, Math.min(maxWidth(r), Number(e.target.value))),
                    })
                  }
                />
              </td>
              <ChannelStart
                route={r}
                side="src"
                deviceWidth={deviceWidths(r).src}
                send={send}
              />
              <ChannelStart
                route={r}
                side="dst"
                deviceWidth={deviceWidths(r).dst}
                send={send}
              />
              <td>
                <select
                  value={r.spec.alsa_format}
                  onChange={(e) => update(send, r.id, r.spec, { alsa_format: e.target.value as SampleFormat })}
                >
                  {FORMATS.map((v) => (<option key={v} value={v}>{v}</option>))}
                </select>
                <ActualFormat spec={r.spec} stats={stats[r.id]} />
              </td>
              <td>
                <select
                  value={r.spec.frames_per_period}
                  onChange={(e) => update(send, r.id, r.spec, { frames_per_period: Number(e.target.value) })}
                >
                  {PERIODS.map((v) => (<option key={v} value={v}>{v}</option>))}
                </select>
              </td>
              <td>
                <select
                  value={r.spec.target_latency_ms}
                  onChange={(e) => update(send, r.id, r.spec, { target_latency_ms: Number(e.target.value) })}
                >
                  {LATENCIES.map((v) => (<option key={v} value={v}>{v} ms</option>))}
                </select>
              </td>
              <td>
                <input
                  type="checkbox"
                  checked={r.spec.fec}
                  onChange={(e) => update(send, r.id, r.spec, { fec: e.target.checked })}
                />
              </td>
              <td style={{ width: 110 }}>
                <LevelMeter label="in" db={stats[r.id]?.capture_level_db ?? null} />
                <LevelMeter label="out" db={stats[r.id]?.playback_level_db ?? null} />
              </td>
              <td>
                {(() => {
                  const lat = summarizeLatency(stats[r.id]);
                  return (
                    <span title={lat.title} style={lat.partial ? { color: "#f59e0b" } : undefined}>
                      {lat.text}
                    </span>
                  );
                })()}
              </td>
              <td title={xrunBreakdown(stats[r.id])}>{stats[r.id]?.xruns ?? 0}</td>
              <td style={(stats[r.id]?.clipped_samples ?? 0) > 0 ? { color: "#ef5350" } : undefined}>
                {stats[r.id]?.clipped_samples ?? "—"}
              </td>
              <td style={{ maxWidth: 220 }}>
                <span title={health.title} style={{ color: health.color }}>
                  {health.text}
                </span>
              </td>
              <td>
                <button onClick={() => send({ type: "remove_route", id: r.id })}>
                  Remove
                </button>
              </td>
            </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

/// Which channel of the device this route's window starts at.
///
/// Shown counting from 1, because that is how the numbers are silk-screened
/// on the front of an interface; `channel_offset` on the wire is 0-based.
/// Getting that translation wrong by one is the kind of mistake that sounds
/// like a patching error rather than a UI bug, so it happens exactly here and
/// nowhere else.
function ChannelStart({
  route,
  side,
  deviceWidth,
  send,
}: {
  route: Route;
  side: "src" | "dst";
  deviceWidth: number | null;
  send: (msg: any) => void;
}) {
  const ref = side === "src" ? route.src : route.dst;
  const offset = ref.channel_offset ?? 0;
  // The window has to fit: a 2-channel route on an 8-channel device can start
  // at 7 at the latest.
  const maxStart = deviceWidth === null ? 32 : Math.max(1, deviceWidth - route.spec.channels + 1);
  return (
    <td style={{ whiteSpace: "nowrap" }}>
      <input
        type="number"
        min={1}
        max={maxStart}
        style={{ width: 46 }}
        value={offset + 1}
        onChange={(e) => {
          const start = Math.max(1, Math.min(maxStart, Number(e.target.value)));
          const patched = { ...ref, channel_offset: start - 1 };
          // Offsets live on the PortRef, not the spec, so this goes back as a
          // whole route. `apply_route` treats a known id as an update: it
          // restarts the pipelines and gossips to the other engine, same as
          // any other change.
          send({
            type: "add_route",
            route: side === "src" ? { ...route, src: patched } : { ...route, dst: patched },
          });
        }}
      />
      <span style={{ color: "#8a94a5", fontSize: 10, marginLeft: 3 }}>
        /{deviceWidth ?? "?"}
      </span>
    </td>
  );
}

function update(
  send: (msg: any) => void,
  id: string,
  spec: StreamSpec,
  patch: Partial<StreamSpec>,
): void {
  send({ type: "update_spec", id, spec: { ...spec, ...patch } });
}

/// Shows what the hardware actually got, whenever that isn't what was asked
/// for. Silence here means "the request went through as-is" (or that this
/// engine holds neither end of the route, which the latency column already
/// makes obvious) — so the row only grows a marker when there's something to
/// know. Without it, picking a format a device doesn't support looks like it
/// applied, and two different settings that fall back to the same substitute
/// are indistinguishable from a bug.
function ActualFormat({ spec, stats }: { spec: StreamSpec; stats?: StreamStats }) {
  if (!stats) return null;
  const sides: Array<[string, SampleFormat | null]> = [
    ["capture", stats.capture_format],
    ["playback", stats.playback_format],
  ];
  const substituted = sides.filter(
    ([, actual]) => actual != null && actual !== spec.alsa_format,
  ) as Array<[string, SampleFormat]>;
  if (substituted.length === 0) return null;
  return (
    <div
      style={{ color: "#f59e0b", fontSize: 10, marginTop: 2 }}
      title={
        substituted
          .map(([side, actual]) => `${side} device does not support ${spec.alsa_format}, opened ${actual} instead`)
          .join("; ") + ". The network always carries f32, so this only affects the local device leg."
      }
    >
      {substituted.map(([side, actual]) => `→ ${actual} (${side})`).join(" ")}
    </div>
  );
}

/// The `xr` column sums both directions, so spell out which side is late —
/// capture overruns and playback underruns sound the same but have opposite
/// causes.
function xrunBreakdown(stats?: StreamStats): string {
  if (!stats) return "No data from this engine for this route.";
  const parts: string[] = [];
  if (stats.capture_xruns != null) parts.push(`${stats.capture_xruns} capture (overrun: input samples lost)`);
  if (stats.playback_xruns != null) parts.push(`${stats.playback_xruns} playback (underrun: output starved)`);
  if (parts.length === 0) return "This engine holds neither end of this route.";
  return parts.join(", ");
}

/// One direction's meter. `db === null` means this engine does not hold that
/// end of the route, which is drawn as a dash rather than as a bar at the
/// bottom of its travel — a meter reading empty is a claim about the audio,
/// and we would be making it about a signal we never saw. For most routes one
/// of the two is null, because the two ends are on different machines and a
/// browser is connected to one engine.
function LevelMeter({ label, db }: { label: string; db: number | null }) {
  const track: React.CSSProperties = {
    background: "#0b0d10",
    height: 8,
    borderRadius: 2,
    overflow: "hidden",
    flex: 1,
  };
  const tag = (
    <span style={{ color: "#8a94a5", fontSize: 9, width: 20, flexShrink: 0 }}>{label}</span>
  );
  if (db === null) {
    // A dashed outline rather than a dimmed empty bar: an empty bar is what a
    // real meter looks like when the audio is silent, and these two states
    // must not be able to be confused. The outline keeps the row aligned with
    // the direction that does have a reading.
    return (
      <div style={{ display: "flex", alignItems: "center", gap: 4, marginBottom: 2 }}>
        {tag}
        <div
          title={`This engine holds no ${label === "in" ? "capture" : "playback"} side of this route, so it has nothing to meter. Open the other machine's UI for that half.`}
          style={{
            height: 8,
            flex: 1,
            border: "1px dashed #333b47",
            borderRadius: 2,
            boxSizing: "border-box",
          }}
        />
      </div>
    );
  }
  // Map [-60, 0] dB → [0, 1].
  const clamped = Math.max(-60, Math.min(0, db));
  const norm = (clamped + 60) / 60;
  const color = db > -3 ? "#ef5350" : db > -12 ? "#f59e0b" : "#4ade80";
  return (
    <div style={{ display: "flex", alignItems: "center", gap: 4, marginBottom: 2 }}>
      {tag}
      <div style={track} title={`${db.toFixed(1)} dBFS`}>
        <div
          style={{
            width: `${norm * 100}%`,
            height: "100%",
            background: color,
            transition: "width 100ms linear",
          }}
        />
      </div>
    </div>
  );
}
