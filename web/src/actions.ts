import { useStore } from "./store";
import { confirmAction, useUi } from "./ui";
import { defaultSpec, type LocalPort, type Route } from "./protocol";

/// A route id chosen here rather than by the engine, so the new row can be
/// selected the moment it appears. `crypto.randomUUID` would be the obvious
/// call, but browsers only expose it in secure contexts — and this UI is
/// normally opened as plain http from a LAN address, where it is undefined.
/// `getRandomValues` has no such restriction.
export function newRouteId(): string {
  const b = crypto.getRandomValues(new Uint8Array(16));
  b[6] = (b[6] & 0x0f) | 0x40;
  b[8] = (b[8] & 0x3f) | 0x80;
  const h = Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
}

/// How many channels a port really has, or null when that is not a limit: a
/// tone synthesizes whatever it is asked for, and an unprobed device's count
/// is a placeholder rather than a fact.
export const widthOf = (p?: LocalPort): number | null =>
  p && p.kind !== "tone" && !p.probe_failed ? p.max_channels : null;

/// The route already reading this input, if any. A capture device is opened
/// once and shared, so a new route on it must match that route's rate,
/// period and format or the engine refuses it. Tones are not shared — every
/// route on one gets its own generator — so they never constrain anything.
export function routeSharingInput(nodeId: string, portId: string): Route | undefined {
  const { ports, routes } = useStore.getState();
  const port = (ports[nodeId] ?? []).find((p) => p.id === portId);
  if (port?.kind !== "capture") return undefined;
  return Object.values(routes).find(
    (r) => r.src.node_id === nodeId && r.src.port_id === portId,
  );
}

/// Create a route and select it. Both ways of making one — the New route
/// dialog and dragging a wire in the patch bay — come through here, so they
/// cannot disagree about the defaults.
///
/// Two things are fixed up rather than left for the engine to refuse: the
/// channel count is clamped to what both devices have (the default stereo
/// route into a mono output used to fail on start), and a shared input's
/// rate/period/format are copied (see `routeSharingInput`).
export function createRoute(opts: {
  src: { node_id: string; port_id: string; channel_offset?: number };
  dst: { node_id: string; port_id: string; channel_offset?: number };
  channels?: number;
}): string {
  const { ports, send } = useStore.getState();
  const port = (n: string, p: string) => (ports[n] ?? []).find((x) => x.id === p);
  const base = defaultSpec();
  const widths = [
    widthOf(port(opts.src.node_id, opts.src.port_id)),
    widthOf(port(opts.dst.node_id, opts.dst.port_id)),
  ].filter((w): w is number => w !== null);
  const channels = Math.max(1, Math.min(opts.channels ?? base.channels, ...widths));
  const shared = routeSharingInput(opts.src.node_id, opts.src.port_id);
  const id = newRouteId();
  const route: Route = {
    id,
    src: { ...opts.src, channel_offset: opts.src.channel_offset ?? 0 },
    dst: { ...opts.dst, channel_offset: opts.dst.channel_offset ?? 0 },
    spec: {
      ...base,
      ...(shared && {
        rate: shared.spec.rate,
        frames_per_period: shared.spec.frames_per_period,
        alsa_format: shared.spec.alsa_format,
      }),
      channels,
    },
  };
  send({ type: "add_route", route });
  useUi.getState().selectRoute(id);
  return id;
}

/// "raspi · USB Audio (in) → studio · HDA Intel PCH (out)".
///
/// Hostnames alone are not enough to tell routes apart: two routes between
/// the same pair of machines read identically, and the one being removed is
/// exactly the thing an operator must not get wrong.
export function describeRoute(r: Route): string {
  const { nodes, ports } = useStore.getState();
  const end = (nodeId: string, portId: string) => {
    const host = nodes[nodeId]?.hostname ?? nodeId.slice(0, 8);
    const port = (ports[nodeId] ?? []).find((p) => p.id === portId);
    return port ? `${host} · ${port.label}` : host;
  };
  return `${end(r.src.node_id, r.src.port_id)} → ${end(r.dst.node_id, r.dst.port_id)}`;
}

/// Remove a route, after asking. Every path that removes one — the Remove
/// button, Delete on a selected row, Delete on a selected wire — comes through
/// here, so none of them can skip the question.
///
/// It asks because removal is immediate and live: the audio stops on both
/// machines the moment the engine gets the message, and there is no undo.
/// At an event, a mis-click on a button that sits in the same row as the
/// latency menu is not a hypothetical.
export function askRemoveRoute(routeId: string): void {
  const route = useStore.getState().routes[routeId];
  if (!route) return;
  confirmAction({
    title: "Remove route?",
    body:
      `${describeRoute(route)}\n\n` +
      "Audio on this route stops immediately on both machines.",
    confirmLabel: "Remove route",
    danger: true,
    onConfirm: () => {
      useStore.getState().send({ type: "remove_route", id: routeId });
      const ui = useUi.getState();
      if (ui.selectedRouteId === routeId) ui.selectRoute(null);
    },
  });
}

/// Routes in a stable, meaningful order: by what they connect, then by id.
///
/// The store keeps them in arrival order, and a reconnect rebuilds the map
/// from the engine's snapshot, which comes out of a hash map in whatever
/// order its shards happen to be walked. Rows that reshuffle on every
/// reconnect move the Remove button out from under the pointer — the same
/// problem the port list had before it was sorted.
export function sortedRoutes(routes: Record<string, Route>): Route[] {
  return Object.values(routes)
    .map((r) => ({ r, key: describeRoute(r) }))
    .sort((a, b) => a.key.localeCompare(b.key) || a.r.id.localeCompare(b.r.id))
    .map(({ r }) => r);
}
