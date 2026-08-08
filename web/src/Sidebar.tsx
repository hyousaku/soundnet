import { useStore } from "./store";

export default function Sidebar() {
  const nodes = useStore((s) => s.nodes);
  const ports = useStore((s) => s.ports);
  const self = useStore((s) => s.self);

  return (
    <aside className="sidebar">
      <h2>This node</h2>
      {self ? (
        <NodeSection node={self} ports={ports[self.id] ?? []} />
      ) : (
        <div className="hint">Connecting…</div>
      )}

      <h2>Peers on the LAN</h2>
      {Object.values(nodes)
        .filter((n) => n.id !== self?.id)
        .map((n) => (
          <NodeSection key={n.id} node={n} ports={ports[n.id] ?? []} />
        ))}
      {Object.values(nodes).length <= 1 && (
        <div className="hint">
          No peers discovered yet. Make sure `soundnet-engine` is running on the
          other host and mDNS traffic isn't blocked between the two.
        </div>
      )}
    </aside>
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
