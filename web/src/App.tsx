import { useEffect } from "react";
import { useStore } from "./store";
import Sidebar from "./Sidebar";
import Patchbay from "./Patchbay";
import RouteEditor from "./RouteEditor";

export default function App() {
  const connect = useStore((s) => s._connect);
  const connected = useStore((s) => s.connected);
  const self = useStore((s) => s.self);

  useEffect(() => {
    connect();
    // No cleanup — the socket lives for the lifetime of the tab.
  }, [connect]);

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
      </header>
      <Sidebar />
      <main className="main">
        <Patchbay />
        <RouteEditor />
      </main>
    </div>
  );
}
