import { useStore } from "./store";
import { confirmAction, portKey, useUi } from "./ui";
import type { LocalPort, Node } from "./protocol";

export default function Sidebar() {
  const nodes = useStore((s) => s.nodes);
  const ports = useStore((s) => s.ports);
  const self = useStore((s) => s.self);
  const manualHosts = useStore((s) => s.manualHosts);
  const send = useStore((s) => s.send);
  const openDialog = useUi((s) => s.openDialog);

  const peers = Object.values(nodes)
    .filter((n) => n.id !== self?.id)
    .sort((a, b) => a.hostname.localeCompare(b.hostname));

  const forget = (addr: string, port: number) =>
    confirmAction({
      title: "Forget host?",
      body:
        `${addr}:${port}\n\n` +
        "It stops being contacted. Routes to or from it stop working until it " +
        "is added again, unless auto-discovery can still see it.",
      confirmLabel: "Forget host",
      danger: true,
      onConfirm: () => send({ type: "remove_manual_host", addr, port }),
    });

  return (
    // tabIndex -1: reachable by the "1" shortcut (focusRegion), not a tab stop
    // of its own — the controls inside it are.
    <aside className="sidebar" id="region-sidebar" tabIndex={-1} aria-label="Machines">
      <h2>This machine</h2>
      {self ? (
        <>
          <NodeSection node={self} ports={ports[self.id] ?? []} />
          <InterfacePicker />
        </>
      ) : (
        <div className="hint">Connecting…</div>
      )}

      <h2>Other machines</h2>
      {peers.map((n) => (
        <NodeSection key={n.id} node={n} ports={ports[n.id] ?? []} />
      ))}
      {peers.length === 0 && (
        <div className="hint">
          None found yet. Machines on the same LAN appear by themselves when
          `soundnet-engine` is running on them. Anything else — another
          subnet, or a Tailscale address (100.x.y.z) — needs{" "}
          <button className="linklike" onClick={() => openDialog({ kind: "addHost" })}>
            Add host
          </button>
          .
        </div>
      )}

      {manualHosts.length > 0 && (
        <>
          <h2>Added by hand</h2>
          <ul className="plain-list">
            {manualHosts.map((h) => (
              <li key={`${h.addr}:${h.port}`} className="node-card manual-host">
                <span>
                  {h.addr}:{h.port}
                </span>
                <button
                  onClick={() => forget(h.addr, h.port)}
                  title="Forget this host"
                  aria-label={`Forget host ${h.addr}:${h.port}`}
                  className="danger-quiet"
                >
                  Forget
                </button>
              </li>
            ))}
          </ul>
        </>
      )}
    </aside>
  );
}

// Network interface picker for THIS engine only — the browser talks to one
// engine, and that engine can only reconfigure itself, not a peer. Rendered
// solely under "This machine" (never inside NodeSection, which is also used
// for peers) so it can't be mistaken for controlling anyone else.
function InterfacePicker() {
  const interfaces = useStore((s) => s.interfaces);
  const selectedInterface = useStore((s) => s.selectedInterface);
  const routes = useStore((s) => s.routes);
  const send = useStore((s) => s.send);

  // Switching interface re-announces this machine and restarts every route
  // it takes part in: a short dropout on all of them. Arrowing through a
  // focused <select> fires a change per item, so without this question,
  // looking at the list with the keyboard was enough to cut the audio
  // several times over. The select stays on the current value until the
  // answer is yes — it shows the store, not what was picked.
  const change = (value: string) => {
    const name = value || null;
    if (name === (selectedInterface ?? null)) return;
    const apply = () => send({ type: "set_interface", name });
    const count = Object.keys(routes).length;
    if (count === 0) {
      apply();
      return;
    }
    confirmAction({
      title: "Change network interface?",
      body:
        `Switch to ${name ?? "Automatic"}.\n\n` +
        `The ${count === 1 ? "route" : `${count} routes`} on this machine restart, ` +
        "so audio drops out briefly. Peers must be able to reach the new address.",
      confirmLabel: "Change interface",
      danger: true,
      onConfirm: apply,
    });
  };

  return (
    <div className="node-card">
      <label className="field">
        <span>Network interface (this machine only)</span>
        <select value={selectedInterface ?? ""} onChange={(e) => change(e.target.value)}>
          <option value="">Automatic</option>
          {interfaces.map((i) => (
            <option key={i.name} value={i.name}>
              {i.name} — {i.addr}
            </option>
          ))}
        </select>
      </label>
    </div>
  );
}

/// One machine and its ports. Each port has a button that opens New route
/// already pointing at it — the list-based twin of dragging from its dot in
/// the patch bay, and the way to start a route from a machine whose card is
/// scrolled out of view.
function NodeSection({ node, ports }: { node: Node; ports: LocalPort[] }) {
  const openDialog = useUi((s) => s.openDialog);
  // Sources first, then destinations — the same grouping as the card in the
  // patch bay, so a port is found in the same place in both.
  const sorted = [...ports].sort(
    (a, b) =>
      Number(a.kind === "playback") - Number(b.kind === "playback") ||
      a.alsa_name.localeCompare(b.alsa_name),
  );
  return (
    <section className="node-card" aria-label={node.hostname}>
      <div className="name">{node.hostname}</div>
      <div className="addr">
        {node.addr}:{node.port} · audio :{node.audio_port}
      </div>
      <ul className="ports plain-list">
        {sorted.length === 0 ? (
          <li className="hint">no ports</li>
        ) : (
          sorted.map((p) => {
            const isSource = p.kind !== "playback";
            const key = portKey(node.id, p.id);
            const what = `${node.hostname} · ${p.label}`;
            return (
              <li key={p.id} className="port-row">
                <span className="kind">{isSource ? p.kind : "dest"}</span>
                <span className="port-label" title={p.alsa_name}>
                  {p.label}
                </span>
                <button
                  className="small"
                  onClick={() =>
                    openDialog({
                      kind: "newRoute",
                      preset: isSource ? { src: key } : { dst: key },
                    })
                  }
                  title={isSource ? "New route from this source" : "New route into this destination"}
                  aria-label={isSource ? `New route from ${what}` : `New route to ${what}`}
                >
                  {isSource ? "Send…" : "Play…"}
                </button>
              </li>
            );
          })
        )}
      </ul>
    </section>
  );
}
