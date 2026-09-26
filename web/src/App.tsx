import { useEffect, useRef, useState } from "react";
import { useStore } from "./store";
import { useUi } from "./ui";
import { askRemoveRoute, sortedRoutes } from "./actions";
import { isTypingTarget } from "./keys";
import Sidebar from "./Sidebar";
import Patchbay from "./Patchbay";
import RouteEditor from "./RouteEditor";
import DialogHost from "./DialogHost";

/// Move keyboard focus to one of the three regions of the page.
///
/// Tab alone is not a practical way around this UI: the patch bay makes every
/// host card and every wire a tab stop, so getting from the sidebar to the
/// route table can take dozens of presses. These are the shortcuts out.
export function focusRegion(region: "sidebar" | "canvas" | "routes"): void {
  if (region === "routes") {
    // Land on something useful — the first control of the selected route's
    // row, or of the first row — rather than on the table as a whole, which
    // would only need another Tab to get anywhere.
    const selected = useUi.getState().selectedRouteId;
    const row =
      (selected && document.querySelector<HTMLElement>(`[data-route-row="${selected}"]`)) ||
      document.querySelector<HTMLElement>("[data-route-row]");
    const control = row?.querySelector<HTMLElement>("select, input, button");
    (control ?? document.getElementById("region-routes"))?.focus();
    return;
  }
  document.getElementById(`region-${region}`)?.focus();
}

export default function App() {
  const connect = useStore((s) => s._connect);
  const connected = useStore((s) => s.connected);
  const self = useStore((s) => s.self);
  const send = useStore((s) => s.send);
  const notices = useStore((s) => s.notices);
  const dismissNotice = useStore((s) => s.dismissNotice);
  const openDialog = useUi((s) => s.openDialog);
  const [rescanning, setRescanning] = useState(false);
  const workRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    connect();
    // No cleanup — the socket lives for the lifetime of the tab.
  }, [connect]);

  // While the engine is unreachable, nothing on the page can do anything:
  // `send` drops messages on a closed socket. Clicks used to vanish without a
  // trace — a Remove that did nothing, a rate change that silently did not
  // happen. `inert` takes the controls out of reach for mouse and keyboard
  // alike, and the banner says why. Set through the DOM because React 18 does
  // not know the attribute.
  useEffect(() => {
    if (workRef.current) workRef.current.inert = !connected;
  }, [connected]);

  const rescan = () => {
    if (!connected || rescanning) return;
    send({ type: "rescan_devices" });
    setRescanning(true);
    // The engine replies with a fresh `state` message almost immediately;
    // this timeout just makes sure the button never gets stuck showing
    // "Rescanning…" if that message gets lost.
    setTimeout(() => setRescanning(false), 1500);
  };

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented || e.ctrlKey || e.metaKey || e.altKey) return;
      const ui = useUi.getState();
      // An open dialog owns the keyboard: it handles Escape and Tab itself,
      // and a letter shortcut firing underneath it would act on a page the
      // operator cannot currently see.
      if (ui.dialog) return;

      if (e.key === "Escape") {
        if (ui.selectedRouteId) {
          ui.selectRoute(null);
          e.preventDefault();
        }
        return;
      }
      if (isTypingTarget(e.target)) return;

      const live = useStore.getState().connected;
      const handled = (() => {
        switch (e.key) {
          case "?":
            ui.openDialog({ kind: "help" });
            return true;
          case "n":
          case "N":
            if (live) ui.openDialog({ kind: "newRoute" });
            return true;
          case "h":
          case "H":
            if (live) ui.openDialog({ kind: "addHost" });
            return true;
          case "r":
          case "R":
            rescan();
            return true;
          case "1":
            focusRegion("sidebar");
            return true;
          case "2":
            focusRegion("canvas");
            return true;
          case "3":
            focusRegion("routes");
            return true;
          case "f":
          case "F":
            ui.fitView?.();
            return true;
          case "j":
          case "k": {
            const list = sortedRoutes(useStore.getState().routes);
            if (list.length === 0) return true;
            const at = list.findIndex((r) => r.id === ui.selectedRouteId);
            const next =
              at < 0
                ? e.key === "j"
                  ? 0
                  : list.length - 1
                : (at + (e.key === "j" ? 1 : -1) + list.length) % list.length;
            ui.selectRoute(list[next].id);
            return true;
          }
          case "Delete":
          case "Backspace":
            // React Flow handles Delete on a selected wire itself (through
            // `onBeforeDelete`, which asks the same question). This covers a
            // route selected from the table whose wire is not on the canvas.
            // Whichever runs first opens the dialog; the other sees it open
            // and stands down, so the operator is only ever asked once.
            if (live && ui.selectedRouteId) askRemoveRoute(ui.selectedRouteId);
            return !!ui.selectedRouteId;
          default:
            return false;
        }
      })();
      if (handled) e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // `rescan` closes over state that changes; re-binding on those is cheap.
  }, [connected, rescanning]);

  return (
    <div className="app">
      <a className="skip-link" href="#region-routes" onClick={(e) => {
        e.preventDefault();
        focusRegion("routes");
      }}>
        Skip to route table
      </a>
      <header className="topbar">
        <h1>SoundNet</h1>
        <span className="status" role="status">
          {connected ? (
            <>
              <span className="ok" aria-hidden>●</span> connected
              {self && <> · {self.hostname} ({self.addr}:{self.port})</>}
            </>
          ) : (
            <>
              <span className="bad" aria-hidden>●</span> disconnected — reconnecting…
            </>
          )}
        </span>
        <div className="toolbar">
          <button
            className="primary"
            onClick={() => openDialog({ kind: "newRoute" })}
            disabled={!connected}
            aria-keyshortcuts="N"
            title="Connect a source to a destination (N). You can also drag between ports in the patch bay."
          >
            New route <kbd>N</kbd>
          </button>
          <button
            onClick={() => openDialog({ kind: "addHost" })}
            disabled={!connected}
            aria-keyshortcuts="H"
            title="Add a machine that auto-discovery cannot see, e.g. over Tailscale (H)"
          >
            Add host <kbd>H</kbd>
          </button>
          <button
            onClick={rescan}
            disabled={!connected || rescanning}
            aria-keyshortcuts="R"
            title="Re-scan local audio devices, e.g. after plugging in a USB interface (R)"
          >
            {rescanning ? "Rescanning…" : "Rescan"} <kbd>R</kbd>
          </button>
          <button
            onClick={() => openDialog({ kind: "help" })}
            aria-keyshortcuts="Shift+?"
            title="Help: every mouse and keyboard way of doing things (?)"
            aria-label="Help"
          >
            ?
          </button>
        </div>
      </header>

      {!connected && (
        <div className="offline-banner" role="alert">
          Not connected to the engine. Nothing can be changed until the
          connection comes back — this page retries on its own.
        </div>
      )}

      <div className="notices" aria-live="polite">
        {notices.map((n, i) => (
          <div key={`${i}-${n}`} className="notice" role="status">
            <span>{n}</span>
            <button onClick={() => dismissNotice(i)} aria-label="Dismiss notice">
              ×
            </button>
          </div>
        ))}
      </div>

      <div className="work" ref={workRef}>
        <Sidebar />
        <main className="main">
          <Patchbay />
          <RouteEditor />
        </main>
      </div>
      <DialogHost />
    </div>
  );
}
