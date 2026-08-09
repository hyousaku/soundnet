import { useStore } from "./store";

export default function Sidebar() {
  const nodes = useStore((s) => s.nodes);
  const ports = useStore((s) => s.ports);
  const self = useStore((s) => s.self);
  const manualHosts = useStore((s) => s.manualHosts);
  const send = useStore((s) => s.send);

  const peers = Object.values(nodes).filter((n) => n.id !== self?.id);

  return (
    <aside className="sidebar">
      <h2>This node</h2>
      {self ? (
        <>
          <NodeSection node={self} ports={ports[self.id] ?? []} />
          <InterfacePicker />
        </>
      ) : (
        <div className="hint">Connecting…</div>
      )}

      <h2>Peers on the LAN</h2>
      {peers.map((n) => (
        <NodeSection key={n.id} node={n} ports={ports[n.id] ?? []} />
      ))}
      {peers.length === 0 && (
        <div className="hint">
          No peers discovered yet. Make sure `soundnet-engine` is running on the
          other host and mDNS traffic isn't blocked between the two.
        </div>
      )}

      {manualHosts.length > 0 && (
        <>
          <h2>Manually-added hosts</h2>
          {manualHosts.map((h) => (
            <div key={`${h.addr}:${h.port}`} className="node-card"
                 style={{ display: "flex", justifyContent: "space-between",
                          alignItems: "center", padding: "6px 10px" }}>
              <span style={{ fontSize: 12 }}>{h.addr}:{h.port}</span>
              <button
                onClick={() => send({ type: "remove_manual_host", addr: h.addr, port: h.port })}
                title="Forget this host"
                style={{ padding: "2px 8px", fontSize: 11 }}
              >
                ×
              </button>
            </div>
          ))}
        </>
      )}
    </aside>
  );
}

// Network interface picker for THIS engine only — the browser talks to one
// engine, and that engine can only reconfigure itself, not a peer. Rendered
// solely under "This node" (never inside NodeSection, which is also used for
// peers) so it can't be mistaken for controlling anyone else.
function InterfacePicker() {
  const interfaces = useStore((s) => s.interfaces);
  const selectedInterface = useStore((s) => s.selectedInterface);
  const send = useStore((s) => s.send);

  return (
    <div className="node-card">
      <div className="hint" style={{ marginBottom: 6 }}>
        Egress interface (this node only)
      </div>
      <select
        value={selectedInterface ?? ""}
        onChange={(e) => send({ type: "set_interface", name: e.target.value || null })}
        style={{ width: "100%" }}
      >
        <option value="">Automatic</option>
        {interfaces.map((i) => (
          <option key={i.name} value={i.name}>
            {i.name} — {i.addr}
          </option>
        ))}
      </select>
    </div>
  );
}

function NodeSection({ node, ports }: { node: any; ports: any[] }) {
  return (
    <div className="node-card">
      <div className="name">{node.hostname}</div>
      <div className="addr">{node.addr}:{node.port} · audio :{node.audio_port}</div>
      <div className="ports">
        {ports.length === 0 ? (
          <div className="hint">no ports</div>
        ) : (
          ports.map((p) => (
            <div key={p.id} className="port-row">
              <span className="kind">{p.kind}</span>
              <span title={p.alsa_name}>{p.label}</span>
            </div>
          ))
        )}
      </div>
    </div>
  );
}
