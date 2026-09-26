import { useState } from "react";
import { useStore } from "./store";
import { portKey, splitPortKey, type RoutePreset } from "./ui";
import { defaultSpec, type LocalPort, type Node } from "./protocol";
import { createRoute, describeRoute, routeSharingInput, widthOf } from "./actions";
import Dialog from "./Dialog";

const range = (n: number) => Array.from({ length: Math.max(1, n) }, (_, i) => i + 1);

export default function NewRouteDialog({
  preset,
  onClose,
}: {
  preset?: RoutePreset;
  onClose: () => void;
}) {
  const nodes = useStore((s) => s.nodes);
  const ports = useStore((s) => s.ports);
  const routes = useStore((s) => s.routes);
  const self = useStore((s) => s.self);

  // This machine first, then everyone else by name — the order people look
  // for them in.
  const hosts: Node[] = Object.values(nodes).sort((a, b) =>
    a.id === self?.id ? -1 : b.id === self?.id ? 1 : a.hostname.localeCompare(b.hostname),
  );
  const portsOf = (n: Node, wantPlayback: boolean) =>
    (ports[n.id] ?? []).filter((p) => (p.kind === "playback") === wantPlayback);
  const find = (key: string): { node?: Node; port?: LocalPort } => {
    const [nodeId, portId] = splitPortKey(key);
    return { node: nodes[nodeId], port: (ports[nodeId] ?? []).find((p) => p.id === portId) };
  };

  const firstSource = hosts.flatMap((n) => portsOf(n, false).map((p) => portKey(n.id, p.id)))[0];
  // Prefer an output on a *different* machine: sending to yourself is the
  // exception, and a default that does it looks like a routing mistake.
  const srcNodeOf = (key?: string) => (key ? splitPortKey(key)[0] : undefined);
  // And one nobody is playing into yet: an output takes one route at a time,
  // so a default that is already taken is a route that cannot start.
  const inUse = new Set(Object.values(routes).map((r) => portKey(r.dst.node_id, r.dst.port_id)));
  const pickDest = (srcKey?: string) => {
    const all = hosts.flatMap((n) => portsOf(n, true).map((p) => portKey(n.id, p.id)));
    const remote = (k: string) => srcNodeOf(k) !== srcNodeOf(srcKey);
    return (
      all.find((k) => remote(k) && !inUse.has(k)) ??
      all.find((k) => !inUse.has(k)) ??
      all.find(remote) ??
      all[0]
    );
  };

  // The same preference the other way round, for "route into this output".
  const pickSource = (dstKey?: string) => {
    const all = hosts.flatMap((n) => portsOf(n, false).map((p) => portKey(n.id, p.id)));
    return all.find((k) => srcNodeOf(k) !== srcNodeOf(dstKey)) ?? all[0];
  };

  const [src, setSrc] = useState<string | undefined>(
    () => preset?.src ?? (preset?.dst ? pickSource(preset.dst) : firstSource),
  );
  const [dst, setDst] = useState<string | undefined>(
    () => preset?.dst ?? pickDest(preset?.src ?? firstSource),
  );
  const [channels, setChannels] = useState(defaultSpec().channels);
  const [srcStart, setSrcStart] = useState(1);
  const [dstStart, setDstStart] = useState(1);

  const srcPort = src ? find(src).port : undefined;
  const dstPort = dst ? find(dst).port : undefined;
  const srcWidth = widthOf(srcPort);
  const dstWidth = widthOf(dstPort);

  // Everything below is derived rather than stored, so changing the port
  // can never leave a start channel pointing past the end of the device.
  const maxChannels = Math.min(srcWidth ?? 32, dstWidth ?? 32);
  const ch = Math.min(channels, maxChannels);
  const maxSrcStart = srcWidth === null ? 32 : srcWidth - ch + 1;
  const maxDstStart = dstWidth === null ? 32 : dstWidth - ch + 1;
  const sStart = Math.min(srcStart, maxSrcStart);
  const dStart = Math.min(dstStart, maxDstStart);

  const routeList = Object.values(routes);
  // Another route already reading this input. A capture device is opened
  // once and shared, so the new route must match its rate, period and
  // format or the engine will refuse it — copy them rather than let the
  // operator find that out from an error.
  const sharesInput = src ? routeSharingInput(...splitPortKey(src)) : undefined;
  // Playback is *not* shared: a second route into the same output fails to
  // open it. Say so before the operator makes one, not after.
  const outputTakenBy = dst
    ? routeList.filter((r) => portKey(r.dst.node_id, r.dst.port_id) === dst)
    : [];

  const describe = describeRoute;

  const canCreate = !!src && !!dst;

  const create = () => {
    if (!src || !dst) return;
    const [srcNode, srcPortId] = splitPortKey(src);
    const [dstNode, dstPortId] = splitPortKey(dst);
    createRoute({
      src: { node_id: srcNode, port_id: srcPortId, channel_offset: sStart - 1 },
      dst: { node_id: dstNode, port_id: dstPortId, channel_offset: dStart - 1 },
      channels: ch,
    });
    onClose();
  };

  const portOptions = (wantPlayback: boolean) =>
    hosts.map((n) => {
      const list = portsOf(n, wantPlayback);
      if (list.length === 0) return null;
      return (
        <optgroup key={n.id} label={n.id === self?.id ? `${n.hostname} (this machine)` : n.hostname}>
          {list.map((p) => {
            const w = widthOf(p);
            return (
              <option key={p.id} value={portKey(n.id, p.id)}>
                {p.label}
                {w !== null ? ` — ${w}ch` : ""}
              </option>
            );
          })}
        </optgroup>
      );
    });

  if (!firstSource || !hosts.some((n) => portsOf(n, true).length > 0)) {
    return (
      <Dialog title="New route" onClose={onClose}>
        <p className="dialog-body">
          There is nothing to connect yet: a route needs a source (a capture
          device or test tone) and a destination (a playback device). Rescan devices after plugging an interface in, or
          add the other machine with “Add host” if it has not appeared.
        </p>
        <div className="dialog-actions">
          <button type="button" className="primary" onClick={onClose} data-autofocus>
            Close
          </button>
        </div>
      </Dialog>
    );
  }

  return (
    <Dialog title="New route" onClose={onClose} wide>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          create();
        }}
        // Browsers submit a form on Enter only from a text field, and this
        // one is all menus — without this, finishing from the keyboard meant
        // tabbing down to the button.
        onKeyDown={(e) => {
          if (e.key === "Enter" && e.target instanceof HTMLSelectElement) {
            e.preventDefault();
            create();
          }
        }}
      >
        <label className="field">
          <span>From (source)</span>
          <select
            value={src}
            data-autofocus
            onChange={(e) => {
              setSrc(e.target.value);
              setSrcStart(1);
            }}
          >
            {portOptions(false)}
          </select>
        </label>

        <label className="field">
          <span>To (destination)</span>
          <select
            value={dst}
            onChange={(e) => {
              setDst(e.target.value);
              setDstStart(1);
            }}
          >
            {portOptions(true)}
          </select>
        </label>

        <div className="field-row">
          <label className="field">
            <span>Channels</span>
            <select value={ch} onChange={(e) => setChannels(Number(e.target.value))}>
              {range(maxChannels).map((v) => (
                <option key={v} value={v}>
                  {v}
                </option>
              ))}
            </select>
          </label>
          <label className="field">
            <span>Source starts at ch</span>
            <select value={sStart} onChange={(e) => setSrcStart(Number(e.target.value))}>
              {range(maxSrcStart).map((v) => (
                <option key={v} value={v}>
                  {v}
                  {ch > 1 ? `–${v + ch - 1}` : ""}
                </option>
              ))}
            </select>
          </label>
          <label className="field">
            <span>Destination starts at ch</span>
            <select value={dStart} onChange={(e) => setDstStart(Number(e.target.value))}>
              {range(maxDstStart).map((v) => (
                <option key={v} value={v}>
                  {v}
                  {ch > 1 ? `–${v + ch - 1}` : ""}
                </option>
              ))}
            </select>
          </label>
        </div>

        {sharesInput && (
          <p className="field-note">
            This source already feeds {describe(sharesInput)}. The new route
            copies its rate ({sharesInput.spec.rate / 1000}k), period (
            {sharesInput.spec.frames_per_period}) and format, because a source
            device is opened once and every route on it has to agree on those.
          </p>
        )}
        {outputTakenBy.length > 0 && (
          <p className="field-error" role="alert">
            This destination is already used by {outputTakenBy.map(describe).join(", ")}. A
            destination can only be played into by one route at a time, so this one
            will not be able to start until that route is removed.
          </p>
        )}

        <p className="hint">
          Rate, period, latency and FEC start at the usual defaults and can be
          changed in the route table afterwards.
        </p>

        <div className="dialog-actions">
          <button type="button" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="primary" disabled={!canCreate}>
            Create route
          </button>
        </div>
      </form>
    </Dialog>
  );
}
