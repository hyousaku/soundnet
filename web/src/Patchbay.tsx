import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import {
  ReactFlow,
  Background,
  Controls,
  MiniMap,
  applyNodeChanges,
  type Edge,
  type OnConnect,
  type OnEdgesChange,
  type OnNodesChange,
  type ReactFlowInstance,
  MarkerType,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";

import { useStore } from "./store";
import { useUi } from "./ui";
import PortNode, { type PortRFNode } from "./PortNode";
import { summarizeLatency } from "./latency";
import { describeHealth } from "./health";
import { createRoute, describeRoute } from "./actions";

const nodeTypes = { port: PortNode };

// Horizontal spacing between node cards. Cards are 420px wide (see
// PortNode.tsx) — the gap needs to be wide enough for a route's edge label
// ("48k · 2ch · 256f · roc 3.1+pb 2.0ms partial") to render without
// overlapping the cards on either side. This tracks the card width: widening
// the card without widening this would quietly eat the edge label's room.
const NODE_SPACING_X = 660;

// How far one arrow press pans the canvas, in screen pixels. Large enough
// that crossing a 420px card takes a handful of presses, not dozens.
const PAN_STEP = 80;

export default function Patchbay() {
  const nodes = useStore((s) => s.nodes);
  const ports = useStore((s) => s.ports);
  const routes = useStore((s) => s.routes);
  const stats = useStore((s) => s.stats);
  const selectedRouteId = useUi((s) => s.selectedRouteId);
  const selectRoute = useUi((s) => s.selectRoute);
  const flow = useRef<ReactFlowInstance<PortRFNode, Edge> | null>(null);

  // Node positions are local UI state, not derived from the store, so that
  // dragging a card actually sticks: we only assign a fresh default
  // position the first time a node id appears, and preserve whatever
  // position the user dragged it to on every subsequent store update
  // (peer discovery, rescans, stats ticks, ...).
  const [rfNodes, setRfNodes] = useState<PortRFNode[]>([]);

  useEffect(() => {
    setRfNodes((prev) => {
      const prevById = new Map(prev.map((n) => [n.id, n]));
      let newIndex = 0;
      return Object.values(nodes).map((n) => {
        const existing = prevById.get(n.id);
        const data = { node: n, ports: ports[n.id] ?? [] };
        // A host card stands for a machine on the network, not something
        // this page owns: the only way to make one go away is for the machine
        // to leave. `deletable: false` matters because React Flow deletes a
        // selected node on Backspace, and a stray Backspace used to make a
        // host vanish from the canvas until the page was reloaded.
        const ariaLabel = `${n.hostname} (${n.addr}:${n.port})`;
        if (existing) {
          return { ...existing, data, ariaLabel };
        }
        const position = { x: 40 + newIndex * NODE_SPACING_X, y: 40 };
        newIndex++;
        return { id: n.id, type: "port" as const, position, data, deletable: false, ariaLabel };
      });
    });
  }, [nodes, ports]);

  const onNodesChange: OnNodesChange = useCallback((changes) => {
    setRfNodes((nds) => applyNodeChanges(changes, nds) as PortRFNode[]);
  }, []);

  const rfEdges: Edge[] = useMemo(() => {
    return Object.values(routes).map((r) => {
      const s = stats[r.id];
      const health = describeHealth(s?.health);
      const latSummary = summarizeLatency(s);
      // "—" means no data at all (route has no local role on this engine)
      // — don't clutter every unrelated edge with a dash.
      const lat = latSummary.text !== "—" ? ` · ${latSummary.text}` : "";
      const xr = s && s.xruns > 0 ? ` · xr ${s.xruns}` : "";
      // A route in trouble says so instead of showing its settings: the
      // settings are still on the row in the table below, and an edge that
      // reads "48k · 2ch · 128f" over a device that stopped moving audio is
      // the exact reassurance this was reporting wrongly before.
      const label = health.bad
        ? health.text
        : `${r.spec.rate / 1000}k · ${r.spec.channels}ch · ${r.spec.frames_per_period}f${lat}${xr}`;
      const color = health.bad ? health.color : "#6cf";
      const selected = r.id === selectedRouteId;
      const labelColor = health.bad
        ? health.color
        : latSummary.partial
          ? "#f59e0b"
          : "#e6e9ef";
      return {
        id: r.id,
        source: r.src.node_id,
        sourceHandle: r.src.port_id,
        target: r.dst.node_id,
        targetHandle: r.dst.port_id,
        label,
        labelBgPadding: [6, 4] as [number, number],
        labelBgBorderRadius: 4,
        labelStyle: { fill: labelColor, fontSize: 11 },
        // Selection lives in the UI store, not in React Flow, so that the
        // wire and its row in the table always agree on what is selected.
        selected,
        deletable: false,
        ariaLabel: `Route ${describeRoute(r)}`,
        style: { stroke: color, strokeWidth: selected ? 4 : 2 },
        labelBgStyle: {
          fill: "#151920",
          stroke: selected ? "#e6e9ef" : "#262d38",
          strokeWidth: 1,
        },
        markerEnd: { type: MarkerType.ArrowClosed, color },
      };
    });
    // `nodes`/`ports` feed describeRoute's labels.
  }, [routes, stats, selectedRouteId, nodes, ports]);

  // React Flow reports a click on a wire (or Enter on a focused one) as a
  // selection change. Mirror it into the shared selection; everything that
  // is *not* a wire being selected — clicking the pane, selecting a card —
  // arrives as the selected wire being deselected, and clears it.
  const onEdgesChange: OnEdgesChange = useCallback(
    (changes) => {
      const picked = changes.find((c) => c.type === "select" && c.selected);
      if (picked && picked.type === "select") {
        selectRoute(picked.id);
        return;
      }
      const current = useUi.getState().selectedRouteId;
      if (changes.some((c) => c.type === "select" && !c.selected && c.id === current)) {
        selectRoute(null);
      }
    },
    [selectRoute],
  );

  const onConnect: OnConnect = useCallback((params) => {
    if (!params.source || !params.target || !params.sourceHandle || !params.targetHandle) {
      return;
    }
    createRoute({
      src: { node_id: params.source, port_id: params.sourceHandle },
      dst: { node_id: params.target, port_id: params.targetHandle },
    });
  }, []);

  // Keys for the canvas as a whole, when it (rather than a card or a wire
  // inside it) has focus: React Flow gives cards and wires keyboard handling
  // of their own, but nothing for moving the view, which a mouse does by
  // dragging and scrolling.
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const f = flow.current;
    if (!f || e.target !== e.currentTarget || e.ctrlKey || e.metaKey || e.altKey) return;
    const pan = (dx: number, dy: number) => {
      const v = f.getViewport();
      f.setViewport({ ...v, x: v.x + dx, y: v.y + dy }, { duration: 120 });
    };
    switch (e.key) {
      case "ArrowLeft":
        pan(PAN_STEP, 0);
        break;
      case "ArrowRight":
        pan(-PAN_STEP, 0);
        break;
      case "ArrowUp":
        pan(0, PAN_STEP);
        break;
      case "ArrowDown":
        pan(0, -PAN_STEP);
        break;
      case "+":
      case "=":
        f.zoomIn({ duration: 120 });
        break;
      case "-":
        f.zoomOut({ duration: 120 });
        break;
      case "0":
        f.fitView({ maxZoom: 1, duration: 200 });
        break;
      default:
        return;
    }
    e.preventDefault();
    // The page-level shortcuts would otherwise see "0"/"-" too.
    e.stopPropagation();
  };

  return (
    <div
      className="canvas"
      id="region-canvas"
      tabIndex={0}
      role="application"
      aria-roledescription="patch bay"
      aria-label="Patch bay. Arrow keys pan, plus and minus zoom, 0 fits. Tab moves between host cards and wires."
      onKeyDown={onKeyDown}
    >
      <ReactFlow
        nodes={rfNodes}
        edges={rfEdges}
        nodeTypes={nodeTypes}
        onNodesChange={onNodesChange}
        onEdgesChange={onEdgesChange}
        onEdgeClick={(_, edge) => selectRoute(edge.id)}
        onPaneClick={() => selectRoute(null)}
        onConnect={onConnect}
        onInit={(inst) => {
          flow.current = inst;
          useUi.setState({ fitView: () => void inst.fitView({ maxZoom: 1, duration: 200 }) });
        }}
        // Removing a route asks first, and the page-level Delete handler is
        // what asks (see App.tsx). React Flow's own Delete would bypass the
        // question — and before cards were made undeletable, it removed a
        // host card on a stray Backspace — so it is switched off.
        deleteKeyCode={null}
        // Only one route is ever "the selected one"; shift-selecting several
        // wires would show a selection that Delete and the table ignore.
        multiSelectionKeyCode={null}
        selectionKeyCode={null}
        fitView
        fitViewOptions={{ maxZoom: 1 }}
        minZoom={0.2}
        colorMode="dark"
      >
        <Background color="#262d38" gap={20} />
        <Controls />
        <MiniMap pannable style={{ background: "#0b0d10" }} />
      </ReactFlow>
    </div>
  );
}
