import { useEffect, useState } from "react";
import { useStore } from "./store";
import Sidebar from "./Sidebar";
import Patchbay from "./Patchbay";
import RouteEditor from "./RouteEditor";
import AddHostDialog from "./AddHostDialog";

export default function App() {
  const connect = useStore((s) => s._connect);
  const connected = useStore((s) => s.connected);
  const self = useStore((s) => s.self);
  const send = useStore((s) => s.send);
  const [showAddHost, setShowAddHost] = useState(false);
  const [rescanning, setRescanning] = useState(false);

  useEffect(() => {
    connect();
    // No cleanup — the socket lives for the lifetime of the tab.
  }, [connect]);

  const rescan = () => {
    send({ type: "rescan_devices" });
    setRescanning(true);
    // The engine replies with a fresh `state` message almost immediately;
    // this timeout just makes sure the button never gets stuck showing
    // "Rescanning…" if that message gets lost.
    setTimeout(() => setRescanning(false), 1500);
  };

  return (
    <div className="app">
      <header className="topbar">
        <h1>SoundNet</h1>
        <span className="status">
          {connected ? (
            <>
              <span className="ok">●</span> connected
              {self && <> · {self.hostname} ({self.addr}:{self.port})</>}
            </>
          ) : (
            <>
              <span className="bad">●</span> disconnected
            </>
          )}
        </span>
        <div style={{ marginLeft: "auto", display: "flex", gap: 8 }}>
          <button onClick={rescan} disabled={!connected || rescanning} title="Re-scan local audio devices (e.g. after plugging in a USB interface)">
            {rescanning ? "Rescanning…" : "Rescan devices"}
          </button>
          <button onClick={() => setShowAddHost(true)}>Add host…</button>
        </div>
      </header>
      <Sidebar />
      <main className="main">
        <Patchbay />
        <RouteEditor />
      </main>
      {showAddHost && <AddHostDialog onClose={() => setShowAddHost(false)} />}
    </div>
  );
}
