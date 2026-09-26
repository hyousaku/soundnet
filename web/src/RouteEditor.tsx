import { useEffect, useRef, useState } from "react";
import { useStore } from "./store";
import { useUi } from "./ui";
import { askRemoveRoute, sortedRoutes, widthOf } from "./actions";
import type { LocalPort, Route, SampleFormat, StreamSpec, StreamStats } from "./protocol";
import { summarizeLatency } from "./latency";
import { describeHealth } from "./health";

const RATES = [44100, 48000, 88200, 96000];
const FORMATS: SampleFormat[] = ["S16_LE", "S24_LE3", "S24_LE", "S32_LE", "F32_LE"];
const PERIODS = [32, 64, 128, 256, 512];
// 120 and 200 exist for paths that leave the LAN entirely — over the public
// internet (a Tailscale tunnel, say) jitter runs much larger than on a wire,
// and 80ms of buffer can still be too little to absorb it. See the README's
// "Running over Tailscale" section.
const LATENCIES = [3, 5, 10, 20, 40, 80, 120, 200];

/// How long after the last change to a row before it is sent.
///
/// Every change to a route restarts its pipelines on both machines — a real,
/// audible gap. Sending on every `change` event meant arrowing a period menu
/// from 128 to 512 restarted the audio twice, and typing "16" into a channel
/// box sent "1" first: a genuine reconfiguration to one channel, on the way
/// to the value that was meant. Edits now collect in a draft and go out
/// together once the operator pauses, or at once on Enter or on leaving the
/// row.
const COMMIT_DELAY_MS = 600;

/// How long to keep showing a draft the engine has not answered. An edit the
/// engine rejects produces an error notice and no updated route, so without
/// a limit the row would show the rejected value forever — the one thing
/// worse than the old behaviour.
const DRAFT_TIMEOUT_MS = 4000;

const range = (n: number) => Array.from({ length: Math.max(1, n) }, (_, i) => i + 1);

export default function RouteEditor() {
  const routes = useStore((s) => s.routes);
  const nodes = useStore((s) => s.nodes);
  const stats = useStore((s) => s.stats);
  const send = useStore((s) => s.send);
  const ports = useStore((s) => s.ports);
  const selectedRouteId = useUi((s) => s.selectedRouteId);
  const selectRoute = useUi((s) => s.selectRoute);
  const openDialog = useUi((s) => s.openDialog);

  const [drafts, setDrafts] = useState<Record<string, Route>>({});
  const draftsRef = useRef(drafts);
  draftsRef.current = drafts;
  const routesRef = useRef(routes);
  routesRef.current = routes;
  const timers = useRef<Record<string, number>>({});
  /// The server's copy of a route at the moment a draft was sent. When a
  /// different object arrives for that id, the engine has answered and the
  /// draft can go.
  const awaiting = useRef<Record<string, Route>>({});

  const dropDraft = (id: string) => {
    delete awaiting.current[id];
    const { [id]: _gone, ...rest } = draftsRef.current;
    draftsRef.current = rest;
    setDrafts(rest);
  };

  useEffect(() => {
    for (const id of Object.keys(awaiting.current)) {
      if (timers.current[id]) continue; // still being edited; keep it
      if (routes[id] !== awaiting.current[id]) dropDraft(id);
    }
  }, [routes]);

  useEffect(
    () => () => {
      for (const t of Object.values(timers.current)) window.clearTimeout(t);
    },
    [],
  );

  const commit = (id: string) => {
    window.clearTimeout(timers.current[id]);
    delete timers.current[id];
    const draft = draftsRef.current[id];
    const server = routesRef.current[id];
    if (!draft || !server) return;
    if (JSON.stringify(draft) === JSON.stringify(server)) {
      // Changed and changed back: nothing to send, and sending would still
      // cost a restart on an engine that compares only after the fact.
      dropDraft(id);
      return;
    }
    awaiting.current[id] = server;
    // The whole route, not just the spec: channel offsets live on the ports,
    // and one message keeps a combined edit to one restart. `add_route` with
    // a known id is an update on the engine side, and — unlike the spec-only
    // message — reports a refusal back to the page.
    send({ type: "add_route", route: draft });
    window.setTimeout(() => {
      if (!timers.current[id] && awaiting.current[id] === server) dropDraft(id);
    }, DRAFT_TIMEOUT_MS);
  };

  const edit = (id: string, next: Route) => {
    draftsRef.current = { ...draftsRef.current, [id]: next };
    setDrafts(draftsRef.current);
    window.clearTimeout(timers.current[id]);
    timers.current[id] = window.setTimeout(() => commit(id), COMMIT_DELAY_MS);
  };

  // A route selected elsewhere — a wire clicked on the canvas, J/K — is
  // brought into view here, unless the operator is already working in that
  // row, in which case scrolling it would move the control they are using.
  useEffect(() => {
    if (!selectedRouteId) return;
    const row = document.querySelector<HTMLElement>(`[data-route-row="${selectedRouteId}"]`);
    if (!row || row.contains(document.activeElement)) return;
    // Vertical only. scrollIntoView on a row wider than the table's box also
    // scrolls sideways, and J/K then left the table scrolled to its right
    // end with the route names under the sticky column's edge.
    const box = row.closest<HTMLElement>(".route-editor");
    if (!box) return;
    const r = row.getBoundingClientRect();
    const b = box.getBoundingClientRect();
    if (r.top < b.top) box.scrollTop -= b.top - r.top;
    else if (r.bottom > b.bottom) box.scrollTop += r.bottom - b.bottom;
  }, [selectedRouteId]);

  const findPort = (nodeId: string, portId: string): LocalPort | undefined =>
    (ports[nodeId] ?? []).find((p) => p.id === portId);

  /// How many channels each end's device has, or null when that is not a
  /// real constraint: a tone synthesizes as many as asked for, and an
  /// unprobed device's "2" is a placeholder rather than a limit — clamping to
  /// it would turn a display problem into a real one.
  const deviceWidths = (r: Route): { src: number | null; dst: number | null } => {
    return {
      src: widthOf(findPort(r.src.node_id, r.src.port_id)),
      dst: widthOf(findPort(r.dst.node_id, r.dst.port_id)),
    };
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

  const routeList = sortedRoutes(routes);

  if (routeList.length === 0) {
    return (
      <section className="route-editor" id="region-routes" tabIndex={-1} aria-label="Routes">
        <div className="empty-state">
          <p>No routes yet.</p>
          <p className="hint">
            Press <kbd>N</kbd> or use{" "}
            <button className="linklike" onClick={() => openDialog({ kind: "newRoute" })}>
              New route
            </button>{" "}
            to pick a source and a destination from lists. Or, in the patch
            bay, drag from the dot at the right end of a source row to the dot
            at the left end of a destination row — or click one dot, then the
            other.
          </p>
        </div>
      </section>
    );
  }

  const endLabel = (nodeId: string, portId: string) => {
    const host = nodes[nodeId]?.hostname ?? nodeId.slice(0, 8);
    const port = findPort(nodeId, portId);
    return { host, port: port?.label ?? portId };
  };

  return (
    <section className="route-editor" id="region-routes" tabIndex={-1} aria-label="Routes">
      <table>
        <thead>
          <tr>
            <th>Route</th>
            <th>Rate</th>
            <th title="How many channels this route carries.">Ch</th>
            <th title="First channel of the source device this route takes, counting from 1.">Src ch</th>
            <th title="First channel of the destination device this route lands on, counting from 1.">Dst ch</th>
            <th>Format</th>
            <th title="ALSA period: smaller is lower latency and more demanding.">Period</th>
            <th title="How much audio the receiving end buffers to ride out network jitter.">Target</th>
            <th title="Forward error correction: spends bandwidth to recover lost packets.">FEC</th>
            <th title="Peak level in and out, as measured by THIS engine. A route's two ends usually live on two machines, and each engine can only meter the half it holds — so a dashed outline means &quot;not mine to measure&quot;, not silence. Open the other machine's UI to see its half.">Level</th>
            <th title="Latency this engine can actually account for — see the cell tooltips for what's missing on a partial figure.">Measured</th>
            <th title="Glitches (xruns): times a device was not served in time.">xr</th>
            <th title="Samples clamped at full scale on the way to the device. Non-zero means the clicks are gain staging, not timing — turn the input down.">Clip</th>
            <th>Health</th>
            <th className="actions-cell"><span className="sr-only">Actions</span></th>
          </tr>
        </thead>
        <tbody>
          {routeList.map((server) => {
            const r = drafts[server.id] ?? server;
            const id = r.id;
            const health = describeHealth(stats[id]?.health);
            const pending = !!drafts[id];
            const src = endLabel(r.src.node_id, r.src.port_id);
            const dst = endLabel(r.dst.node_id, r.dst.port_id);
            const name = `${src.host} → ${dst.host}`;
            const widths = deviceWidths(r);
            const setSpec = (patch: Partial<StreamSpec>) =>
              edit(id, { ...r, spec: { ...r.spec, ...patch } });
            const setStart = (side: "src" | "dst", start: number) =>
              edit(id, { ...r, [side]: { ...r[side], channel_offset: start - 1 } });
            const classes = [
              selectedRouteId === id ? "selected" : "",
              health.bad ? "bad" : "",
            ].join(" ");
            return (
              <tr
                key={id}
                data-route-row={id}
                className={classes}
                style={health.bad ? { background: `${health.color}14` } : undefined}
                onMouseDown={() => selectRoute(id)}
                onFocus={() => selectRoute(id)}
                onBlur={(e) => {
                  // Leaving the row sends whatever is waiting. Moving between
                  // controls *within* the row does not, so a rate change and a
                  // period change made together still cost one restart.
                  if (!e.currentTarget.contains(e.relatedTarget as Node | null) && timers.current[id]) {
                    commit(id);
                  }
                }}
                onKeyDown={(e) => {
                  const t = e.target as HTMLElement;
                  if (e.key === "Enter" && (t.tagName === "SELECT" || t.tagName === "INPUT")) {
                    e.preventDefault();
                    commit(id);
                  }
                }}
              >
                <td className="route-name">
                  <div>
                    {src.host} <span aria-hidden>→</span> {dst.host}
                    {pending && (
                      <span className="pending" title="Waiting to apply — this change has not reached the engine yet">
                        applying…
                      </span>
                    )}
                  </div>
                  <div className="route-ports" title={`${src.port}\n→ ${dst.port}`}>
                    {src.port} → {dst.port}
                  </div>
                </td>
                <td>
                  <select
                    aria-label={`Sample rate, ${name}`}
                    value={r.spec.rate}
                    onChange={(e) => setSpec({ rate: Number(e.target.value) })}
                  >
                    {RATES.map((v) => (
                      <option key={v} value={v}>
                        {v / 1000}k
                      </option>
                    ))}
                  </select>
                </td>
                <td>
                  {/*
                    A menu rather than a number box: it can only offer values
                    that fit both devices, it works the same from the mouse and
                    the keyboard, and it never sends a half-typed number.
                  */}
                  <select
                    aria-label={`Channels, ${name}`}
                    value={r.spec.channels}
                    onChange={(e) => setSpec({ channels: Number(e.target.value) })}
                  >
                    {range(Math.max(maxWidth(r), r.spec.channels)).map((v) => (
                      <option key={v} value={v}>
                        {v}
                      </option>
                    ))}
                  </select>
                </td>
                <ChannelStart
                  label={`Source start channel, ${name}`}
                  offset={r.src.channel_offset ?? 0}
                  channels={r.spec.channels}
                  deviceWidth={widths.src}
                  onChange={(start) => setStart("src", start)}
                />
                <ChannelStart
                  label={`Destination start channel, ${name}`}
                  offset={r.dst.channel_offset ?? 0}
                  channels={r.spec.channels}
                  deviceWidth={widths.dst}
                  onChange={(start) => setStart("dst", start)}
                />
                <td>
                  <select
                    aria-label={`Sample format, ${name}`}
                    value={r.spec.alsa_format}
                    onChange={(e) => setSpec({ alsa_format: e.target.value as SampleFormat })}
                  >
                    {FORMATS.map((v) => (
                      <option key={v} value={v}>
                        {v}
                      </option>
                    ))}
                  </select>
                  <ActualFormat spec={r.spec} stats={stats[id]} />
                </td>
                <td>
                  <select
                    aria-label={`Period, ${name}`}
                    value={r.spec.frames_per_period}
                    onChange={(e) => setSpec({ frames_per_period: Number(e.target.value) })}
                  >
                    {PERIODS.map((v) => (
                      <option key={v} value={v}>
                        {v}
                      </option>
                    ))}
                  </select>
                </td>
                <td>
                  <select
                    aria-label={`Target latency, ${name}`}
                    value={r.spec.target_latency_ms}
                    onChange={(e) => setSpec({ target_latency_ms: Number(e.target.value) })}
                  >
                    {LATENCIES.map((v) => (
                      <option key={v} value={v}>
                        {v} ms
                      </option>
                    ))}
                  </select>
                </td>
                <td>
                  <input
                    type="checkbox"
                    aria-label={`Forward error correction, ${name}`}
                    checked={r.spec.fec}
                    onChange={(e) => setSpec({ fec: e.target.checked })}
                  />
                </td>
                <td style={{ width: 110 }}>
                  <LevelMeter label="in" db={stats[id]?.capture_level_db ?? null} />
                  <LevelMeter label="out" db={stats[id]?.playback_level_db ?? null} />
                </td>
                <td>
                  {(() => {
                    const lat = summarizeLatency(stats[id]);
                    return (
                      <span title={lat.title} style={lat.partial ? { color: "#f59e0b" } : undefined}>
                        {lat.text}
                      </span>
                    );
                  })()}
                </td>
                <td title={xrunBreakdown(stats[id])}>{stats[id]?.xruns ?? 0}</td>
                <td style={(stats[id]?.clipped_samples ?? 0) > 0 ? { color: "#ef5350" } : undefined}>
                  {stats[id]?.clipped_samples ?? "—"}
                </td>
                <td className="health-cell">
                  <span title={health.title} style={{ color: health.color }}>
                    {health.text}
                  </span>
                </td>
                <td className="actions-cell">
                  <button
                    className="danger-quiet"
                    onClick={() => askRemoveRoute(id)}
                    aria-label={`Remove route ${name}`}
                    title="Remove this route (Del). Asks first."
                  >
                    Remove
                  </button>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </section>
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
  label,
  offset,
  channels,
  deviceWidth,
  onChange,
}: {
  label: string;
  offset: number;
  channels: number;
  deviceWidth: number | null;
  onChange: (start: number) => void;
}) {
  // The window has to fit: a 2-channel route on an 8-channel device can start
  // at 7 at the latest. The current value is always offered even if it no
  // longer fits, so a device that shrank shows what is configured rather than
  // silently displaying a different number.
  const maxStart = deviceWidth === null ? 32 : Math.max(1, deviceWidth - channels + 1);
  const current = offset + 1;
  return (
    <td style={{ whiteSpace: "nowrap" }}>
      <select aria-label={label} value={current} onChange={(e) => onChange(Number(e.target.value))}>
        {range(Math.max(maxStart, current)).map((v) => (
          <option key={v} value={v}>
            {channels > 1 ? `${v}–${v + channels - 1}` : v}
          </option>
        ))}
      </select>
      {/* Nothing when there is no limit to show: "/?" beside a tone read as
          a problem with the route rather than a fact about tones. */}
      {deviceWidth !== null && <span className="of-width">/{deviceWidth}</span>}
    </td>
  );
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
  // Capture-side figures belong to the *device*, not to this route: several
  // routes can read one input, and one late read was late for all of them.
  // Two routes off the same interface showing the same count is one event
  // seen twice, not two events — worth saying, because the natural reading
  // of two identical numbers is that they add up.
  if (stats.capture_xruns != null) parts.push(`${stats.capture_xruns} capture (overrun: input samples lost; counted per input device, so routes sharing one input report the same number)`);
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
      <div
        style={track}
        role="meter"
        aria-label={`${label === "in" ? "Source" : "Destination"} level`}
        aria-valuemin={-60}
        aria-valuemax={0}
        aria-valuenow={Math.round(clamped)}
        title={`${db.toFixed(1)} dBFS`}
      >
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
