import { useCallback, useEffect, useMemo, useState } from "react";
import {
  ReactFlow,
  Background,
  Controls,
  MiniMap,
  applyNodeChanges,
  type Edge,
  type OnConnect,
  type OnNodesChange,
  MarkerType,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";

import { useStore } from "./store";
import PortNode, { type PortRFNode } from "./PortNode";
import { defaultSpec, type PortRef, type Route } from "./protocol";
import { summarizeLatency } from "./latency";
import { describeHealth } from "./health";

const nodeTypes = { port: PortNode };

// Horizontal spacing between node cards. Cards are 420px wide (see
// PortNode.tsx) — the gap needs to be wide enough for a route's edge label
// ("48k · 2ch · 256f · roc 3.1+pb 2.0ms partial") to render without
// overlapping the cards on either side. This tracks the card width: widening
// the card without widening this would quietly eat the edge label's room.
const NODE_SPACING_X = 660;

export default function Patchbay() {
  const nodes = useStore((s) => s.nodes);
  const ports = useStore((s) => s.ports);
  const routes = useStore((s) => s.routes);
  const stats = useStore((s) => s.stats);
  const send = useStore((s) => s.send);

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
        if (existing) {
          return { ...existing, data };
        }
        const position = { x: 40 + newIndex * NODE_SPACING_X, y: 40 };
        newIndex++;
        return { id: n.id, type: "port" as const, position, data };
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
        labelBgStyle: { fill: "#151920", stroke: "#262d38", strokeWidth: 1 },
        labelBgPadding: [6, 4] as [number, number],
        labelBgBorderRadius: 4,
        labelStyle: { fill: labelColor, fontSize: 11 },
        style: { stroke: color, strokeWidth: 2 },
        markerEnd: { type: MarkerType.ArrowClosed, color },
      };
    });
  }, [routes, stats]);

  const onConnect: OnConnect = useCallback(
    (params) => {
      if (!params.source || !params.target || !params.sourceHandle || !params.targetHandle) {
        return;
      }
      const route: Route = {
        id: "",
        src: {
          node_id: params.source,
          port_id: params.sourceHandle,
          channel_offset: 0,
        } as PortRef,
        dst: {
          node_id: params.target,
          port_id: params.targetHandle,
          channel_offset: 0,
        } as PortRef,
        spec: defaultSpec(),
      };
      send({ type: "add_route", route });
    },
    [send],
  );

  return (
    <div className="canvas">
      <ReactFlow
        nodes={rfNodes}
        edges={rfEdges}
        nodeTypes={nodeTypes}
        onNodesChange={onNodesChange}
        onConnect={onConnect}
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
