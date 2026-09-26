import { useState } from "react";
import { useStore } from "./store";
import Dialog from "./Dialog";

export default function AddHostDialog({ onClose }: { onClose: () => void }) {
  const send = useStore((s) => s.send);
  const [addr, setAddr] = useState("");
  const [port, setPort] = useState("7788");

  const portNum = Number(port);
  const portOk = Number.isInteger(portNum) && portNum >= 1 && portNum <= 65535;
  const addrOk = addr.trim().length > 0;
  const ok = addrOk && portOk;

  const submit = () => {
    if (!ok) return;
    send({ type: "add_manual_host", addr: addr.trim(), port: portNum });
    onClose();
  };

  return (
    <Dialog title="Add host" onClose={onClose}>
      <p className="hint">
        For a peer that auto-discovery cannot see: a different VLAN, a
        firewall, or anything over a VPN such as Tailscale (use its{" "}
        <code>100.x.y.z</code> address). The engine keeps retrying a host
        added here, so it comes back on its own after a network blip.
      </p>
      {/*
        A form so Enter submits from either field. It used to be wired to the
        address box only, so after tabbing to the port Enter did nothing.
      */}
      <form
        onSubmit={(e) => {
          e.preventDefault();
          submit();
        }}
      >
        <div className="field-row">
          <label className="field grow">
            <span>Address</span>
            <input
              type="text"
              placeholder="192.168.1.42, raspi.local or 100.x.y.z"
              value={addr}
              onChange={(e) => setAddr(e.target.value)}
              data-autofocus
              autoComplete="off"
              spellCheck={false}
            />
          </label>
          <label className="field">
            <span>Port</span>
            <input
              type="number"
              min={1}
              max={65535}
              value={port}
              onChange={(e) => setPort(e.target.value)}
              style={{ width: 90 }}
              aria-invalid={!portOk}
            />
          </label>
        </div>
        {!portOk && <p className="field-error">Port must be between 1 and 65535.</p>}
        <div className="dialog-actions">
          <button type="button" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="primary" disabled={!ok}>
            Add host
          </button>
        </div>
      </form>
    </Dialog>
  );
}
