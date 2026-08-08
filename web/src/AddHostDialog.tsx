import { useState } from "react";
import { useStore } from "./store";

export default function AddHostDialog({ onClose }: { onClose: () => void }) {
  const send = useStore((s) => s.send);
  const [addr, setAddr] = useState("");
  const [port, setPort] = useState(7788);

  const submit = () => {
    if (!addr.trim()) return;
    send({ type: "add_manual_host", addr: addr.trim(), port });
    onClose();
  };

  return (
    <div className="dialog" onClick={onClose}>
      <div className="box" onClick={(e) => e.stopPropagation()}>
        <h3>Add host manually</h3>
        <p className="hint">
          Use this when mDNS is blocked (different VLAN, VPN, or the peer's
          firewall). Point it at the peer's control port.
        </p>
        <div style={{ display: "flex", gap: 8, marginTop: 12, alignItems: "center" }}>
          <input
            type="text"
            placeholder="192.168.1.42 or raspi.local"
            value={addr}
            onChange={(e) => setAddr(e.target.value)}
            style={{ flex: 1, padding: "6px 8px", background: "#1c222b",
                     color: "#e6e9ef", border: "1px solid #262d38", borderRadius: 4 }}
            onKeyDown={(e) => { if (e.key === "Enter") submit(); }}
            autoFocus
          />
          <input
            type="number"
            min={1}
            max={65535}
            value={port}
            onChange={(e) => setPort(Number(e.target.value))}
            style={{ width: 90 }}
          />
        </div>
        <div style={{ display: "flex", gap: 8, marginTop: 16, justifyContent: "flex-end" }}>
          <button onClick={onClose}>Cancel</button>
          <button onClick={submit} disabled={!addr.trim()}>Add</button>
        </div>
      </div>
    </div>
  );
}
