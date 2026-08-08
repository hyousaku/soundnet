import { useStore } from "./store";
import type { SampleFormat, StreamSpec } from "./protocol";

const RATES = [44100, 48000, 88200, 96000];
const FORMATS: SampleFormat[] = ["S16_LE", "S24_LE3", "S32_LE", "F32_LE"];
const PERIODS = [32, 64, 128, 256, 512];
const LATENCIES = [3, 5, 10, 20, 40, 80];

export default function RouteEditor() {
  const routes = useStore((s) => s.routes);
  const nodes = useStore((s) => s.nodes);
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
            <th></th>
          </tr>
        </thead>
        <tbody>
          {routeList.map((r) => (
            <tr key={r.id}>
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
              <td>
                <button onClick={() => send({ type: "remove_route", id: r.id })}>
                  Remove
                </button>
              </td>
            </tr>
          ))}
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
