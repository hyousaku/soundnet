import { useStore } from "./store";
import type { SampleFormat, StreamSpec, StreamStats } from "./protocol";
import { summarizeLatency } from "./latency";

const RATES = [44100, 48000, 88200, 96000];
const FORMATS: SampleFormat[] = ["S16_LE", "S24_LE3", "S32_LE", "F32_LE"];
const PERIODS = [32, 64, 128, 256, 512];
const LATENCIES = [3, 5, 10, 20, 40, 80];

export default function RouteEditor() {
  const routes = useStore((s) => s.routes);
  const nodes = useStore((s) => s.nodes);
  const stats = useStore((s) => s.stats);
  const send = useStore((s) => s.send);

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
            <th>Ch</th>
            <th>Format</th>
            <th>Period</th>
            <th>Latency</th>
            <th>FEC</th>
            <th>Level</th>
            <th title="Latency this engine can actually account for — see the cell tooltips for what's missing on a partial figure.">
              latency
            </th>
            <th>jitter</th>
            <th>xr</th>
            <th>health</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {routeList.map((r) => {
            const health = stats[r.id]?.health;
            const failing = health?.type === "retrying";
            return (
            <tr key={r.id} style={failing ? { background: "rgba(239, 83, 80, 0.08)" } : undefined}>
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
              <td>
                <input
                  type="number"
                  min={1}
                  max={32}
                  style={{ width: 50 }}
                  value={r.spec.channels}
                  onChange={(e) => update(send, r.id, r.spec, { channels: Number(e.target.value) })}
                />
              </td>
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
              <td style={{ width: 100 }}>
                <LevelMeter db={stats[r.id]?.level_db ?? -120} />
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
              <td>{stats[r.id] ? `${stats[r.id].jitter_ms.toFixed(2)} ms` : "—"}</td>
              <td title={xrunBreakdown(stats[r.id])}>{stats[r.id]?.xruns ?? 0}</td>
              <td style={{ maxWidth: 220 }}>
                {failing && health?.type === "retrying" ? (
                  <span title={health.reason} style={{ color: "#ef5350" }}>
                    retrying ({health.attempts}) — {health.reason}
                  </span>
                ) : health ? (
                  <span style={{ color: "#4ade80" }}>ok</span>
                ) : (
                  "—"
                )}
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

function LevelMeter({ db }: { db: number }) {
  // Map [-60, 0] dB → [0, 1].
  const clamped = Math.max(-60, Math.min(0, db));
  const norm = (clamped + 60) / 60;
  const color = db > -3 ? "#ef5350" : db > -12 ? "#f59e0b" : "#4ade80";
  return (
    <div style={{ background: "#0b0d10", height: 10, borderRadius: 2, overflow: "hidden" }}>
      <div
        style={{
          width: `${norm * 100}%`,
          height: "100%",
          background: color,
          transition: "width 100ms linear",
        }}
      />
    </div>
  );
}
